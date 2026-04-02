use crate::{
    BinaryReader, ComponentExternalKind, ComponentValType, FromReader, Result, SectionLimited,
};

/// Represents the type bounds for imports and exports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TypeBounds {
    /// The type is bounded by equality.
    Eq(u32),
    /// A fresh resource type,
    SubResource,
}

impl<'a> FromReader<'a> for TypeBounds {
    fn from_reader(reader: &mut BinaryReader<'a>) -> Result<Self> {
        Ok(match reader.read_u8()? {
            0x00 => TypeBounds::Eq(reader.read()?),
            0x01 => TypeBounds::SubResource,
            x => return reader.invalid_leading_byte(x, "type bound"),
        })
    }
}

/// Represents a reference to a component type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComponentTypeRef {
    /// The reference is to a core module type.
    ///
    /// The index is expected to be core type index to a core module type.
    Module(u32),
    /// The reference is to a function type.
    ///
    /// The index is expected to be a type index to a function type.
    Func(u32),
    /// The reference is to a value type.
    Value(ComponentValType),
    /// The reference is to a bounded type.
    ///
    /// The index is expected to be a type index.
    Type(TypeBounds),
    /// The reference is to an instance type.
    ///
    /// The index is a type index to an instance type.
    Instance(u32),
    /// The reference is to a component type.
    ///
    /// The index is a type index to a component type.
    Component(u32),
}

impl ComponentTypeRef {
    /// Returns the corresponding [`ComponentExternalKind`] for this reference.
    pub fn kind(&self) -> ComponentExternalKind {
        match self {
            ComponentTypeRef::Module(_) => ComponentExternalKind::Module,
            ComponentTypeRef::Func(_) => ComponentExternalKind::Func,
            ComponentTypeRef::Value(_) => ComponentExternalKind::Value,
            ComponentTypeRef::Type(..) => ComponentExternalKind::Type,
            ComponentTypeRef::Instance(_) => ComponentExternalKind::Instance,
            ComponentTypeRef::Component(_) => ComponentExternalKind::Component,
        }
    }
}

impl<'a> FromReader<'a> for ComponentTypeRef {
    fn from_reader(reader: &mut BinaryReader<'a>) -> Result<Self> {
        Ok(match reader.read()? {
            ComponentExternalKind::Module => ComponentTypeRef::Module(reader.read()?),
            ComponentExternalKind::Func => ComponentTypeRef::Func(reader.read_var_u32()?),
            ComponentExternalKind::Value => ComponentTypeRef::Value(reader.read()?),
            ComponentExternalKind::Type => ComponentTypeRef::Type(reader.read()?),
            ComponentExternalKind::Instance => ComponentTypeRef::Instance(reader.read()?),
            ComponentExternalKind::Component => ComponentTypeRef::Component(reader.read()?),
        })
    }
}

/// Represents an import in a WebAssembly component
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct ComponentImport<'a> {
    /// The name of the imported item.
    pub name: ComponentImportName<'a>,
    /// The type reference for the import.
    pub ty: ComponentTypeRef,
}

impl<'a> FromReader<'a> for ComponentImport<'a> {
    fn from_reader(reader: &mut BinaryReader<'a>) -> Result<Self> {
        Ok(ComponentImport {
            name: reader.read()?,
            ty: reader.read()?,
        })
    }
}

/// A reader for the import section of a WebAssembly component.
///
/// # Examples
///
/// ```
/// use wasmparser::{ComponentImportSectionReader, BinaryReader};
/// let data: &[u8] = &[0x01, 0x00, 0x01, 0x41, 0x01, 0x66];
/// let reader = BinaryReader::new(data, 0);
/// let reader = ComponentImportSectionReader::new(reader).unwrap();
/// for import in reader {
///     let import = import.expect("import");
///     println!("Import: {:?}", import);
/// }
/// ```
pub type ComponentImportSectionReader<'a> = SectionLimited<'a, ComponentImport<'a>>;

/// Represents the name of a component import.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct ComponentImportName<'a> {
    /// The import name. When `version_suffix` is present, this is the
    /// canonical interface name (e.g. `ns:pkg/iface@0.2`).
    pub name: &'a str,
    /// An optional semver version suffix that, when concatenated with the
    /// canonical version in `name`, produces the full semver version.
    /// For example if `name` is `ns:pkg/iface@0.2` and `version_suffix`
    /// is `.6`, the full version is `0.2.6`.
    pub version_suffix: Option<&'a str>,
}

/// Tries to parse a canonical interface name by splitting a full semver
/// version into its canonical prefix and remaining suffix.
///
/// The split point is chosen per the component model spec:
/// - If `major > 0`, split after major: `1.2.3` → `1` / `.2.3`
/// - Else if `minor > 0`, split after minor: `0.2.6-rc.1` → `0.2` / `.6-rc.1`
/// - Else, split after patch: `0.0.1-alpha` → `0.0.1` / `-alpha`
///
/// Returns `None` if the name has no `@`, or the version can't be parsed,
pub(crate) fn try_parse_canonical_name(name: &str) -> Option<(&str, &str)> {
    let at = name.rfind('@')?;
    let version_str = &name[at + 1..];
    let version = semver::Version::parse(version_str).ok()?;

    // Determine how many version components form the canonical prefix
    // by counting characters in the original string rather than allocating.
    let canon_len = if version.major > 0 {
        digit_count(version.major)
    } else if version.minor > 0 {
        // "0." + minor digits
        2 + digit_count(version.minor)
    } else {
        // "0.0." + patch digits
        4 + digit_count(version.patch)
    };

    let split = at + 1 + canon_len;
    let suffix = &name[split..];
    Some((&name[..split], suffix))
}

fn digit_count(n: u64) -> usize {
    if n == 0 {
        return 1;
    }
    let mut count = 0;
    let mut n = n;
    while n > 0 {
        count += 1;
        n /= 10;
    }
    count
}

impl<'a> FromReader<'a> for ComponentImportName<'a> {
    fn from_reader(reader: &mut BinaryReader<'a>) -> Result<Self> {
        #[cfg(feature = "features")]
        let parse_canonical_name = reader.features().cm_canonical_interface_names();
        #[cfg(not(feature = "features"))]
        let parse_canonical_name = false;
        let prefix = reader.read_u8()?;
        if !matches!(prefix, 0x00 | 0x01) {
            return reader.invalid_leading_byte(prefix, "import name");
        }
        let name = reader.read_string()?;
        if parse_canonical_name {
            match prefix {
                0x00 => {
                    if let Some((name, version_suffix)) = try_parse_canonical_name(name) {
                        return Ok(ComponentImportName {
                            name,
                            version_suffix: Some(version_suffix),
                        });
                    }
                    return Ok(ComponentImportName {
                        name,
                        version_suffix: None,
                    });
                }
                0x01 => {
                    let version_suffix = match reader.read_string() {
                        Ok(s) => s,
                        Err(_) => return reader.invalid_leading_byte(prefix, "missing import version suffix"),
                    };
                    return Ok(ComponentImportName {
                        name,
                        version_suffix: Some(version_suffix),
                    });
                }
                _ => unreachable!(),
            }
        } else {
            return Ok(ComponentImportName {
                name,
                version_suffix: None,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_parse_canonical_name_test() {
        let result = try_parse_canonical_name("ns:pkg/iface@1.2.3");
        assert_eq!(result, Some(("ns:pkg/iface@1", ".2.3")));
        let result = try_parse_canonical_name("ns:pkg/iface@2.0.0");
        assert_eq!(result, Some(("ns:pkg/iface@2", ".0.0")));
        let result = try_parse_canonical_name("ns:pkg/iface@1.0.0-beta.1");
        assert_eq!(result, Some(("ns:pkg/iface@1", ".0.0-beta.1")));
        let result = try_parse_canonical_name("ns:pkg/iface@0.2.6");
        assert_eq!(result, Some(("ns:pkg/iface@0.2", ".6")));
        let result = try_parse_canonical_name("ns:pkg/iface@0.10.6-rc.1");
        assert_eq!(result, Some(("ns:pkg/iface@0.10", ".6-rc.1")));
        let result = try_parse_canonical_name("ns:pkg/iface@0.0.1-alpha");
        assert_eq!(result, Some(("ns:pkg/iface@0.0.1", "-alpha")));
        let result = try_parse_canonical_name("ns:pkg/iface@0.0.1");
        assert_eq!(result, Some(("ns:pkg/iface@0.0.1", "")));
        let result = try_parse_canonical_name("ns:pkg/iface@0.0.0+build");
        assert_eq!(result, Some(("ns:pkg/iface@0.0.0", "+build")));

        assert_eq!(try_parse_canonical_name("ns:pkg/iface"), None);
        assert_eq!(try_parse_canonical_name("ns:pkg/iface@notaversion"), None);
        assert_eq!(try_parse_canonical_name("ns:pkg/iface@1.2"), None);
    }
}
