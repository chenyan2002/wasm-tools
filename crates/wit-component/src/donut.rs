//! Support for "donut wrapping" a guest component.
//!
//! A *donut* is a core module that implements (virtualizes) some of the
//! imports of a *guest* component, embeds and instantiates that guest, and
//! calls the guest's exports from its own code. See the "Virtualization and
//! donut wrapping" section of the component model explainer.
//!
//! The donut's core module is produced by a bindings generator from two
//! worlds:
//!
//! * the ordinary component world of the donut (its `component-type`
//!   sections), whose exports include the virtualized guest imports, and
//! * the guest's exports, imported through core modules prefixed with
//!   [`GUEST_EXPORT_MODULE_PREFIX`]. These calls are satisfied by lowering the
//!   embedded guest instance's exports rather than by component imports.
//!
//! Which guest imports are virtualized is described by a manifest custom
//! section, [`MANIFEST_SECTION_NAME`], created with [`encode_manifest`]. When
//! [`ComponentEncoder::module`](crate::ComponentEncoder::module) finds this
//! section it switches the encoder into donut mode, which then also requires
//! the guest component via
//! [`ComponentEncoder::donut_guest`](crate::ComponentEncoder::donut_guest).

use crate::StringEncoding;
use crate::metadata::Bindgen;
use anyhow::{Context, Result, bail};
use indexmap::IndexSet;
use std::borrow::Cow;
use wasm_encoder::Encode;
use wasmparser::{BinaryReader, Encoding, Parser, Payload};
use wit_parser::{Resolve, World, WorldId, WorldItem};

/// Name of the custom section holding the donut manifest.
pub const MANIFEST_SECTION_NAME: &str = "wit-bindgen-donut:v0";

/// Prefix of core import modules whose imports call the guest's exports.
///
/// For example `"[guest-export]a:b/c" "f"` calls function `f` of the guest's
/// export `a:b/c`, and `"[guest-export]a:b/c" "[resource-drop]r"` drops a
/// resource `r` defined by the guest.
pub const GUEST_EXPORT_MODULE_PREFIX: &str = "[guest-export]";

/// Prefix shared by all versions of the manifest section name.
const MANIFEST_SECTION_PREFIX: &str = "wit-bindgen-donut";

/// Version byte at the start of the manifest payload.
const MANIFEST_VERSION: u8 = 0;

/// Encodes the payload of a [`MANIFEST_SECTION_NAME`] custom section.
///
/// * `guest` is the world of the guest component.
/// * `virtualized` lists the guest imports, by their world key name (e.g.
///   `"a:b/c"`), that the donut implements. These must be exported by the
///   donut's world. Every other guest import is passed through from the
///   donut's own imports.
///
/// Layout: `version: u8`, the guest world as encoded by
/// [`metadata::encode`](crate::metadata::encode) (length-prefixed), and the
/// virtualized names as a vector of strings.
pub fn encode_manifest(
    resolve: &Resolve,
    guest: WorldId,
    virtualized: &[String],
) -> Result<Vec<u8>> {
    let world = &resolve.worlds[guest];
    for name in virtualized {
        let found = world.imports.iter().any(|(key, item)| {
            matches!(item, WorldItem::Interface { .. }) && resolve.name_world_key(key) == *name
        });
        if !found {
            bail!("virtualized interface `{name}` is not an interface imported by the guest world");
        }
    }
    let encoded_world = crate::metadata::encode(resolve, guest, StringEncoding::UTF8, None, false)?;
    let mut out = vec![MANIFEST_VERSION];
    encoded_world.encode(&mut out);
    virtualized.len().encode(&mut out);
    for name in virtualized {
        name.as_str().encode(&mut out);
    }
    Ok(out)
}

/// A decoded manifest.
pub(crate) struct Manifest {
    guest: Bindgen,
    virtualized: Vec<String>,
}

fn decode_manifest(data: &[u8]) -> Result<Manifest> {
    let mut reader = BinaryReader::new(data, 0);
    let version = reader.read_u8()?;
    if version != MANIFEST_VERSION {
        bail!("unsupported donut manifest version {version}");
    }
    let len = reader.read_var_u32()? as usize;
    let world = reader.read_bytes(len)?;
    let guest = Bindgen::decode_custom_section(world).context("decoding the guest world")?;
    let count = reader.read_var_u32()?;
    let mut virtualized = Vec::new();
    for _ in 0..count {
        virtualized.push(reader.read_string()?.to_string());
    }
    if !reader.eof() {
        bail!("trailing bytes in donut manifest");
    }
    Ok(Manifest { guest, virtualized })
}

/// Finds and removes the donut manifest section from `wasm`, if present.
pub(crate) fn extract_manifest(wasm: &[u8]) -> Result<(Cow<'_, [u8]>, Option<Manifest>)> {
    let mut manifest = None;
    let mut new_module = wasm_encoder::Module::new();
    for payload in Parser::new(0).parse_all(wasm) {
        let payload = payload.context("decoding item in module")?;
        match payload {
            Payload::Version { encoding, .. } if encoding != Encoding::Module => {
                return Ok((Cow::Borrowed(wasm), None));
            }
            Payload::CustomSection(cs) if cs.name().starts_with(MANIFEST_SECTION_PREFIX) => {
                if cs.name() != MANIFEST_SECTION_NAME {
                    bail!(
                        "unsupported donut manifest section `{}`, expected `{MANIFEST_SECTION_NAME}`",
                        cs.name()
                    );
                }
                if manifest.is_some() {
                    bail!("multiple `{MANIFEST_SECTION_NAME}` sections found");
                }
                manifest = Some(
                    decode_manifest(cs.data())
                        .with_context(|| format!("decoding custom section `{}`", cs.name()))?,
                );
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
    match manifest {
        Some(m) => Ok((Cow::Owned(new_module.finish()), Some(m))),
        None => Ok((Cow::Borrowed(wasm), None)),
    }
}

/// Donut-mode state of a [`ComponentEncoder`](crate::ComponentEncoder).
pub(crate) struct Donut {
    /// The guest's world, merged into the encoder's `Resolve`.
    pub guest_world: WorldId,
    /// "World B": the guest's exports, as imports. Classification and ABI
    /// details of `[guest-export]` core imports are derived from this world.
    pub guest_exports_world: WorldId,
    /// Names of the guest imports that the donut virtualizes.
    pub virtualized: IndexSet<String>,
}

impl Donut {
    /// Merges the manifest's guest world into `resolve` and builds World B.
    pub fn new(resolve: &mut Resolve, manifest: Manifest) -> Result<Donut> {
        let Manifest { guest, virtualized } = manifest;
        let remap = resolve
            .merge(guest.resolve)
            .context("failed to merge the guest world into the donut's world")?;
        let guest_world = remap.worlds[guest.world.index()].unwrap();

        // World B is a copy of the guest world importized: the guest's exports
        // become imports, and their type dependencies are elaborated.
        let world_b = World {
            name: format!("{}-donut-guest-exports", resolve.worlds[guest_world].name),
            ..resolve.worlds[guest_world].clone()
        };
        let name = world_b.name.clone();
        let pkg = world_b.package.unwrap();
        let guest_exports_world = resolve.worlds.alloc(world_b);
        resolve.packages[pkg]
            .worlds
            .insert(name.clone(), guest_exports_world);
        resolve.importize(guest_exports_world, Some(name))?;

        Ok(Donut {
            guest_world,
            guest_exports_world,
            virtualized: virtualized.into_iter().collect(),
        })
    }
}
