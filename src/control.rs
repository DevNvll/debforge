//! Parsing and normalization of Debian binary package control metadata.
//!
//! Debian control files use the Deb822 format.  This module keeps continuation
//! lines intact so that callers can distinguish the short and long parts of a
//! description.  Other folded fields can be read with
//! [`DebianControl::get_unfolded`].

use std::collections::BTreeMap;
use std::fmt::{self, Display, Formatter};

use crate::error::{AppError, Context, OptionContext, Result};

/// One parsed Deb822 paragraph from a binary package `control` file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebianControl {
    fields: BTreeMap<String, String>,
}

impl DebianControl {
    /// Parse one Deb822 paragraph.
    ///
    /// Field names are stored in lower case and are matched without regard to
    /// case.  Exactly one paragraph is accepted because a binary package has
    /// exactly one control record.
    pub fn parse(input: &str) -> Result<Self> {
        if input.contains('\0') {
            return Err(AppError::new(
                "the Debian control record contains a NUL byte",
            ));
        }

        let mut fields = BTreeMap::new();
        let mut current: Option<(String, String)> = None;
        let mut paragraph_finished = false;

        for (line_index, line) in input.lines().enumerate() {
            let line_number = line_index + 1;

            if line.is_empty() {
                finish_field(&mut fields, current.take())?;
                if !fields.is_empty() {
                    paragraph_finished = true;
                }
                continue;
            }

            if line.starts_with([' ', '\t']) {
                if paragraph_finished {
                    return Err(AppError::new(format!(
                        "unexpected continuation line after the control paragraph at line {line_number}"
                    )));
                }
                let (_, value) = current.as_mut().context(format!(
                    "control continuation line {line_number} has no field"
                ))?;
                value.push('\n');
                // Deb822 removes the first space or tab.  Any additional space
                // is data and can be important in a preformatted description.
                value.push_str(line[1..].trim_end_matches([' ', '\t']));
                continue;
            }

            if paragraph_finished {
                return Err(AppError::new(format!(
                    "the binary package control file has more than one paragraph (line {line_number})"
                )));
            }

            finish_field(&mut fields, current.take())?;
            let (name, value) = line.split_once(':').context(format!(
                "control line {line_number} does not contain a field separator"
            ))?;
            validate_field_name(name, line_number)?;
            let key = name.to_ascii_lowercase();
            if fields.contains_key(&key) {
                return Err(AppError::new(format!(
                    "control field '{name}' occurs more than once"
                )));
            }
            current = Some((
                key,
                value
                    .trim_start_matches([' ', '\t'])
                    .trim_end_matches([' ', '\t'])
                    .to_owned(),
            ));
        }

        finish_field(&mut fields, current)?;
        if fields.is_empty() {
            return Err(AppError::new("the Debian control record is empty"));
        }

        Ok(Self { fields })
    }

    /// Get a field value.  Field-name matching is case-insensitive.
    ///
    /// A newline separates each physical continuation line.  Use
    /// [`Self::get_unfolded`] for fields where line boundaries have no meaning.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.fields
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// Get a field and replace folded line boundaries with one space.
    pub fn get_unfolded(&self, name: &str) -> Option<String> {
        self.get(name).map(unfold_field)
    }

    /// Iterate over normalized field names and their values.
    pub fn fields(&self) -> impl Iterator<Item = (&str, &str)> {
        self.fields
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    /// Parse the `Description` field into its short and long parts.
    pub fn description(&self) -> DebianDescription {
        DebianDescription::parse(self.get("Description"))
    }
}

fn finish_field(
    fields: &mut BTreeMap<String, String>,
    current: Option<(String, String)>,
) -> Result<()> {
    if let Some((name, value)) = current {
        if fields.insert(name.clone(), value).is_some() {
            return Err(AppError::new(format!(
                "control field '{name}' occurs more than once"
            )));
        }
    }
    Ok(())
}

fn validate_field_name(name: &str, line_number: usize) -> Result<()> {
    let valid = !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && name.as_bytes()[0].is_ascii_alphanumeric();
    if valid {
        Ok(())
    } else {
        Err(AppError::new(format!(
            "invalid control field name '{name}' at line {line_number}"
        )))
    }
}

fn unfold_field(value: &str) -> String {
    value
        .split('\n')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The two semantic parts of a Debian package description.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DebianDescription {
    /// The short description from the first physical field line.
    pub summary: Option<String>,
    /// The optional extended description, with paragraph breaks as newlines.
    pub long: Option<String>,
}

impl DebianDescription {
    fn parse(value: Option<&str>) -> Self {
        let Some(value) = value else {
            return Self::default();
        };
        let mut lines = value.split('\n');
        let summary = lines
            .next()
            .map(str::trim)
            .filter(|line| !line.is_empty() && *line != ".")
            .map(str::to_owned);

        let long = lines
            .map(|line| if line.trim() == "." { "" } else { line })
            .collect::<Vec<_>>()
            .join("\n")
            .trim_matches('\n')
            .to_owned();

        Self {
            summary,
            long: (!long.is_empty()).then_some(long),
        }
    }
}

/// A Debian relationship field that can contain package relations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RelationField {
    PreDepends,
    Depends,
    Recommends,
    Suggests,
    Enhances,
    Provides,
    Conflicts,
    Breaks,
    Replaces,
}

impl RelationField {
    /// Get the canonical Debian field name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreDepends => "Pre-Depends",
            Self::Depends => "Depends",
            Self::Recommends => "Recommends",
            Self::Suggests => "Suggests",
            Self::Enhances => "Enhances",
            Self::Provides => "Provides",
            Self::Conflicts => "Conflicts",
            Self::Breaks => "Breaks",
            Self::Replaces => "Replaces",
        }
    }

    /// All supported relationship fields in stable output order.
    pub const ALL: [Self; 9] = [
        Self::PreDepends,
        Self::Depends,
        Self::Recommends,
        Self::Suggests,
        Self::Enhances,
        Self::Provides,
        Self::Conflicts,
        Self::Breaks,
        Self::Replaces,
    ];
}

/// Typed metadata needed to build an Arch package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebianMetadata {
    /// Exact Debian binary package name.
    pub name: String,
    /// Debian source package name, or `name` when `Source` is absent.
    pub source: String,
    /// Exact Debian version for provenance metadata.
    pub debian_version: String,
    /// Complete Arch version, including an optional epoch and package release.
    pub full_version: String,
    /// Short Debian description.  An empty or dot-only description is absent.
    pub description: Option<String>,
    /// Extended Debian description, when it has useful text.
    pub long_description: Option<String>,
    /// Debian homepage value.
    pub url: Option<String>,
    /// Mapped Arch architecture.
    pub architecture: String,
    /// Exact Debian architecture for provenance metadata.
    pub debian_architecture: String,
    /// Debian `Installed-Size` value in KiB.
    pub installed_size: Option<u64>,
    /// Nonstandard Debian license field, when present.
    pub license: Option<String>,
    /// Debian maintainer value, when present.
    pub maintainer: Option<String>,
    relations: BTreeMap<RelationField, String>,
}

impl DebianMetadata {
    /// Validate and convert one parsed control record.
    ///
    /// `Package`, `Version`, and `Architecture` are required because an Arch
    /// package cannot be built without them.
    pub fn from_control(control: &DebianControl) -> Result<Self> {
        let name = required_unfolded(control, "Package")?;
        validate_package_name(&name, "Package")?;

        let debian_version = required_unfolded(control, "Version")?;
        let full_version = ArchVersion::from_debian(&debian_version)?.to_string();

        let debian_architecture = required_unfolded(control, "Architecture")?;
        let architecture = map_architecture(&debian_architecture)?.to_owned();

        let source = match control.get_unfolded("Source") {
            Some(source) if !source.trim().is_empty() => parse_source_name(&source)?,
            _ => name.clone(),
        };

        let description = control.description();
        let installed_size = control
            .get_unfolded("Installed-Size")
            .filter(|value| !value.trim().is_empty())
            .map(|value| {
                value.parse::<u64>().context(format!(
                    "invalid Installed-Size value '{value}'; expected KiB as an integer"
                ))
            })
            .transpose()?;

        let mut relations = BTreeMap::new();
        for field in RelationField::ALL {
            if let Some(value) = control.get_unfolded(field.as_str()) {
                if !value.is_empty() {
                    relations.insert(field, value);
                }
            }
        }

        Ok(Self {
            name,
            source,
            debian_version,
            full_version,
            description: description.summary,
            long_description: description.long,
            url: optional_unfolded(control, "Homepage"),
            architecture,
            debian_architecture,
            installed_size,
            license: optional_unfolded(control, "License"),
            maintainer: optional_unfolded(control, "Maintainer"),
            relations,
        })
    }

    /// Get the unfolded source text of a relationship field.
    pub fn relation_source(&self, field: RelationField) -> Option<&str> {
        self.relations.get(&field).map(String::as_str)
    }

    /// Iterate over present relationship fields in stable field order.
    pub fn relation_sources(&self) -> impl Iterator<Item = (RelationField, &str)> {
        self.relations
            .iter()
            .map(|(field, value)| (*field, value.as_str()))
    }
}

fn required_unfolded(control: &DebianControl, field: &str) -> Result<String> {
    let value = control
        .get_unfolded(field)
        .context(format!("required control field '{field}' is missing"))?;
    if value.trim().is_empty() {
        Err(AppError::new(format!(
            "required control field '{field}' is empty"
        )))
    } else {
        Ok(value)
    }
}

fn optional_unfolded(control: &DebianControl, field: &str) -> Option<String> {
    control
        .get_unfolded(field)
        .filter(|value| !value.trim().is_empty())
}

fn parse_source_name(source: &str) -> Result<String> {
    let source = source.trim();
    let name = if source.ends_with(')') {
        source
            .rfind(" (")
            .map_or(source, |version_start| &source[..version_start])
    } else {
        source
    };
    validate_package_name(name, "Source")?;
    Ok(name.to_owned())
}

fn validate_package_name(name: &str, field: &str) -> Result<()> {
    let bytes = name.as_bytes();
    let valid = bytes.len() >= 2 && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit());
    let valid = valid
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"+.-".contains(byte)
        });
    if valid {
        Ok(())
    } else {
        Err(AppError::new(format!(
            "invalid Debian {field} package name '{name}'"
        )))
    }
}

/// A Debian version converted to the parts used in an Arch package version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchVersion {
    /// A nonzero Debian epoch.  Epoch zero is omitted from Arch output.
    pub epoch: Option<u64>,
    /// Debian version text with characters forbidden by Arch escaped.
    pub pkgver: String,
    /// Converter package release.  It is `1` for a new conversion.
    pub pkgrel: u64,
}

impl ArchVersion {
    /// Convert an exact Debian version into an Arch version.
    pub fn from_debian(version: &str) -> Result<Self> {
        validate_debian_version(version)?;
        let (epoch, body) = match version.split_once(':') {
            Some((epoch, body)) => {
                if epoch.is_empty() || !epoch.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(AppError::new(format!(
                        "invalid Debian version epoch in '{version}'"
                    )));
                }
                let parsed = epoch
                    .parse::<u64>()
                    .context(format!("Debian version epoch is too large in '{version}'"))?;
                ((parsed != 0).then_some(parsed), body)
            }
            None => (None, version),
        };

        if body.is_empty() {
            return Err(AppError::new(format!(
                "Debian version '{version}' has an empty version after its epoch"
            )));
        }

        let mut pkgver = String::with_capacity(body.len());
        for character in body.chars() {
            match character {
                '%' => pkgver.push_str("%25"),
                '-' => pkgver.push_str("%2D"),
                ':' => pkgver.push_str("%3A"),
                '/' => pkgver.push_str("%2F"),
                _ => pkgver.push(character),
            }
        }

        Ok(Self {
            epoch,
            pkgver,
            pkgrel: 1,
        })
    }
}

impl Display for ArchVersion {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        if let Some(epoch) = self.epoch {
            write!(formatter, "{epoch}:")?;
        }
        write!(formatter, "{}-{}", self.pkgver, self.pkgrel)
    }
}

/// Convert a Debian version into a complete Arch version string.
pub fn debian_version_to_arch(version: &str) -> Result<String> {
    ArchVersion::from_debian(version).map(|version| version.to_string())
}

fn validate_debian_version(version: &str) -> Result<()> {
    if version.is_empty() {
        return Err(AppError::new("the Debian Version field is empty"));
    }
    if version
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(AppError::new(format!(
            "Debian version '{version}' contains whitespace or a control character"
        )));
    }

    let body = version
        .split_once(':')
        .map_or(version, |(_, version_body)| version_body);
    if !body.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        return Err(AppError::new(format!(
            "Debian version '{version}' must start with a digit after its optional epoch"
        )));
    }
    if !body.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(byte, b'.' | b'+' | b'~' | b'-' | b':' | b'/' | b'%')
    }) {
        return Err(AppError::new(format!(
            "Debian version '{version}' contains an invalid character"
        )));
    }
    Ok(())
}

/// Map a Debian binary architecture to the matching Arch architecture.
pub fn map_architecture(architecture: &str) -> Result<&'static str> {
    match architecture {
        "all" => Ok("any"),
        "amd64" => Ok("x86_64"),
        "i386" => Ok("i686"),
        "arm64" => Ok("aarch64"),
        "armhf" => Ok("armv7h"),
        "armel" => Ok("arm"),
        "ppc64el" => Ok("ppc64le"),
        "ppc64" => Ok("ppc64"),
        "riscv64" => Ok("riscv64"),
        "s390x" => Ok("s390x"),
        _ => Err(AppError::new(format!(
            "unsupported Debian architecture '{architecture}'; provide an explicit mapping"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHATGPT_CONTROL: &str = "Package: chatgpt\nVersion: 26.803.81509\nArchitecture: amd64\nDepends: libgtk-3-0,\n libc6 (>= 2.35)\nInstalled-Size: 1284968\nHomepage: https://example.invalid\nDescription: ChatGPT by OpenAI\n ChatGPT is an AI assistant.\n .\n A second paragraph.\n";

    #[test]
    fn parses_folded_fields_and_descriptions() {
        let control = DebianControl::parse(CHATGPT_CONTROL).unwrap();
        assert_eq!(control.get("depends"), Some("libgtk-3-0,\nlibc6 (>= 2.35)"));
        assert_eq!(
            control.get_unfolded("DEPENDS").as_deref(),
            Some("libgtk-3-0, libc6 (>= 2.35)")
        );
        assert_eq!(
            control.description(),
            DebianDescription {
                summary: Some("ChatGPT by OpenAI".to_owned()),
                long: Some("ChatGPT is an AI assistant.\n\nA second paragraph.".to_owned()),
            }
        );
    }

    #[test]
    fn dot_only_description_is_absent() {
        let control = DebianControl::parse(
            "Package: opencode\nVersion: 1.18.3\nArchitecture: amd64\nDescription:\n .\n",
        )
        .unwrap();
        let metadata = DebianMetadata::from_control(&control).unwrap();
        assert_eq!(metadata.description, None);
        assert_eq!(metadata.long_description, None);
    }

    #[test]
    fn builds_typed_metadata() {
        let control = DebianControl::parse(CHATGPT_CONTROL).unwrap();
        let metadata = DebianMetadata::from_control(&control).unwrap();
        assert_eq!(metadata.name, "chatgpt");
        assert_eq!(metadata.source, "chatgpt");
        assert_eq!(metadata.full_version, "26.803.81509-1");
        assert_eq!(metadata.architecture, "x86_64");
        assert_eq!(metadata.debian_architecture, "amd64");
        assert_eq!(metadata.installed_size, Some(1_284_968));
        assert_eq!(
            metadata.relation_source(RelationField::Depends),
            Some("libgtk-3-0, libc6 (>= 2.35)")
        );
    }

    #[test]
    fn removes_source_version() {
        let control = DebianControl::parse(
            "Package: binary-name\nSource: source-name (2:1.0-1)\nVersion: 1.0\nArchitecture: all\n",
        )
        .unwrap();
        let metadata = DebianMetadata::from_control(&control).unwrap();
        assert_eq!(metadata.source, "source-name");
        assert_eq!(metadata.architecture, "any");
    }

    #[test]
    fn converts_epoch_and_escapes_arch_forbidden_characters() {
        assert_eq!(
            debian_version_to_arch("2:1.0~rc1-3").unwrap(),
            "2:1.0~rc1%2D3-1"
        );
        assert_eq!(debian_version_to_arch("0:1.0-1").unwrap(), "1.0%2D1-1");
        assert_eq!(debian_version_to_arch("1:2/3:4").unwrap(), "1:2%2F3%3A4-1");
    }

    #[test]
    fn rejects_missing_required_fields_and_bad_records() {
        let missing = DebianControl::parse("Package: example\nVersion: 1\n").unwrap();
        assert!(DebianMetadata::from_control(&missing).is_err());
        assert!(DebianControl::parse(" orphan continuation\n").is_err());
        assert!(DebianControl::parse("Package: one\n\nPackage: two\n").is_err());
        assert!(DebianControl::parse("Package: one\nPackage: two\n").is_err());
    }

    #[test]
    fn maps_all_supported_architectures() {
        let cases = [
            ("all", "any"),
            ("amd64", "x86_64"),
            ("i386", "i686"),
            ("arm64", "aarch64"),
            ("armhf", "armv7h"),
            ("armel", "arm"),
            ("ppc64el", "ppc64le"),
            ("ppc64", "ppc64"),
            ("riscv64", "riscv64"),
            ("s390x", "s390x"),
        ];
        for (debian, arch) in cases {
            assert_eq!(map_architecture(debian).unwrap(), arch);
        }
        assert!(map_architecture("mips64el").is_err());
    }
}
