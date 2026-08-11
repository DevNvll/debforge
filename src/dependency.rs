//! Debian package relation parsing and deterministic Arch dependency mapping.
//!
//! A [`PackageCatalog`] loads pacman's package names once.  A
//! [`DependencyResolver`] then performs only in-memory lookups for every
//! relation in the package.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display, Formatter};
use std::io::ErrorKind;
use std::process::Command;

use crate::error::{AppError, Context, OptionContext, Result};

/// A comma-separated Debian relation list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RelationSet {
    /// Relation groups in their source order.
    pub groups: Vec<RelationGroup>,
}

impl RelationSet {
    /// Parse Debian dependency, recommendation, provide, or conflict syntax.
    ///
    /// Commas separate groups and vertical bars separate alternatives.  The
    /// parser also accepts versions, multiarch qualifiers, architecture
    /// restrictions, and build-profile restrictions.
    pub fn parse(source: &str) -> Result<Self> {
        if source.trim().is_empty() {
            return Ok(Self::default());
        }

        let mut groups = Vec::new();
        for group_source in split_top_level(source, ',')? {
            let group_source = group_source.trim();
            if group_source.is_empty() {
                return Err(AppError::new("empty relation group after a comma"));
            }
            let mut alternatives = Vec::new();
            for relation_source in split_top_level(group_source, '|')? {
                let relation_source = relation_source.trim();
                if relation_source.is_empty() {
                    return Err(AppError::new("empty alternative after a vertical bar"));
                }
                alternatives.push(Relation::parse(relation_source)?);
            }
            groups.push(RelationGroup { alternatives });
        }
        Ok(Self { groups })
    }

    /// Append another parsed relation set while preserving source order.
    pub fn append(&mut self, mut other: Self) {
        self.groups.append(&mut other.groups);
    }

    /// Return `true` when the set has no relation groups.
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }
}

/// Alternatives that satisfy one dependency group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelationGroup {
    /// Alternatives in Debian preference order.
    pub alternatives: Vec<Relation>,
}

/// One named package relation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relation {
    /// Debian binary or virtual package name, without a qualifier.
    pub name: String,
    /// Optional Debian multiarch qualifier.
    pub qualifier: Option<PackageQualifier>,
    /// Optional Debian version constraint.
    pub version: Option<VersionConstraint>,
    /// Terms from the optional square-bracket architecture restriction.
    pub architecture_restrictions: Vec<Restriction>,
    /// Build-profile restriction lists.  Each inner list comes from one pair
    /// of angle brackets.
    pub profile_restrictions: Vec<Vec<Restriction>>,
}

impl Relation {
    fn parse(source: &str) -> Result<Self> {
        let bytes = source.as_bytes();
        let mut cursor = 0;
        while cursor < bytes.len()
            && !bytes[cursor].is_ascii_whitespace()
            && !matches!(bytes[cursor], b':' | b'(' | b'[' | b'<')
        {
            cursor += 1;
        }
        let name = &source[..cursor];
        validate_relation_name(name)?;

        let qualifier = if bytes.get(cursor) == Some(&b':') {
            cursor += 1;
            let start = cursor;
            while cursor < bytes.len()
                && !bytes[cursor].is_ascii_whitespace()
                && !matches!(bytes[cursor], b'(' | b'[' | b'<')
            {
                cursor += 1;
            }
            let qualifier = source
                .get(start..cursor)
                .context("a package qualifier ended in the middle of a UTF-8 character")?;
            Some(PackageQualifier::parse(qualifier)?)
        } else {
            None
        };

        let mut version = None;
        let mut architecture_restrictions = Vec::new();
        let mut profile_restrictions = Vec::new();

        while cursor < bytes.len() {
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if cursor == bytes.len() {
                break;
            }

            let (opening, closing) = match bytes[cursor] {
                b'(' => (b'(', b')'),
                b'[' => (b'[', b']'),
                b'<' => (b'<', b'>'),
                byte => {
                    return Err(AppError::new(format!(
                        "unexpected character '{}' in relation '{source}'",
                        char::from(byte)
                    )));
                }
            };
            let content_start = cursor + 1;
            let relative_end = bytes[content_start..]
                .iter()
                .position(|byte| *byte == closing)
                .context(format!(
                    "unclosed '{}' section in relation '{source}'",
                    char::from(opening)
                ))?;
            let content_end = content_start + relative_end;
            let content = source
                .get(content_start..content_end)
                .context("a relation section ended in the middle of a UTF-8 character")?;
            cursor = content_end + 1;

            match opening {
                b'(' => {
                    if version.is_some() {
                        return Err(AppError::new(format!(
                            "relation '{source}' has more than one version constraint"
                        )));
                    }
                    version = Some(VersionConstraint::parse(content)?);
                }
                b'[' => {
                    if !architecture_restrictions.is_empty() {
                        return Err(AppError::new(format!(
                            "relation '{source}' has more than one architecture restriction"
                        )));
                    }
                    architecture_restrictions = parse_restrictions(content, "architecture")?;
                    let has_positive = architecture_restrictions
                        .iter()
                        .any(|restriction| !restriction.negated);
                    let has_negative = architecture_restrictions
                        .iter()
                        .any(|restriction| restriction.negated);
                    if has_positive && has_negative {
                        return Err(AppError::new(format!(
                            "relation '{source}' mixes positive and negative architecture restrictions"
                        )));
                    }
                }
                b'<' => {
                    profile_restrictions.push(parse_restrictions(content, "build profile")?);
                }
                _ => unreachable!(),
            }
        }

        Ok(Self {
            name: name.to_owned(),
            qualifier,
            version,
            architecture_restrictions,
            profile_restrictions,
        })
    }

    /// Test the relation against a Debian target architecture.
    pub fn matches_architecture(&self, target: &str) -> bool {
        if self.architecture_restrictions.is_empty() {
            return true;
        }
        let positives = self
            .architecture_restrictions
            .iter()
            .filter(|restriction| !restriction.negated)
            .collect::<Vec<_>>();
        if positives.is_empty() {
            self.architecture_restrictions
                .iter()
                .all(|restriction| !architecture_term_matches(&restriction.name, target))
        } else {
            positives
                .iter()
                .any(|restriction| architecture_term_matches(&restriction.name, target))
        }
    }

    /// Test all build-profile restrictions against a set of active profiles.
    pub fn matches_profiles(&self, active: &BTreeSet<String>) -> bool {
        if self.profile_restrictions.is_empty() {
            return true;
        }
        self.profile_restrictions.iter().any(|list| {
            list.iter()
                .all(|restriction| active.contains(&restriction.name) != restriction.negated)
        })
    }
}

/// A package architecture qualifier from `name:qualifier`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackageQualifier {
    Any,
    Native,
    Architecture(String),
}

impl PackageQualifier {
    fn parse(source: &str) -> Result<Self> {
        if source.is_empty()
            || !source
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(AppError::new(format!(
                "invalid Debian package qualifier '{source}'"
            )));
        }
        Ok(match source {
            "any" => Self::Any,
            "native" => Self::Native,
            architecture => Self::Architecture(architecture.to_owned()),
        })
    }
}

/// A version comparison operator in a Debian relation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersionOperator {
    LessThan,
    LessOrEqual,
    Equal,
    GreaterOrEqual,
    GreaterThan,
}

impl VersionOperator {
    fn parse(source: &str) -> Result<Self> {
        match source {
            "<<" | "<" => Ok(Self::LessThan),
            "<=" => Ok(Self::LessOrEqual),
            "=" => Ok(Self::Equal),
            ">=" => Ok(Self::GreaterOrEqual),
            ">>" | ">" => Ok(Self::GreaterThan),
            _ => Err(AppError::new(format!(
                "invalid Debian version operator '{source}'"
            ))),
        }
    }

    const fn arch_text(self) -> &'static str {
        match self {
            Self::LessThan => "<",
            Self::LessOrEqual => "<=",
            Self::Equal => "=",
            Self::GreaterOrEqual => ">=",
            Self::GreaterThan => ">",
        }
    }
}

/// A parsed version constraint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionConstraint {
    pub operator: VersionOperator,
    pub version: String,
}

impl VersionConstraint {
    fn parse(source: &str) -> Result<Self> {
        let mut parts = source.split_whitespace();
        let operator = VersionOperator::parse(
            parts
                .next()
                .context("a Debian version constraint has no operator")?,
        )?;
        let version = parts
            .next()
            .context("a Debian version constraint has no version")?;
        if parts.next().is_some()
            || version.is_empty()
            || version
                .bytes()
                .any(|byte| byte.is_ascii_control() || matches!(byte, b',' | b'|' | b'(' | b')'))
        {
            return Err(AppError::new(format!(
                "invalid Debian version constraint '({source})'"
            )));
        }
        Ok(Self {
            operator,
            version: version.to_owned(),
        })
    }
}

/// One positive or negative architecture or build-profile term.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Restriction {
    pub name: String,
    pub negated: bool,
}

fn parse_restrictions(source: &str, kind: &str) -> Result<Vec<Restriction>> {
    let mut restrictions = Vec::new();
    for term in source.split_whitespace() {
        let (negated, name) = term
            .strip_prefix('!')
            .map_or((false, term), |name| (true, name));
        if name.is_empty()
            || !name.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'+' | b'.' | b'-')
            })
        {
            return Err(AppError::new(format!(
                "invalid {kind} restriction '{term}'"
            )));
        }
        restrictions.push(Restriction {
            name: name.to_owned(),
            negated,
        });
    }
    if restrictions.is_empty() {
        Err(AppError::new(format!("empty {kind} restriction")))
    } else {
        Ok(restrictions)
    }
}

fn validate_relation_name(name: &str) -> Result<()> {
    let bytes = name.as_bytes();
    let first_is_valid = bytes
        .first()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
    if bytes.len() < 2
        || !first_is_valid
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"+.-".contains(byte))
    {
        Err(AppError::new(format!(
            "invalid Debian relation package name '{name}'"
        )))
    } else {
        Ok(())
    }
}

fn split_top_level(source: &str, separator: char) -> Result<Vec<&str>> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut stack = Vec::new();
    for (index, character) in source.char_indices() {
        match character {
            '(' | '[' if stack.is_empty() => stack.push(character),
            '<' if stack.is_empty() => stack.push(character),
            ')' => {
                if stack.pop() != Some('(') {
                    return Err(AppError::new(format!(
                        "unmatched '{character}' in relation list '{source}'"
                    )));
                }
            }
            ']' => {
                if stack.pop() != Some('[') {
                    return Err(AppError::new(format!(
                        "unmatched '{character}' in relation list '{source}'"
                    )));
                }
            }
            '>' if stack.last() == Some(&'<') => {
                stack.pop();
            }
            '>' if stack.last() != Some(&'(') => {
                return Err(AppError::new(format!(
                    "unmatched '{character}' in relation list '{source}'"
                )));
            }
            _ if character == separator && stack.is_empty() => {
                parts.push(&source[start..index]);
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    if let Some(opening) = stack.pop() {
        return Err(AppError::new(format!(
            "unclosed '{opening}' in relation list '{source}'"
        )));
    }
    parts.push(&source[start..]);
    Ok(parts)
}

fn architecture_term_matches(term: &str, target: &str) -> bool {
    if term == "any" || term == target || target == "all" {
        return true;
    }
    if let Some(cpu) = term.strip_prefix("any-") {
        return target == cpu || target.ends_with(&format!("-{cpu}"));
    }
    if let Some(os) = term.strip_suffix("-any") {
        // Debian's common binary architecture names have Linux as their
        // implicit operating system.  Explicit OS-CPU names keep their prefix.
        return os == "linux" && !target.contains('-')
            || target
                .split_once('-')
                .is_some_and(|(target_os, _)| target_os == os);
    }
    false
}

/// Names read from the local pacman databases.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackageCatalog {
    available: BTreeSet<String>,
    installed: BTreeSet<String>,
    warnings: Vec<String>,
    available_query_succeeded: bool,
    installed_query_succeeded: bool,
}

impl PackageCatalog {
    /// Load the sync and installed package-name sets with one process each.
    ///
    /// A missing or unusable pacman command produces an empty set and a
    /// warning.  It does not stop conversion.  No database update or network
    /// operation is performed.
    pub fn load() -> Result<Self> {
        let available = run_pacman_query(&["-Slq"], "available")?;
        let installed = run_pacman_query(&["-Qq"], "installed")?;
        let mut warnings = available.warnings;
        for warning in installed.warnings {
            push_unique(&mut warnings, warning);
        }
        Ok(Self {
            available: available.names,
            installed: installed.names,
            warnings,
            available_query_succeeded: available.succeeded,
            installed_query_succeeded: installed.succeeded,
        })
    }

    /// Build a deterministic catalog without starting pacman.
    pub fn from_packages<A, I, AS, IS>(available: A, installed: I) -> Self
    where
        A: IntoIterator<Item = AS>,
        I: IntoIterator<Item = IS>,
        AS: Into<String>,
        IS: Into<String>,
    {
        Self {
            available: available.into_iter().map(Into::into).collect(),
            installed: installed.into_iter().map(Into::into).collect(),
            warnings: Vec::new(),
            available_query_succeeded: true,
            installed_query_succeeded: true,
        }
    }

    /// Build a catalog from the exact stdout of `pacman -Slq` and `pacman -Qq`.
    pub fn from_pacman_output(available: &str, installed: &str) -> Self {
        Self::from_packages(nonempty_lines(available), nonempty_lines(installed))
    }

    /// Test whether a package is in the sync database or is installed.
    pub fn contains(&self, name: &str) -> bool {
        self.available.contains(name) || self.installed.contains(name)
    }

    /// Test whether a package is installed.
    pub fn is_installed(&self, name: &str) -> bool {
        self.installed.contains(name)
    }

    /// Test whether a package is in a configured sync database.
    pub fn is_available(&self, name: &str) -> bool {
        self.available.contains(name)
    }

    /// Warnings produced while the catalog was loaded.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Return `true` when the sync database query completed successfully.
    pub fn availability_is_known(&self) -> bool {
        self.available_query_succeeded
    }

    /// Return `true` when the installed database query completed successfully.
    pub fn installed_state_is_known(&self) -> bool {
        self.installed_query_succeeded
    }
}

struct PacmanQuery {
    names: BTreeSet<String>,
    warnings: Vec<String>,
    succeeded: bool,
}

fn run_pacman_query(arguments: &[&str], label: &str) -> Result<PacmanQuery> {
    let output = match Command::new("pacman").args(arguments).output() {
        Ok(output) => output,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(PacmanQuery {
                names: BTreeSet::new(),
                warnings: vec![
                    "pacman is not available; dependency availability could not be checked"
                        .to_owned(),
                ],
                succeeded: false,
            });
        }
        Err(error) => {
            return Ok(PacmanQuery {
                names: BTreeSet::new(),
                warnings: vec![format!("could not read {label} pacman packages: {error}")],
                succeeded: false,
            });
        }
    };

    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let suffix = if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        };
        return Ok(PacmanQuery {
            names: BTreeSet::new(),
            warnings: vec![format!(
                "pacman could not list {label} packages (status {}){suffix}",
                output.status
            )],
            succeeded: false,
        });
    }

    let stdout = String::from_utf8(output.stdout)
        .context(format!("pacman returned non-UTF-8 {label} package names"))?;
    Ok(PacmanQuery {
        names: nonempty_lines(&stdout).map(str::to_owned).collect(),
        warnings: Vec::new(),
        succeeded: true,
    })
}

fn nonempty_lines(source: &str) -> impl Iterator<Item = &str> {
    source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
}

/// One Debian-to-Arch package-name mapping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageMapping {
    pub arch_name: String,
    /// Keep Debian version constraints only when this value is true.
    pub version_compatible: bool,
}

impl PackageMapping {
    pub fn new(arch_name: impl Into<String>, version_compatible: bool) -> Self {
        Self {
            arch_name: arch_name.into(),
            version_compatible,
        }
    }
}

/// Get the maintained Arch name for a Debian package.
///
/// `None` means that the resolver will keep the valid Debian name and report a
/// warning, unless the same name occurs in the local pacman catalog.
pub fn built_in_package_mapping(name: &str) -> Option<(&str, bool)> {
    let arch_name = match name {
        // ChatGPT and OpenCode Desktop runtime libraries.
        "libgtk-3-0" => "gtk3",
        "libnotify4" => "libnotify",
        "libnss3" => "nss",
        "libatspi2.0-0" | "libatk1.0-0" | "libatk-bridge2.0-0" => "at-spi2-core",
        "libdrm2" => "libdrm",
        "libgbm1" => "mesa",
        "libglib2.0-0" | "libglib2.0-bin" => "glib2",
        "libasound2" => "alsa-lib",
        "libc6" => "glibc",
        "libcairo2" => "cairo",
        "libcups2" => "libcups",
        "libdbus-1-3" => "dbus",
        "libexpat1" => "expat",
        "libgcc-s1" | "libstdc++6" => "gcc-libs",
        "libgdk-pixbuf-2.0-0" => "gdk-pixbuf2",
        "libgl1" => "libglvnd",
        "libgraphite2-3" => "graphite",
        "libnspr4" => "nspr",
        "libpango-1.0-0" => "pango",
        "libssl3" => "openssl",
        "libudev1" => "systemd-libs",
        "libusb-1.0-0" => "libusb",
        "libx11-6" | "libx11-xcb1" => "libx11",
        "libxcb1" | "libxcb-dri3-0" => "libxcb",
        "libxcomposite1" => "libxcomposite",
        "libxdamage1" => "libxdamage",
        "libxext6" => "libxext",
        "libxfixes3" => "libxfixes",
        "libxkbcommon0" => "libxkbcommon",
        "libxrandr2" => "libxrandr",
        "libxss1" => "libxss",
        "libxtst6" => "libxtst",
        "libuuid1" => "util-linux-libs",
        "libsecret-1-0" => "libsecret",
        "libappindicator3-1" => "libappindicator",
        "xz-utils" => "xz",
        "gvfs-bin" => "gvfs",
        "gir1.2-gnomekeyring-1.0" | "libgnome-keyring0" => "gnome-keyring",

        // Common library package names used by other desktop Debian packages.
        "libfontconfig1" => "fontconfig",
        "libfreetype6" => "freetype2",
        "libxi6" => "libxi",
        "libxinerama1" => "libxinerama",
        "libxrender1" => "libxrender",
        "zlib1g" => "zlib",
        "libcurl4" => "curl",
        "libsqlite3-0" => "sqlite",
        "libreadline8" => "readline",
        "libncurses6" => "ncurses",
        "liblzma5" => "xz",
        "libbz2-1.0" => "bzip2",
        "libzstd1" => "zstd",

        // Explicit identity entries suppress false unknown-name warnings for
        // every same-name relation in the two acceptance-test packages.
        "xdg-utils" | "kde-cli-tools" | "kde-runtime" | "trash-cli" | "pulseaudio" | "git"
        | "lsb-release" => name,
        _ => return None,
    };
    // Debian and Arch package release versions are not assumed equivalent.
    Some((arch_name, false))
}

/// The selected package and the unused alternatives from one relation group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlternativeSelection {
    pub selected: String,
    pub other_choices: Vec<String>,
}

/// Deterministic dependency resolution output.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolution {
    /// Arch dependency strings in first-occurrence order.
    pub dependencies: Vec<String>,
    /// Conversion warnings in stable order, without duplicates.
    pub warnings: Vec<String>,
    /// Decisions for groups that had more than one active alternative.
    pub alternative_selections: Vec<AlternativeSelection>,
}

impl Resolution {
    /// Add another result and remove duplicate output strings and warnings.
    pub fn merge(&mut self, other: Self) {
        for dependency in other.dependencies {
            if !self.dependencies.contains(&dependency) {
                self.dependencies.push(dependency);
            }
        }
        for warning in other.warnings {
            push_unique(&mut self.warnings, warning);
        }
        self.alternative_selections
            .extend(other.alternative_selections);
    }
}

/// Resolve parsed Debian relations against one immutable package catalog.
pub struct DependencyResolver<'catalog> {
    catalog: &'catalog PackageCatalog,
    target_architecture: String,
    active_profiles: BTreeSet<String>,
    mappings: BTreeMap<String, PackageMapping>,
}

impl<'catalog> DependencyResolver<'catalog> {
    /// Create a resolver for the current machine's corresponding Debian
    /// architecture.  Use [`Self::with_target_architecture`] for a cross-build.
    pub fn new(catalog: &'catalog PackageCatalog) -> Self {
        Self {
            catalog,
            target_architecture: host_debian_architecture().to_owned(),
            active_profiles: BTreeSet::new(),
            mappings: BTreeMap::new(),
        }
    }

    /// Set the Debian output architecture used for restriction evaluation.
    pub fn with_target_architecture(mut self, architecture: impl Into<String>) -> Self {
        self.target_architecture = architecture.into();
        self
    }

    /// Set active Debian build profiles used for restriction evaluation.
    pub fn with_active_profiles<I, S>(mut self, profiles: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.active_profiles = profiles.into_iter().map(Into::into).collect();
        self
    }

    /// Add or replace one mapping.  Custom mappings take priority over the
    /// built-in mapping table.
    pub fn add_mapping(
        &mut self,
        debian_name: impl Into<String>,
        mapping: PackageMapping,
    ) -> Result<()> {
        let debian_name = debian_name.into();
        validate_relation_name(&debian_name)?;
        validate_arch_package_name(&mapping.arch_name)?;
        self.mappings.insert(debian_name, mapping);
        Ok(())
    }

    /// Resolve required dependencies and pre-dependencies.
    pub fn resolve_required(&self, relations: &RelationSet) -> Resolution {
        self.resolve(relations, true, None)
    }

    /// Parse and resolve several required relation fields as one set.
    pub fn resolve_required_sources(&self, sources: &[&str]) -> Result<Resolution> {
        let mut combined = RelationSet::default();
        for source in sources {
            combined.append(RelationSet::parse(source)?);
        }
        Ok(self.resolve_required(&combined))
    }

    /// Resolve recommendations or suggestions as Arch optional dependencies.
    /// Each output string has the form `dependency: reason`.
    pub fn resolve_optional(&self, relations: &RelationSet, reason: &str) -> Resolution {
        self.resolve(relations, true, Some(reason))
    }

    /// Resolve name-only relations such as Provides, Conflicts, and Replaces.
    /// Availability is not required because these names can be virtual or can
    /// describe packages that are not in a configured repository.
    pub fn resolve_names(&self, relations: &RelationSet) -> Resolution {
        self.resolve(relations, false, None)
    }

    fn resolve(
        &self,
        relations: &RelationSet,
        check_availability: bool,
        optional_reason: Option<&str>,
    ) -> Resolution {
        let mut warnings = self.catalog.warnings.clone();
        let mut alternative_selections = Vec::new();
        let mut selected_dependencies: Vec<SelectedDependency> = Vec::new();
        let mut selected_indices = BTreeMap::<String, usize>::new();

        for group in &relations.groups {
            let active = group
                .alternatives
                .iter()
                .filter(|relation| {
                    relation.matches_architecture(&self.target_architecture)
                        && relation.matches_profiles(&self.active_profiles)
                })
                .collect::<Vec<_>>();
            if active.is_empty() {
                continue;
            }

            let candidates = active
                .iter()
                .map(|relation| self.map_candidate(relation))
                .collect::<Vec<_>>();
            let selected_index = if check_availability {
                candidates
                    .iter()
                    .position(|candidate| self.catalog.is_installed(&candidate.name))
                    .or_else(|| {
                        candidates
                            .iter()
                            .position(|candidate| self.catalog.is_available(&candidate.name))
                    })
                    .unwrap_or(0)
            } else {
                0
            };
            let selected = candidates[selected_index].clone();

            if candidates.len() > 1 {
                alternative_selections.push(AlternativeSelection {
                    selected: selected.name.clone(),
                    other_choices: candidates
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| *index != selected_index)
                        .map(|(_, candidate)| candidate.name.clone())
                        .collect(),
                });
            }

            if selected.unknown_mapping {
                push_unique(
                    &mut warnings,
                    format!(
                        "no Debian-to-Arch mapping is known for '{}'; kept the same package name",
                        selected.original_name
                    ),
                );
            }
            if selected.dropped_version {
                push_unique(
                    &mut warnings,
                    format!(
                        "dropped the Debian version constraint for '{}' because Arch versions are not marked compatible",
                        selected.original_name
                    ),
                );
            }
            if matches!(
                active[selected_index].qualifier,
                Some(PackageQualifier::Architecture(_))
            ) {
                push_unique(
                    &mut warnings,
                    format!(
                        "removed the foreign architecture qualifier from '{}'",
                        selected.original_name
                    ),
                );
            }
            if check_availability
                && self.catalog.availability_is_known()
                && self.catalog.installed_state_is_known()
                && !self.catalog.contains(&selected.name)
            {
                push_unique(
                    &mut warnings,
                    format!(
                        "selected Arch dependency '{}' is not installed or available in the local pacman databases",
                        selected.name
                    ),
                );
            }

            if let Some(existing_index) = selected_indices.get(&selected.name).copied() {
                let existing = &mut selected_dependencies[existing_index];
                if let Some(warning) = merge_constraints(&mut existing.version, selected.version) {
                    push_unique(&mut warnings, warning);
                }
            } else {
                selected_indices.insert(selected.name.clone(), selected_dependencies.len());
                selected_dependencies.push(SelectedDependency {
                    name: selected.name,
                    version: selected.version,
                });
            }
        }

        let dependencies = selected_dependencies
            .into_iter()
            .map(|dependency| {
                let rendered = dependency.to_string();
                optional_reason.map_or(rendered.clone(), |reason| {
                    if reason.trim().is_empty() {
                        rendered
                    } else {
                        format!("{rendered}: {}", reason.trim())
                    }
                })
            })
            .collect();

        Resolution {
            dependencies,
            warnings,
            alternative_selections,
        }
    }

    fn map_candidate(&self, relation: &Relation) -> MappedCandidate {
        let (mapping, explicit_mapping) = if let Some(mapping) = self.mappings.get(&relation.name) {
            (mapping.clone(), true)
        } else if let Some((arch_name, version_compatible)) =
            built_in_package_mapping(&relation.name)
        {
            (PackageMapping::new(arch_name, version_compatible), true)
        } else {
            (
                PackageMapping::new(&relation.name, false),
                self.catalog.contains(&relation.name),
            )
        };
        let version = mapping
            .version_compatible
            .then(|| relation.version.clone())
            .flatten();
        MappedCandidate {
            name: mapping.arch_name,
            original_name: relation.name.clone(),
            version,
            unknown_mapping: !explicit_mapping,
            dropped_version: relation.version.is_some() && !mapping.version_compatible,
        }
    }
}

#[derive(Clone)]
struct MappedCandidate {
    name: String,
    original_name: String,
    version: Option<VersionConstraint>,
    unknown_mapping: bool,
    dropped_version: bool,
}

struct SelectedDependency {
    name: String,
    version: Option<VersionConstraint>,
}

impl Display for SelectedDependency {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.name)?;
        if let Some(version) = &self.version {
            formatter.write_str(version.operator.arch_text())?;
            formatter.write_str(&version.version)?;
        }
        Ok(())
    }
}

fn validate_arch_package_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'@' | b'.' | b'_' | b'+' | b'-')
        })
    {
        Err(AppError::new(format!(
            "invalid Arch package name '{name}' in dependency mapping"
        )))
    } else {
        Ok(())
    }
}

fn host_debian_architecture() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "x86" => "i386",
        "aarch64" => "arm64",
        "arm" => "armhf",
        "powerpc64" => "ppc64",
        "riscv64" => "riscv64",
        "s390x" => "s390x",
        _ => "all",
    }
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}

fn merge_constraints(
    current: &mut Option<VersionConstraint>,
    incoming: Option<VersionConstraint>,
) -> Option<String> {
    let incoming = incoming?;
    let Some(existing) = current.as_ref() else {
        *current = Some(incoming);
        return None;
    };
    if existing == &incoming {
        return None;
    }

    let existing_is_lower = matches!(
        existing.operator,
        VersionOperator::GreaterOrEqual | VersionOperator::GreaterThan
    );
    let incoming_is_lower = matches!(
        incoming.operator,
        VersionOperator::GreaterOrEqual | VersionOperator::GreaterThan
    );
    let existing_is_upper = matches!(
        existing.operator,
        VersionOperator::LessOrEqual | VersionOperator::LessThan
    );
    let incoming_is_upper = matches!(
        incoming.operator,
        VersionOperator::LessOrEqual | VersionOperator::LessThan
    );

    let replace = if existing.operator == VersionOperator::Equal {
        false
    } else if incoming.operator == VersionOperator::Equal {
        true
    } else if existing_is_lower && incoming_is_lower {
        match compare_debian_versions(&existing.version, &incoming.version) {
            Ordering::Less => true,
            Ordering::Greater => false,
            Ordering::Equal => incoming.operator == VersionOperator::GreaterThan,
        }
    } else if existing_is_upper && incoming_is_upper {
        match compare_debian_versions(&existing.version, &incoming.version) {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => incoming.operator == VersionOperator::LessThan,
        }
    } else {
        return Some(format!(
            "could not combine incompatible version limits '{}{}' and '{}{}'; kept the first limit",
            existing.operator.arch_text(),
            existing.version,
            incoming.operator.arch_text(),
            incoming.version
        ));
    };

    if replace {
        *current = Some(incoming);
    }
    None
}

/// Compare two versions with Debian's epoch, upstream, and revision ordering.
///
/// This is used only to select the stronger compatible duplicate limit.  It
/// does not imply that a Debian version can be compared with every Arch package
/// version.
pub fn compare_debian_versions(left: &str, right: &str) -> Ordering {
    let (left_epoch, left_body) = split_epoch(left);
    let (right_epoch, right_body) = split_epoch(right);
    match compare_decimal(left_epoch, right_epoch) {
        Ordering::Equal => {}
        ordering => return ordering,
    }

    let (left_upstream, left_revision) = split_revision(left_body);
    let (right_upstream, right_revision) = split_revision(right_body);
    match compare_version_part(left_upstream, right_upstream) {
        Ordering::Equal => compare_version_part(left_revision, right_revision),
        ordering => ordering,
    }
}

fn split_epoch(version: &str) -> (&str, &str) {
    version.split_once(':').unwrap_or(("0", version))
}

fn split_revision(version: &str) -> (&str, &str) {
    version.rsplit_once('-').unwrap_or((version, "0"))
}

fn compare_decimal(left: &str, right: &str) -> Ordering {
    let left = left.trim_start_matches('0');
    let right = right.trim_start_matches('0');
    left.len()
        .cmp(&right.len())
        .then_with(|| left.as_bytes().cmp(right.as_bytes()))
}

fn compare_version_part(mut left: &str, mut right: &str) -> Ordering {
    while !left.is_empty() || !right.is_empty() {
        while left
            .as_bytes()
            .first()
            .is_some_and(|byte| !byte.is_ascii_digit())
            || right
                .as_bytes()
                .first()
                .is_some_and(|byte| !byte.is_ascii_digit())
        {
            let left_character = left.chars().next();
            let right_character = right.chars().next();
            let ordering = version_character_order(left_character)
                .cmp(&version_character_order(right_character));
            if ordering != Ordering::Equal {
                return ordering;
            }
            if left_character.is_some() {
                left = &left[left_character.map_or(0, char::len_utf8)..];
            }
            if right_character.is_some() {
                right = &right[right_character.map_or(0, char::len_utf8)..];
            }
        }

        let left_digits = left.bytes().take_while(u8::is_ascii_digit).count();
        let right_digits = right.bytes().take_while(u8::is_ascii_digit).count();
        let (left_number, left_rest) = left.split_at(left_digits);
        let (right_number, right_rest) = right.split_at(right_digits);
        let ordering = compare_decimal(left_number, right_number);
        if ordering != Ordering::Equal {
            return ordering;
        }
        left = left_rest;
        right = right_rest;
    }
    Ordering::Equal
}

fn version_character_order(character: Option<char>) -> i32 {
    match character {
        Some('~') => -1,
        None => 0,
        Some(character) if character.is_ascii_alphabetic() => character as i32,
        Some(character) => character as i32 + 256,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_complete_relation_syntax() {
        let parsed = RelationSet::parse(
            "libfoo:any (<< 2:3.0-1) [amd64 arm64] <!nocheck> | libbar:native, baz-qux (>> 1.0)",
        )
        .unwrap();
        assert_eq!(parsed.groups.len(), 2);
        let relation = &parsed.groups[0].alternatives[0];
        assert_eq!(relation.name, "libfoo");
        assert_eq!(relation.qualifier, Some(PackageQualifier::Any));
        assert_eq!(
            relation.version,
            Some(VersionConstraint {
                operator: VersionOperator::LessThan,
                version: "2:3.0-1".to_owned(),
            })
        );
        assert_eq!(relation.architecture_restrictions.len(), 2);
        assert!(relation.matches_architecture("amd64"));
        assert!(!relation.matches_architecture("i386"));
        assert!(relation.matches_profiles(&BTreeSet::new()));
    }

    #[test]
    fn evaluates_negative_architecture_and_profile_restrictions() {
        let parsed = RelationSet::parse("libfoo [!arm64] <stage1 !nocheck>").unwrap();
        let relation = &parsed.groups[0].alternatives[0];
        assert!(relation.matches_architecture("amd64"));
        assert!(!relation.matches_architecture("arm64"));
        let mut profiles = BTreeSet::new();
        profiles.insert("stage1".to_owned());
        assert!(relation.matches_profiles(&profiles));
        profiles.insert("nocheck".to_owned());
        assert!(!relation.matches_profiles(&profiles));
    }

    #[test]
    fn rejects_malformed_relations() {
        for source in [
            "libfoo |",
            "libfoo (>=)",
            "libfoo (>= 1",
            "libfoo [amd64 !arm64]",
            "Bad_Name",
            "libfoo:",
        ] {
            assert!(RelationSet::parse(source).is_err(), "accepted {source}");
        }
    }

    #[test]
    fn catalog_deduplicates_pacman_output() {
        let catalog = PackageCatalog::from_pacman_output("gtk3\nglib2\ngtk3\n", "glibc\n");
        assert!(catalog.is_available("gtk3"));
        assert!(catalog.is_installed("glibc"));
        assert!(!catalog.contains("missing"));
    }

    #[test]
    fn selects_an_installed_alternative_then_an_available_one() {
        let catalog = PackageCatalog::from_packages(["glib2", "kde-cli-tools"], ["kde-cli-tools"]);
        let resolver = DependencyResolver::new(&catalog);
        let relations =
            RelationSet::parse("libglib2.0-bin | kde-cli-tools | kde-runtime | trash-cli").unwrap();
        let resolution = resolver.resolve_required(&relations);
        assert_eq!(resolution.dependencies, ["kde-cli-tools"]);
        assert_eq!(resolution.alternative_selections.len(), 1);
        assert_eq!(
            resolution.alternative_selections[0].other_choices,
            ["glib2", "kde-runtime", "trash-cli"]
        );
    }

    #[test]
    fn maps_and_deduplicates_chatgpt_dependencies() {
        let catalog = PackageCatalog::from_packages(
            ["gtk3", "at-spi2-core", "glibc", "glib2", "mesa", "gcc-libs"],
            std::iter::empty::<&str>(),
        );
        let resolver = DependencyResolver::new(&catalog);
        let relations = RelationSet::parse(
            "libgtk-3-0, libatspi2.0-0, libatk1.0-0 (>= 2.32), libc6 (>= 2.30), libc6 (>= 2.35), libgbm1, libglib2.0-0, libgcc-s1, libstdc++6",
        )
        .unwrap();
        let resolution = resolver.resolve_required(&relations);
        assert_eq!(
            resolution.dependencies,
            ["gtk3", "at-spi2-core", "glibc", "mesa", "glib2", "gcc-libs"]
        );
        assert!(
            !resolution
                .dependencies
                .iter()
                .any(|dependency| dependency.contains("2.35"))
        );
    }

    #[test]
    fn includes_opencode_secret_and_uuid_mappings() {
        let catalog = PackageCatalog::from_packages(
            ["libsecret", "util-linux-libs", "libappindicator"],
            std::iter::empty::<&str>(),
        );
        let resolver = DependencyResolver::new(&catalog);
        let required =
            resolver.resolve_required(&RelationSet::parse("libuuid1, libsecret-1-0").unwrap());
        let optional = resolver.resolve_optional(
            &RelationSet::parse("libappindicator3-1").unwrap(),
            "Debian recommends",
        );
        assert_eq!(required.dependencies, ["util-linux-libs", "libsecret"]);
        assert_eq!(
            optional.dependencies,
            ["libappindicator: Debian recommends"]
        );
    }

    #[test]
    fn retains_and_merges_only_explicitly_compatible_versions() {
        let catalog = PackageCatalog::from_packages(["foo"], std::iter::empty::<&str>());
        let mut resolver = DependencyResolver::new(&catalog);
        resolver
            .add_mapping("libfoo", PackageMapping::new("foo", true))
            .unwrap();
        let relations = RelationSet::parse("libfoo (>= 2.30), libfoo (>= 2.35)").unwrap();
        let resolution = resolver.resolve_required(&relations);
        assert_eq!(resolution.dependencies, ["foo>=2.35"]);
    }

    #[test]
    fn warns_and_keeps_unknown_valid_names() {
        let catalog =
            PackageCatalog::from_packages(std::iter::empty::<&str>(), std::iter::empty::<&str>());
        let resolver = DependencyResolver::new(&catalog);
        let resolution = resolver.resolve_required(&RelationSet::parse("some-runtime").unwrap());
        assert_eq!(resolution.dependencies, ["some-runtime"]);
        assert!(
            resolution
                .warnings
                .iter()
                .any(|warning| warning.contains("no Debian-to-Arch mapping"))
        );
    }

    #[test]
    fn compares_debian_versions_for_stronger_limits() {
        assert_eq!(compare_debian_versions("2.35", "2.30"), Ordering::Greater);
        assert_eq!(compare_debian_versions("1.0~rc1", "1.0"), Ordering::Less);
        assert_eq!(
            compare_debian_versions("2:1.0-1", "1:9.0-9"),
            Ordering::Greater
        );
        assert_eq!(compare_debian_versions("1.01", "1.1"), Ordering::Equal);
    }
}
