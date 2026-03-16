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

impl<'a> FromReader<'a> for ComponentImportName<'a> {
    fn from_reader(reader: &mut BinaryReader<'a>) -> Result<Self> {
        let use_canonical_name = match reader.read_u8()? {
            0x00 => false,
            0x01 => {
                #[cfg(feature = "features")]
                {
                    reader.features().cm_canonical_interface_names()
                }
                #[cfg(not(feature = "features"))]
                false
            }
            x => return reader.invalid_leading_byte(x, "import name"),
        };
        let name = reader.read_string()?;
        if use_canonical_name {
            let version_suffix = reader.read_string()?;
            Ok(ComponentImportName {
                name,
                version_suffix: Some(version_suffix),
            })
        } else {
            // TODO: try to parse version_suffix
            Ok(ComponentImportName {
                name,
                version_suffix: None,
            })
        }
    }
}
