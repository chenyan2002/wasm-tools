//! Support for "donut wrapping" guest components, in *hole mode*.
//!
//! A *donut* is a core module that implements (virtualizes) some of the
//! imports of one or more *child* components, and calls the children's
//! exports from its own code. The component encoded from it has a *hole* for
//! each child: a component-typed import, `(import "<child>" (component G))`,
//! where `G` is the child's world. The donut instantiates each child itself,
//! with its virtual interfaces and its passthrough imports, once the main
//! module is instantiated. Plugging an actual component into a hole is
//! ordinary composition (e.g. `wac plug`).
//!
//! Each child is described by a manifest custom section,
//! [`MANIFEST_SECTION_PREFIX`]`<child>`, whose presence switches the encoder
//! into hole mode. It names, for the child:
//!
//! * `G`, its world, which is the hole's exact type;
//! * for each virtualized import of `G`, the export of the donut's world that
//!   implements it (a renamed copy, `virtual-<child>:<ns>-<pkg>/…`); every
//!   other import of `G` is passed through from the donut's own imports;
//! * for each export of `G`, the interface the core module imports to call
//!   it (a renamed copy, `guest-<child>:<ns>-<pkg>/…`);
//! * which exports of `G` the donut re-exports unchanged.
//!
//! The bindings generator describes the `guest-<child>` copies as imports in
//! the donut's `component-type` metadata. They aren't host imports: the
//! encoder removes every import in namespace `guest-<child>` from the merged
//! world, and satisfies the core imports from those modules by lowering the
//! child instance's exports instead.
//!
//! Children are instantiated in the order of their manifest sections, each
//! linked (its exports patched into the main module's table) before the next
//! is instantiated.

use crate::metadata::Bindgen;
use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use std::borrow::Cow;
use std::collections::HashSet;
use wasmparser::{BinaryReader, Encoding, Parser, Payload};
use wit_parser::{InterfaceId, Resolve, World, WorldId, WorldItem, WorldKey};

/// Prefix of the manifest sections' names: `wit-bindgen-child:<child>`.
pub const MANIFEST_SECTION_PREFIX: &str = "wit-bindgen-child:";

/// Version byte at the start of a manifest's payload.
const MANIFEST_VERSION: u8 = 1;

/// Namespace prefix of the copies of a child's exports (and of what they
/// use) that the core module imports: `guest-<child>`.
pub const GUEST_NAMESPACE_PREFIX: &str = "guest-";

/// A decoded manifest.
pub(crate) struct Manifest {
    name: String,
    /// `G`'s encoding, to merge identical worlds once.
    guest_bytes: Vec<u8>,
    guest: Bindgen,
    virtualized: Vec<(String, String)>,
    exports: Vec<(String, String)>,
    export_passthrough: Vec<String>,
}

/// Decodes a manifest's payload: `version: u8`, `name: string`,
/// `guest: vec<u8>` (the world `G`, as encoded by
/// [`metadata::encode`](crate::metadata::encode)),
/// `virtualized: vec<(string, string)>`, `exports: vec<(string, string)>`,
/// and `export_passthrough: vec<string>`.
fn decode_manifest(data: &[u8]) -> Result<Manifest> {
    let mut reader = BinaryReader::new(data, 0);
    let version = reader.read_u8()?;
    if version != MANIFEST_VERSION {
        bail!("unsupported child manifest version {version}, expected {MANIFEST_VERSION}");
    }
    let name = reader.read_string()?.to_string();
    let len = reader.read_var_u32()? as usize;
    let world = reader.read_bytes(len)?;
    let guest = Bindgen::decode_custom_section(world).context("decoding the child's world")?;
    let guest_bytes = world.to_vec();
    let mut pairs = || -> Result<Vec<(String, String)>> {
        let count = reader.read_var_u32()?;
        (0..count)
            .map(|_| {
                Ok((
                    reader.read_string()?.to_string(),
                    reader.read_string()?.to_string(),
                ))
            })
            .collect()
    };
    let virtualized = pairs()?;
    let exports = pairs()?;
    let count = reader.read_var_u32()?;
    let export_passthrough = (0..count)
        .map(|_| Ok(reader.read_string()?.to_string()))
        .collect::<Result<_>>()?;
    if !reader.eof() {
        bail!("trailing bytes in child manifest");
    }
    Ok(Manifest {
        name,
        guest_bytes,
        guest,
        virtualized,
        exports,
        export_passthrough,
    })
}

/// Finds and removes the child manifest sections from `wasm`.
pub(crate) fn extract_manifests(wasm: &[u8]) -> Result<(Cow<'_, [u8]>, Vec<Manifest>)> {
    let mut manifests: Vec<Manifest> = Vec::new();
    let mut new_module = wasm_encoder::Module::new();
    for payload in Parser::new(0).parse_all(wasm) {
        let payload = payload.context("decoding item in module")?;
        match payload {
            Payload::Version { encoding, .. } if encoding != Encoding::Module => {
                return Ok((Cow::Borrowed(wasm), Vec::new()));
            }
            Payload::CustomSection(cs) if cs.name().starts_with(MANIFEST_SECTION_PREFIX) => {
                let manifest = decode_manifest(cs.data())
                    .with_context(|| format!("decoding custom section `{}`", cs.name()))?;
                let expected = format!("{MANIFEST_SECTION_PREFIX}{}", manifest.name);
                if cs.name() != expected {
                    bail!(
                        "custom section `{}` holds the manifest of child `{}`, expected it in `{expected}`",
                        cs.name(),
                        manifest.name
                    );
                }
                if manifests.iter().any(|m| m.name == manifest.name) {
                    bail!("two children are named `{}`", manifest.name);
                }
                manifests.push(manifest);
            }
            _ => {
                if let Some((id, range)) = payload.as_section() {
                    new_module.section(&wasm_encoder::RawSection {
                        id,
                        data: &wasm[range.start as usize..range.end as usize],
                    });
                }
            }
        }
    }
    if manifests.is_empty() {
        Ok((Cow::Borrowed(wasm), manifests))
    } else {
        Ok((Cow::Owned(new_module.finish()), manifests))
    }
}

/// Hole-mode state of a [`ComponentEncoder`](crate::ComponentEncoder): its
/// children, in the order they're instantiated.
#[derive(Default)]
pub(crate) struct Donut {
    pub children: Vec<Child>,
}

/// One child.
pub(crate) struct Child {
    /// The child's name: the hole's import name.
    pub name: String,
    /// `G`, merged into the encoder's `Resolve`.
    pub guest_world: WorldId,
    /// `G`'s exports, as imports. The ABI of the core imports that call the
    /// child is derived from this world.
    pub guest_exports_world: WorldId,
    /// Virtualized imports of `G`: its import name, and the name of the
    /// donut world's export that implements it.
    pub virtualized: IndexMap<String, String>,
    /// The core import modules that call the child (`guest-<child>:…`), and
    /// the export of `G` each calls.
    pub exports: IndexMap<String, String>,
    /// Exports of `G` the donut re-exports.
    pub export_passthrough: Vec<String>,
}

impl Donut {
    /// Merges each manifest's `G` into `resolve`, then fixes up `world`, the
    /// donut's merged world: removes the children's `guest-<child>` imports,
    /// and adds `G`'s passthrough imports that it lacks.
    pub fn new(resolve: &mut Resolve, world: WorldId, manifests: Vec<Manifest>) -> Result<Donut> {
        let mut donut = Donut::default();
        // Children of the same world merge it once: merging binds it to the
        // interfaces already in `resolve`, which may elaborate it differently
        // (e.g. a newer patch of a WASI interface with more dependencies), so
        // a second copy of the original wouldn't merge into the first.
        let mut merged: Vec<(Vec<u8>, WorldId)> = Vec::new();
        for manifest in manifests {
            let Manifest {
                name,
                guest_bytes,
                guest,
                virtualized,
                exports,
                export_passthrough,
            } = manifest;
            let guest_world = match merged.iter().find(|(b, _)| *b == guest_bytes) {
                Some((_, world)) => *world,
                None => {
                    let remap = resolve
                        .merge(guest.resolve)
                        .with_context(|| format!("failed to merge child `{name}`'s world"))?;
                    let world = remap.worlds[guest.world.index()].unwrap();
                    merged.push((guest_bytes, world));
                    world
                }
            };

            // `G` importized: its exports become imports, with their type
            // dependencies elaborated.
            let exports_world = World {
                name: format!("{}-child-{name}-exports", resolve.worlds[guest_world].name),
                ..resolve.worlds[guest_world].clone()
            };
            let world_name = exports_world.name.clone();
            let pkg = exports_world.package.unwrap();
            let guest_exports_world = resolve.worlds.alloc(exports_world);
            resolve.packages[pkg]
                .worlds
                .insert(world_name.clone(), guest_exports_world);
            resolve.importize(guest_exports_world, Some(world_name))?;

            let g = &resolve.worlds[guest_world];
            let imports_g = |n: &str| g.imports.keys().any(|k| resolve.name_world_key(k) == n);
            let exports_g = |n: &str| g.exports.keys().any(|k| resolve.name_world_key(k) == n);
            for (guest_name, _) in virtualized.iter() {
                if !imports_g(guest_name) {
                    bail!(
                        "child `{name}`'s virtualized `{guest_name}` isn't an import of its world"
                    );
                }
            }
            for export in exports
                .iter()
                .map(|(e, _)| e)
                .chain(export_passthrough.iter())
            {
                if !exports_g(export) {
                    bail!("child `{name}`'s `{export}` isn't an export of its world");
                }
            }

            donut.children.push(Child {
                name,
                guest_world,
                guest_exports_world,
                virtualized: virtualized.into_iter().collect(),
                exports: exports.into_iter().map(|(g, m)| (m, g)).collect(),
                export_passthrough,
            });
        }

        donut.remove_guest_imports(resolve, world);
        donut.add_passthrough_imports(resolve, world)?;
        Ok(donut)
    }

    /// Removes the imports in the children's `guest-<child>` namespaces: the
    /// children's exports (and types-only copies of what they use), which the
    /// bindings generator describes as imports.
    fn remove_guest_imports(&self, resolve: &mut Resolve, world: WorldId) {
        let namespaces = self
            .children
            .iter()
            .map(|c| format!("{GUEST_NAMESPACE_PREFIX}{}", c.name))
            .collect::<HashSet<_>>();
        let packages = &resolve.packages;
        let interfaces = &resolve.interfaces;
        resolve.worlds[world].imports.retain(|_, item| match item {
            WorldItem::Interface { id, .. } => interfaces[*id]
                .package
                .is_none_or(|p| !namespaces.contains(&packages[p].name.namespace)),
            _ => true,
        });
    }

    /// Adds every passthrough import of every `G` (and what it depends on)
    /// that `world` doesn't import a semver-compatible version of. The
    /// bindings generator only imports the passthrough interfaces its own
    /// code uses, but the donut has to pass all of them to the child.
    fn add_passthrough_imports(&self, resolve: &mut Resolve, world: WorldId) -> Result<()> {
        for child in self.children.iter() {
            let imports = resolve.worlds[child.guest_world]
                .imports
                .iter()
                .filter(|(k, _)| !child.virtualized.contains_key(&resolve.name_world_key(k)))
                .filter_map(|(_, item)| match item {
                    WorldItem::Interface { id, .. } => Some(*id),
                    _ => None,
                })
                .collect::<Vec<_>>();
            for id in imports {
                add_import(resolve, world, id);
            }
        }
        return Ok(());

        fn add_import(resolve: &mut Resolve, world: WorldId, id: InterfaceId) {
            if passthrough_import(resolve, world, id).is_some() {
                return;
            }
            for dep in resolve.interface_direct_deps(id).collect::<Vec<_>>() {
                add_import(resolve, world, dep);
            }
            resolve.worlds[world].imports.insert(
                WorldKey::Interface(id),
                WorldItem::Interface {
                    id,
                    stability: Default::default(),
                    external_id: None,
                    docs: Default::default(),
                    span: Default::default(),
                },
            );
        }
    }

    /// The child whose exports the core import module `module` calls, and
    /// the export of `G` it calls.
    pub fn child_export(&self, module: &str) -> Option<(usize, &str)> {
        self.children
            .iter()
            .enumerate()
            .find_map(|(i, c)| c.exports.get(module).map(|e| (i, e.as_str())))
    }
}

/// The interface `world` imports to pass `id`, an import of a child's world,
/// through: `id` itself, or a semver-compatible version of it.
pub(crate) fn passthrough_import(
    resolve: &Resolve,
    world: WorldId,
    id: InterfaceId,
) -> Option<InterfaceId> {
    let wanted = resolve.canonicalized_id_of(id);
    resolve.worlds[world]
        .imports
        .values()
        .find_map(|item| match item {
            WorldItem::Interface { id: other, .. }
                if *other == id
                    || (wanted.is_some() && resolve.canonicalized_id_of(*other) == wanted) =>
            {
                Some(*other)
            }
            _ => None,
        })
}
