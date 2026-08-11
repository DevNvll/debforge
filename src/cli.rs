use std::env;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use crate::error::{AppError, Result};
use crate::scripts::ScriptPolicy;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PkgbuildMode {
    None,
    Also,
    Only,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvertOptions {
    pub input: PathBuf,
    pub output_dir: Option<PathBuf>,
    pub compression_level: u8,
    pub quiet: bool,
    pub force: bool,
    pub keep_work: bool,
    pub generate_mtree: bool,
    pub wipe_versions: bool,
    pub pseudo_64: bool,
    pub pkgbuild: PkgbuildMode,
    pub script_policy: ScriptPolicy,
    pub packager: String,
    pub source_date_epoch: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOptions {
    pub input: PathBuf,
    pub assume_yes: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Convert(ConvertOptions),
    Install(InstallOptions),
    RegisterHandler,
    Help,
    Version,
    Update,
}

pub fn parse() -> Result<Action> {
    parse_from(env::args_os().skip(1))
}

pub fn parse_from<I, S>(arguments: I) -> Result<Action>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let arguments: Vec<OsString> = arguments.into_iter().map(Into::into).collect();
    if arguments.first().and_then(|value| value.to_str()) == Some("install") {
        return parse_install(arguments.into_iter().skip(1));
    }
    if arguments.first().and_then(|value| value.to_str()) == Some("register-handler") {
        if arguments.len() != 1 {
            return Err(AppError::new(
                "register-handler does not accept additional arguments.",
            ));
        }
        return Ok(Action::RegisterHandler);
    }

    let mut input: Option<PathBuf> = None;
    let mut output_dir = None;
    let mut compression_level = 3_u8;
    let mut quiet = false;
    let mut force = false;
    let mut keep_work = false;
    let mut generate_mtree = true;
    let mut wipe_versions = false;
    let mut pseudo_64 = false;
    let mut pkgbuild = PkgbuildMode::None;
    let mut script_policy = ScriptPolicy::Safe;
    let mut packager = env::var("PACKAGER")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "debtap-rs local conversion".to_string());
    let mut source_date_epoch = env::var("SOURCE_DATE_EPOCH")
        .ok()
        .map(|value| parse_u64("SOURCE_DATE_EPOCH", &value))
        .transpose()?;
    let mut positional_only = false;
    let mut requested_action: Option<Action> = None;
    let mut index = 0;

    while index < arguments.len() {
        let argument = &arguments[index];

        if positional_only || !is_option(argument) {
            set_input(&mut input, argument)?;
            index += 1;
            continue;
        }

        if argument == "--" {
            positional_only = true;
            index += 1;
            continue;
        }

        let text = argument
            .to_str()
            .ok_or_else(|| AppError::new("An option contains text that is not valid UTF-8."))?;

        if let Some(value) = text.strip_prefix("--output=") {
            output_dir = Some(nonempty_path("--output", value)?);
        } else if let Some(value) = text.strip_prefix("--compression-level=") {
            compression_level = parse_compression_level(value)?;
        } else if let Some(value) = text.strip_prefix("--scripts=") {
            script_policy = parse_script_policy(value)?;
        } else if let Some(value) = text.strip_prefix("--packager=") {
            packager = nonempty_text("--packager", value)?;
        } else if let Some(value) = text.strip_prefix("--source-date-epoch=") {
            source_date_epoch = Some(parse_u64("--source-date-epoch", value)?);
        } else {
            match text {
                "-h" | "--help" => requested_action = Some(Action::Help),
                "-v" | "--version" => requested_action = Some(Action::Version),
                "-u" | "--update" => requested_action = Some(Action::Update),
                "-q" | "-Q" | "--quiet" | "--Quiet" => quiet = true,
                "-w" | "--wipeout" => wipe_versions = true,
                "-s" | "--pseudo" => pseudo_64 = true,
                "-p" | "--pkgbuild" => pkgbuild = PkgbuildMode::Also,
                "-P" | "--Pkgbuild" => pkgbuild = PkgbuildMode::Only,
                "-o" | "--output" => {
                    index += 1;
                    output_dir = Some(next_path(&arguments, index, text)?);
                }
                "--compression-level" => {
                    index += 1;
                    let value = next_text(&arguments, index, text)?;
                    compression_level = parse_compression_level(value)?;
                }
                "--scripts" => {
                    index += 1;
                    let value = next_text(&arguments, index, text)?;
                    script_policy = parse_script_policy(value)?;
                }
                "--packager" => {
                    index += 1;
                    let value = next_text(&arguments, index, text)?;
                    packager = nonempty_text(text, value)?;
                }
                "--source-date-epoch" => {
                    index += 1;
                    let value = next_text(&arguments, index, text)?;
                    source_date_epoch = Some(parse_u64(text, value)?);
                }
                "--force" => force = true,
                "--keep-work" => keep_work = true,
                "--no-mtree" => generate_mtree = false,
                _ if text.starts_with('-') && !text.starts_with("--") => {
                    parse_short_group(
                        text,
                        &mut quiet,
                        &mut wipe_versions,
                        &mut pseudo_64,
                        &mut pkgbuild,
                    )?;
                }
                _ => return Err(AppError::new(format!("Unknown option: {text}"))),
            }
        }

        index += 1;
    }

    if let Some(action) = requested_action {
        return Ok(action);
    }

    let input = input.ok_or_else(|| {
        AppError::new("No Debian package was given. Use debtap-rs [options] package.deb.")
    })?;

    Ok(Action::Convert(ConvertOptions {
        input,
        output_dir,
        compression_level,
        quiet,
        force,
        keep_work,
        generate_mtree,
        wipe_versions,
        pseudo_64,
        pkgbuild,
        script_policy,
        packager,
        source_date_epoch,
    }))
}

fn parse_install<I>(arguments: I) -> Result<Action>
where
    I: IntoIterator<Item = OsString>,
{
    let mut input = None;
    let mut assume_yes = false;
    let mut positional_only = false;

    for argument in arguments {
        if positional_only || !is_option(&argument) {
            set_input(&mut input, &argument)?;
            continue;
        }

        if argument == "--" {
            positional_only = true;
            continue;
        }

        let text = argument
            .to_str()
            .ok_or_else(|| AppError::new("An install option is not valid UTF-8."))?;
        match text {
            "-y" | "--yes" => assume_yes = true,
            "-h" | "--help" => return Ok(Action::Help),
            _ => return Err(AppError::new(format!("Unknown install option: {text}"))),
        }
    }

    let input = input.ok_or_else(|| {
        AppError::new("No Debian package was given. Use debtap-rs install package.deb.")
    })?;
    Ok(Action::Install(InstallOptions { input, assume_yes }))
}

fn is_option(argument: &OsStr) -> bool {
    argument.as_encoded_bytes().first() == Some(&b'-')
}

fn set_input(input: &mut Option<PathBuf>, value: &OsStr) -> Result<()> {
    if input.is_some() {
        return Err(AppError::new(
            "Only one Debian package can be converted at a time.",
        ));
    }
    *input = Some(PathBuf::from(value));
    Ok(())
}

fn next_path(arguments: &[OsString], index: usize, option: &str) -> Result<PathBuf> {
    arguments
        .get(index)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| AppError::new(format!("{option} needs a path.")))
}

fn next_text<'a>(arguments: &'a [OsString], index: usize, option: &str) -> Result<&'a str> {
    arguments
        .get(index)
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::new(format!("{option} needs a value.")))
}

fn nonempty_path(option: &str, value: &str) -> Result<PathBuf> {
    if value.is_empty() {
        Err(AppError::new(format!("{option} needs a path.")))
    } else {
        Ok(PathBuf::from(value))
    }
}

fn nonempty_text(option: &str, value: &str) -> Result<String> {
    if value.trim().is_empty() {
        Err(AppError::new(format!("{option} needs text.")))
    } else if value.contains('\n') || value.contains('\r') {
        Err(AppError::new(format!(
            "{option} cannot contain a new line."
        )))
    } else {
        Ok(value.trim().to_string())
    }
}

fn parse_compression_level(value: &str) -> Result<u8> {
    let level = value.parse::<u8>().map_err(|error| {
        AppError::new(format!(
            "Invalid Zstandard compression level '{value}': {error}"
        ))
    })?;
    if (1..=19).contains(&level) {
        Ok(level)
    } else {
        Err(AppError::new(
            "The Zstandard compression level must be from 1 through 19.",
        ))
    }
}

fn parse_script_policy(value: &str) -> Result<ScriptPolicy> {
    match value {
        "safe" => Ok(ScriptPolicy::Safe),
        "none" => Ok(ScriptPolicy::None),
        "raw" => Ok(ScriptPolicy::Raw),
        _ => Err(AppError::new(format!(
            "Unknown script policy '{value}'. Use safe, none, or raw."
        ))),
    }
}

fn parse_u64(name: &str, value: &str) -> Result<u64> {
    value
        .parse::<u64>()
        .map_err(|error| AppError::new(format!("Invalid {name} value '{value}': {error}")))
}

fn parse_short_group(
    text: &str,
    quiet: &mut bool,
    wipe_versions: &mut bool,
    pseudo_64: &mut bool,
    pkgbuild: &mut PkgbuildMode,
) -> Result<()> {
    for option in text.trim_start_matches('-').chars() {
        match option {
            'q' | 'Q' => *quiet = true,
            'w' => *wipe_versions = true,
            's' => *pseudo_64 = true,
            'p' => *pkgbuild = PkgbuildMode::Also,
            'P' => *pkgbuild = PkgbuildMode::Only,
            _ => {
                return Err(AppError::new(format!(
                    "Unknown short option '-{option}' in '{text}'."
                )));
            }
        }
    }
    Ok(())
}

pub fn help() -> &'static str {
    "debtap-rs - fast Debian-to-Arch package converter\n\
\n\
Usage:\n\
  debtap-rs [options] package.deb\n\
  debtap-rs install [--yes] package.deb\n\
  debtap-rs register-handler\n\
  debtap-rs --update\n\
\n\
Compatible options:\n\
  -o, --output DIR          Write output to DIR\n\
  -q, --quiet               Show only warnings and the result\n\
  -Q, --Quiet               Alias for --quiet\n\
  -w, --wipeout             Remove Debian version limits\n\
  -s, --pseudo              Mark an i686 package for x86_64 compatibility\n\
  -p, --pkgbuild            Also generate a PKGBUILD\n\
  -P, --Pkgbuild            Generate a PKGBUILD only\n\
  -u, --update              Check the local mapping and package catalog\n\
  -v, --version             Show the version\n\
  -h, --help                Show this help\n\
\n\
Additional options:\n\
      --compression-level N Use Zstandard level 1 through 19 (default: 3)\n\
      --scripts POLICY      Use safe, none, or raw maintainer scripts\n\
      --packager TEXT       Set the Arch packager field\n\
      --source-date-epoch N Set a deterministic build time\n\
      --no-mtree            Do not add .MTREE\n\
      --keep-work           Keep the private work directory\n\
      --force               Replace an existing output package\n\
\n\
Install commands:\n\
  install FILE              Convert, review, authorize, and install FILE\n\
  install --yes FILE        Skip the text confirmation (authorization remains)\n\
  register-handler          Make Debforge the default .deb file handler\n"
}

#[cfg(test)]
mod tests {
    use super::{Action, PkgbuildMode, parse_from};
    use crate::scripts::ScriptPolicy;

    #[test]
    fn parses_desktop_handler_arguments() {
        let action = parse_from(["--Quiet", "--output", "/tmp/out", "/tmp/app name.deb"])
            .expect("arguments must parse");

        let Action::Convert(options) = action else {
            panic!("conversion action expected");
        };
        assert!(options.quiet);
        assert_eq!(
            options.output_dir.expect("output"),
            PathBuf::from("/tmp/out")
        );
        assert_eq!(options.input, PathBuf::from("/tmp/app name.deb"));
    }

    #[test]
    fn parses_new_options_and_compatible_group() {
        let action = parse_from([
            "-Qwp",
            "--compression-level=7",
            "--scripts=none",
            "--force",
            "app.deb",
        ])
        .expect("arguments must parse");

        let Action::Convert(options) = action else {
            panic!("conversion action expected");
        };
        assert!(options.quiet);
        assert!(options.wipe_versions);
        assert!(options.force);
        assert_eq!(options.pkgbuild, PkgbuildMode::Also);
        assert_eq!(options.compression_level, 7);
        assert_eq!(options.script_policy, ScriptPolicy::None);
    }

    #[test]
    fn rejects_more_than_one_input() {
        let error = parse_from(["one.deb", "two.deb"]).expect_err("must reject inputs");
        assert!(error.to_string().contains("Only one"));
    }

    #[test]
    fn rejects_bad_compression_level() {
        let error = parse_from(["--compression-level", "20", "one.deb"])
            .expect_err("must reject the level");
        assert!(error.to_string().contains("1 through 19"));
    }

    #[test]
    fn parses_install_and_handler_actions() {
        let action = parse_from(["install", "--yes", "/tmp/app name.deb"])
            .expect("install arguments must parse");
        let Action::Install(options) = action else {
            panic!("install action expected");
        };
        assert!(options.assume_yes);
        assert_eq!(options.input, PathBuf::from("/tmp/app name.deb"));

        assert_eq!(
            parse_from(["register-handler"]).expect("handler action"),
            Action::RegisterHandler
        );
    }

    #[test]
    fn rejects_unknown_install_options_and_extra_handler_arguments() {
        let error = parse_from(["install", "--force", "app.deb"])
            .expect_err("install must reject converter options");
        assert!(error.to_string().contains("Unknown install option"));

        let error = parse_from(["register-handler", "extra"])
            .expect_err("handler action must reject arguments");
        assert!(error.to_string().contains("does not accept"));
    }

    use std::path::PathBuf;
}
