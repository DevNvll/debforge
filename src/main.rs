use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use debtap_rs::archive::{self, PackageOptions};
use debtap_rs::cli::{self, Action, ConvertOptions, PkgbuildMode};
use debtap_rs::control::{ArchVersion, DebianControl, DebianMetadata, RelationField};
use debtap_rs::dependency::{DependencyResolver, PackageCatalog, RelationSet, Resolution};
use debtap_rs::error::{AppError, Context, Result};
use debtap_rs::package::{self, ArchPackageMetadata, PkgbuildSpec};
use debtap_rs::process;
use debtap_rs::scripts;
use debtap_rs::transform;
use debtap_rs::workspace::Workspace;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    match cli::parse()? {
        Action::Help => {
            print!("{}", cli::help());
            Ok(())
        }
        Action::Version => {
            println!("debtap-rs {}", cli::VERSION);
            Ok(())
        }
        Action::Update => update_catalog(),
        Action::Convert(options) => convert(options),
    }
}

fn update_catalog() -> Result<()> {
    let catalog = PackageCatalog::load()?;
    for warning in catalog.warnings() {
        eprintln!("Warning: {warning}");
    }
    if catalog.availability_is_known() {
        println!("The local Pacman catalog is ready.");
    } else {
        println!("The built-in dependency map is ready. Pacman package availability is unknown.");
    }
    println!("debtap-rs does not need the Debian and Ubuntu Contents databases.");
    Ok(())
}

fn convert(options: ConvertOptions) -> Result<()> {
    let started = Instant::now();
    process::require_tools(&["ar", "bsdtar", "zstd"])?;

    let input = fs::canonicalize(&options.input).context(format!(
        "Cannot resolve Debian package {}",
        options.input.display()
    ))?;
    if !input.is_file() {
        return Err(AppError::new(format!(
            "The input is not a regular file: {}",
            input.display()
        )));
    }

    let output_dir = output_directory(&input, options.output_dir.as_deref())?;
    fs::create_dir_all(&output_dir).context(format!(
        "Cannot create output directory {}",
        output_dir.display()
    ))?;

    progress(&options, "Reading Debian package metadata...");
    let workspace = Workspace::create(options.keep_work)?;
    let deb_archive = archive::extract_deb_members(&input, workspace.members_dir())?;
    let control_path =
        archive::extract_control(&deb_archive.control_member, workspace.control_dir())?;
    let control_text = fs::read_to_string(&control_path)
        .context(format!("Cannot read {}", control_path.display()))?;
    let control = DebianControl::parse(&control_text)?;
    let mut debian = DebianMetadata::from_control(&control)?;

    let mut warnings = Vec::new();
    if options.pseudo_64 {
        if debian.architecture == "i686" {
            debian.architecture = "x86_64".to_string();
            push_warning(
                &mut warnings,
                "Pseudo mode changed the output architecture from i686 to x86_64. Payload binaries remain 32-bit."
                    .to_string(),
            );
        } else {
            push_warning(
                &mut warnings,
                format!(
                    "Pseudo mode has no effect on Debian architecture '{}'.",
                    debian.debian_architecture
                ),
            );
        }
    }

    progress(
        &options,
        "Resolving dependencies from the local Pacman catalog...",
    );
    let catalog = PackageCatalog::load()?;
    let resolver = DependencyResolver::new(&catalog)
        .with_target_architecture(debian.debian_architecture.clone());
    let relations = resolve_relations(&debian, &resolver)?;
    warnings.extend(relations.warnings);

    progress(
        &options,
        "Extracting and normalizing the package payload...",
    );
    archive::extract_payload(&deb_archive.data_member, workspace.payload_dir())?;
    reject_reserved_payload_files(workspace.payload_dir())?;
    let transform = transform::normalize_payload(
        workspace.payload_dir(),
        &debian.architecture,
        workspace.control_dir(),
    )?;
    warnings.extend(transform.warnings);

    let script_output =
        scripts::generate_install_script(options.script_policy, workspace.control_dir())?;
    warnings.extend(script_output.warnings);

    let arch_version = ArchVersion::from_debian(&debian.debian_version)?;
    let builddate = build_timestamp(options.source_date_epoch)?;
    let metadata = ArchPackageMetadata {
        pkgname: debian.name.clone(),
        pkgbase: debian.source.clone(),
        full_version: debian.full_version.clone(),
        version: arch_version.pkgver.clone(),
        release: arch_version.pkgrel.to_string(),
        epoch: arch_version.epoch,
        description: debian.description.clone(),
        url: debian.url.clone(),
        builddate,
        packager: options.packager.clone(),
        installed_size: transform.installed_size,
        architecture: debian.architecture.clone(),
        licenses: parse_licenses(debian.license.as_deref()),
        depends: relations.required,
        optional_depends: relations.optional,
        provides: relations.provides,
        conflicts: relations.conflicts,
        replaces: relations.replaces,
        backups: transform.backup_paths,
        debian_package: debian.name.clone(),
        debian_version: debian.debian_version.clone(),
        debian_architecture: debian.debian_architecture.clone(),
    };
    metadata.validate()?;

    let package_path = output_dir.join(metadata.output_file_name()?);
    if package_path.exists() && !options.force && options.pkgbuild != PkgbuildMode::Only {
        return Err(AppError::new(format!(
            "The output package already exists: {}. Use --force to replace it.",
            package_path.display()
        )));
    }

    package::write_pkginfo(workspace.payload_dir(), &metadata)?;
    if let Some(install) = script_output.install.as_deref() {
        package::write_install(workspace.payload_dir(), install)?;
    }

    if options.generate_mtree && options.pkgbuild != PkgbuildMode::Only {
        progress(&options, "Generating the package file index...");
        archive::generate_mtree_with_epoch(
            workspace.payload_dir(),
            options.source_date_epoch.map(epoch_to_i64).transpose()?,
        )?;
    }

    let mut generated = Vec::new();
    if options.pkgbuild != PkgbuildMode::Only {
        progress(
            &options,
            &format!(
                "Writing the Arch package with Zstandard level {}...",
                options.compression_level
            ),
        );
        archive::create_package_with_options(
            workspace.payload_dir(),
            &package_path,
            PackageOptions {
                zstd_level: i32::from(options.compression_level),
                source_date_epoch: options.source_date_epoch.map(epoch_to_i64).transpose()?,
                overwrite: options.force,
                validate_with_pacman: true,
            },
        )?;
        generated.push(package_path.clone());
    }

    if options.pkgbuild != PkgbuildMode::None {
        progress(&options, "Generating a reproducible PKGBUILD...");
        let pkgbuild_directory = output_dir.join(format!("{}-PKGBUILD", metadata.pkgname));
        if pkgbuild_directory.exists() && options.force {
            fs::remove_dir_all(&pkgbuild_directory).context(format!(
                "Cannot replace PKGBUILD directory {}",
                pkgbuild_directory.display()
            ))?;
        }
        let hash = sha256_file(&input)?;
        let data_member = deb_archive
            .data_member_name
            .to_str()
            .ok_or_else(|| AppError::new("The data archive member name is not valid UTF-8."))?;
        let path = package::write_pkgbuild(
            &output_dir,
            &PkgbuildSpec {
                metadata: &metadata,
                source_deb: &input,
                data_member,
                sha256: &hash,
            },
        )?;
        generated.push(path);
    }

    print_warnings(&warnings);
    for path in &generated {
        println!("Created: {}", path.display());
    }
    println!(
        "Completed in {:.2} seconds.",
        started.elapsed().as_secs_f64()
    );
    if options.keep_work {
        println!("Work directory: {}", workspace.root().display());
    }
    Ok(())
}

#[derive(Debug, Default)]
struct ResolvedRelations {
    required: Vec<String>,
    optional: Vec<String>,
    provides: Vec<String>,
    conflicts: Vec<String>,
    replaces: Vec<String>,
    warnings: Vec<String>,
}

fn resolve_relations(
    metadata: &DebianMetadata,
    resolver: &DependencyResolver<'_>,
) -> Result<ResolvedRelations> {
    let mut required_set = RelationSet::default();
    append_relation_field(metadata, RelationField::PreDepends, &mut required_set)?;
    append_relation_field(metadata, RelationField::Depends, &mut required_set)?;
    let required = resolver.resolve_required(&required_set);

    let mut recommended_set = RelationSet::default();
    append_relation_field(metadata, RelationField::Recommends, &mut recommended_set)?;
    let recommended =
        resolver.resolve_optional(&recommended_set, "recommended by the Debian package");

    let mut suggested_set = RelationSet::default();
    append_relation_field(metadata, RelationField::Suggests, &mut suggested_set)?;
    let suggested = resolver.resolve_optional(&suggested_set, "suggested by the Debian package");

    let mut provides_set = RelationSet::default();
    append_relation_field(metadata, RelationField::Provides, &mut provides_set)?;
    let provides = resolver.resolve_names(&provides_set);

    let mut conflicts_set = RelationSet::default();
    append_relation_field(metadata, RelationField::Conflicts, &mut conflicts_set)?;
    append_relation_field(metadata, RelationField::Breaks, &mut conflicts_set)?;
    let conflicts = resolver.resolve_names(&conflicts_set);

    let mut replaces_set = RelationSet::default();
    append_relation_field(metadata, RelationField::Replaces, &mut replaces_set)?;
    let replaces = resolver.resolve_names(&replaces_set);

    let mut all = Resolution::default();
    all.merge(required.clone());
    all.merge(recommended.clone());
    all.merge(suggested.clone());
    all.merge(provides.clone());
    all.merge(conflicts.clone());
    all.merge(replaces.clone());
    for selection in all.alternative_selections {
        push_warning(
            &mut all.warnings,
            format!(
                "Selected '{}' from Debian alternatives; other choices were: {}.",
                selection.selected,
                selection.other_choices.join(", ")
            ),
        );
    }

    let required_dependencies = required.dependencies;
    let optional_dependencies = remove_required_optional(
        &required_dependencies,
        unique_joined(recommended.dependencies, suggested.dependencies),
    );

    Ok(ResolvedRelations {
        required: required_dependencies,
        optional: optional_dependencies,
        provides: provides.dependencies,
        conflicts: conflicts.dependencies,
        replaces: replaces.dependencies,
        warnings: all.warnings,
    })
}

fn append_relation_field(
    metadata: &DebianMetadata,
    field: RelationField,
    output: &mut RelationSet,
) -> Result<()> {
    if let Some(source) = metadata.relation_source(field) {
        output.append(RelationSet::parse(source)?);
    }
    Ok(())
}

fn unique_joined(left: Vec<String>, right: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    left.into_iter()
        .chain(right)
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

fn remove_required_optional(required: &[String], optional: Vec<String>) -> Vec<String> {
    let required_names = required
        .iter()
        .map(|value| relation_package_name(value))
        .collect::<BTreeSet<_>>();
    optional
        .into_iter()
        .filter(|value| !required_names.contains(relation_package_name(value)))
        .collect()
}

fn relation_package_name(value: &str) -> &str {
    value
        .split(['<', '>', '=', ':'])
        .next()
        .unwrap_or(value)
        .trim()
}

fn reject_reserved_payload_files(root: &Path) -> Result<()> {
    for name in [".PKGINFO", ".BUILDINFO", ".MTREE", ".INSTALL"] {
        let path = root.join(name);
        if fs::symlink_metadata(&path).is_ok() {
            return Err(AppError::new(format!(
                "The Debian payload contains reserved Arch metadata path '{name}'."
            )));
        }
    }
    Ok(())
}

fn output_directory(input: &Path, configured: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = configured {
        return Ok(path.to_path_buf());
    }
    input
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| AppError::new("The Debian package has no parent directory."))
}

fn build_timestamp(configured: Option<u64>) -> Result<u64> {
    if let Some(value) = configured {
        return Ok(value);
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| AppError::new(format!("The system clock is before 1970: {error}")))
}

fn epoch_to_i64(value: u64) -> Result<i64> {
    i64::try_from(value)
        .map_err(|_| AppError::new("SOURCE_DATE_EPOCH is too large for archive timestamps."))
}

fn parse_licenses(source: Option<&str>) -> Vec<String> {
    source
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn sha256_file(path: &Path) -> Result<String> {
    let sha256sum = process::require_tool("sha256sum")?;
    let output = process::capture_text(Command::new(sha256sum).arg("--").arg(path))?;
    output
        .split_whitespace()
        .next()
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(str::to_string)
        .ok_or_else(|| AppError::new("sha256sum returned an invalid digest."))
}

fn progress(options: &ConvertOptions, message: &str) {
    if !options.quiet {
        println!("==> {message}");
    }
}

fn push_warning(warnings: &mut Vec<String>, warning: String) {
    if !warnings.contains(&warning) {
        warnings.push(warning);
    }
}

fn print_warnings(warnings: &[String]) {
    let mut printed = BTreeSet::new();
    for warning in warnings {
        if printed.insert(warning) {
            eprintln!("Warning: {warning}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::remove_required_optional;

    #[test]
    fn removes_optional_dependencies_that_are_already_required() {
        let required = vec!["alsa-lib".to_string(), "glibc>=2.40".to_string()];
        let optional = vec![
            "alsa-lib: recommended".to_string(),
            "glibc: suggested".to_string(),
            "git: recommended".to_string(),
        ];

        assert_eq!(
            remove_required_optional(&required, optional),
            vec!["git: recommended"]
        );
    }
}
