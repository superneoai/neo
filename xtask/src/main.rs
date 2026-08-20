use cargo_platform::{Cfg, Platform};
use plist::Value;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Cursor, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const EXPECTED_COPYRIGHT: &str = "Copyright © 2026 ACTUAL LTD.";
const PACKAGER_VERSION: &str = "cargo-packager 0.11.8";
const ABOUT_VERSION: &str = "cargo-about 0.8.2";
const NOTICES: &str = "packaging/generated/THIRD-PARTY-NOTICES.md";
const BUNDLED_NOTICES: &str = "Contents/Resources/Legal/THIRD-PARTY-NOTICES.md";
const BUNDLED_LICENSE: &str = "Contents/Resources/Legal/AGPL-3.0-or-later.txt";
const APP: &str = "dist/NEO.app";
const ENTITLEMENTS: &str = "packaging/NEO.entitlements";
const SIGNING_IDENTITY_ENV: &str = "NEO_SIGNING_IDENTITY";
const NOTARY_PROFILE: &str = "SUPERNEO_NOTARY";
const NOTARY_ARCHIVE: &str = "dist/NEO.zip";

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        let mut source = error.source();
        while let Some(cause) = source {
            eprintln!("caused by: {cause}");
            source = cause.source();
        }
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    match (
        arguments.next().as_deref(),
        arguments.next().as_deref(),
        arguments.next(),
    ) {
        (Some(command), None, None) if command == "package" => package(PackageProfile::Release),
        (Some(command), Some(option), None) if command == "package" && option == "--debug" => {
            package(PackageProfile::Debug)
        }
        (Some(command), None, None) if command == "sign" => sign(),
        (Some(command), None, None) if command == "notarize" => notarize(),
        _ => Err(failure(
            "usage: cargo xtask <package [--debug]|sign|notarize>",
        )),
    }
}

#[derive(Clone, Copy)]
enum PackageProfile {
    Release,
    Debug,
}

impl PackageProfile {
    fn apply(self, command: &mut Command) {
        if matches!(self, Self::Release) {
            command.arg("--release");
        }
    }
}

struct ReleaseBuildEnvironment {
    home: PathBuf,
    root: PathBuf,
    cargo_home: PathBuf,
    cargo_home_alias: PathBuf,
    target: PathBuf,
    target_alias: PathBuf,
    path: OsString,
    rustflags_config: String,
}

impl ReleaseBuildEnvironment {
    fn new(root: &Path) -> Result<Self> {
        let home = std::env::var_os("HOME").ok_or_else(|| failure("HOME is not set"))?;
        let cargo_home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(&home).join(".cargo"));
        reject_target_rustflags(root, &cargo_home)?;
        reject_release_profile_overrides(root, &cargo_home)?;
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("target"));
        fs::create_dir_all(&target)?;
        let cargo_home = fs::canonicalize(cargo_home)?;
        let target = fs::canonicalize(target)?;
        let alias_directory = root.join(".neo-release");
        fs::create_dir_all(&alias_directory)?;
        let cargo_home_alias = alias_directory.join("cargo");
        let target_alias = alias_directory.join("target");
        ensure_symlink(&cargo_home, &cargo_home_alias)?;
        ensure_symlink(&target, &target_alias)?;
        let wrapper_directory = alias_directory.join("bin");
        prepare_xcrun_wrapper(&wrapper_directory)?;
        let mut path = vec![wrapper_directory];
        if let Some(existing) = std::env::var_os("PATH") {
            path.extend(std::env::split_paths(&existing));
        }
        let path = std::env::join_paths(path)?;

        let mut rustflags = ambient_release_rustflags();
        reject_profile_codegen_rustflags(&rustflags)?;
        for (path, replacement) in [
            (Path::new(&home), "/build"),
            (root, "/source"),
            (&cargo_home_alias, "/cargo"),
            (&target_alias, "/target"),
        ] {
            let mut flag = OsString::from("--remap-path-prefix=");
            flag.push(path);
            flag.push("=");
            flag.push(replacement);
            rustflags.push(flag);
        }
        let rustflags = rustflags
            .into_iter()
            .map(|flag| {
                flag.into_string()
                    .map(toml::Value::String)
                    .map_err(|_| failure("release rustflags must contain valid Unicode"))
            })
            .collect::<Result<Vec<_>>>()?;
        let rustflags_config = format!("build.rustflags={}", toml::Value::Array(rustflags));

        Ok(Self {
            home: PathBuf::from(home),
            root: root.to_path_buf(),
            cargo_home,
            cargo_home_alias,
            target,
            target_alias,
            path,
            rustflags_config,
        })
    }

    fn apply(&self, command: &mut Command) {
        command
            .args(["--config", &self.rustflags_config])
            .env("CARGO_HOME", &self.cargo_home_alias)
            .env("CARGO_TARGET_DIR", &self.target_alias)
            .env("NEO_RELEASE_HOME", &self.home)
            .env("NEO_RELEASE_ROOT", &self.root)
            .env("NEO_RELEASE_CARGO_HOME", &self.cargo_home)
            .env("NEO_RELEASE_CARGO_ALIAS", &self.cargo_home_alias)
            .env("NEO_RELEASE_TARGET", &self.target)
            .env("NEO_RELEASE_TARGET_ALIAS", &self.target_alias)
            .env("PATH", &self.path)
            .env_remove("CARGO_BUILD_RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("RUSTFLAGS");
    }
}

fn ambient_release_rustflags() -> Vec<OsString> {
    let encoded = std::env::var_os("CARGO_ENCODED_RUSTFLAGS");
    let plain = std::env::var_os("RUSTFLAGS");
    release_rustflags(encoded.as_deref(), plain.as_deref())
}

fn release_rustflags(encoded: Option<&OsStr>, plain: Option<&OsStr>) -> Vec<OsString> {
    let (flags, separator) = match encoded {
        Some(flags) => (Some(flags), 0x1f),
        None => (plain, b' '),
    };
    flags
        .into_iter()
        .flat_map(|flags| flags.as_bytes().split(move |byte| *byte == separator))
        .filter(|flag| !flag.is_empty())
        .map(|flag| OsStr::from_bytes(flag).to_os_string())
        .collect()
}

fn reject_profile_codegen_rustflags(rustflags: &[OsString]) -> Result<()> {
    let mut rustflags = rustflags.iter();
    while let Some(flag) = rustflags.next() {
        let bytes = flag.as_bytes();
        let option = if bytes == b"-C" {
            rustflags.next().map(|option| option.as_bytes())
        } else {
            bytes
                .strip_prefix(b"-C")
                .map(|option| option.strip_prefix(b"=").unwrap_or(option))
        };
        let Some(option) = option else {
            continue;
        };
        let name = option
            .split(|byte| *byte == b'=')
            .next()
            .unwrap_or_default();
        if matches!(
            name,
            b"opt-level" | b"debug-assertions" | b"overflow-checks"
        ) {
            return Err(failure(format!(
                "release rustflags cannot set {}; remove this profile-owned codegen option",
                String::from_utf8_lossy(name)
            )));
        }
    }
    Ok(())
}

struct TargetContext {
    triple: String,
    cfg: Vec<Cfg>,
}

impl TargetContext {
    fn host() -> Result<Self> {
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
        let version = command_output(Command::new(&rustc).arg("-vV"))?;
        let version = String::from_utf8(version.stdout)?;
        let triple = version
            .lines()
            .find_map(|line| line.strip_prefix("host: "))
            .filter(|triple| !triple.is_empty())
            .ok_or_else(|| failure("rustc -vV output has no host triple"))?
            .to_owned();
        let cfg = command_output(Command::new(&rustc).args(["--print", "cfg"]))?;
        let cfg = String::from_utf8(cfg.stdout)?
            .lines()
            .map(str::parse)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Self { triple, cfg })
    }
}

fn reject_target_rustflags(root: &Path, cargo_home: &Path) -> Result<()> {
    reject_target_rustflags_for(root, cargo_home, &TargetContext::host()?)
}

fn reject_target_rustflags_for(
    root: &Path,
    cargo_home: &Path,
    target: &TargetContext,
) -> Result<()> {
    let rustflags_environment = [
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTFLAGS",
        "CARGO_BUILD_RUSTFLAGS",
    ]
    .into_iter()
    .find(|name| std::env::var_os(name).is_some());
    if rustflags_environment == Some("CARGO_BUILD_RUSTFLAGS") {
        return Err(failure(
            "CARGO_BUILD_RUSTFLAGS conflicts with release rustflags; pass supported flags through RUSTFLAGS or CARGO_ENCODED_RUSTFLAGS",
        ));
    }
    let target_environment = format!(
        "CARGO_TARGET_{}_RUSTFLAGS",
        target.triple.replace('-', "_").to_ascii_uppercase()
    );
    if std::env::var_os(&target_environment).is_some() {
        return Err(failure(format!(
            "target-specific rustflags environment variable {target_environment} overrides release path remapping; pass those flags through RUSTFLAGS or CARGO_ENCODED_RUSTFLAGS"
        )));
    }

    let mut paths = root
        .ancestors()
        .flat_map(|directory| {
            [
                directory.join(".cargo/config.toml"),
                directory.join(".cargo/config"),
            ]
        })
        .collect::<Vec<_>>();
    paths.extend([cargo_home.join("config.toml"), cargo_home.join("config")]);
    paths.sort();
    paths.dedup();
    for path in paths {
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "cannot read Cargo configuration {}: {error}",
                        path.display()
                    ),
                )
                .into());
            }
        };
        let configuration = contents.parse::<toml::Value>().map_err(|error| {
            failure(format!(
                "cannot parse Cargo configuration {}: {error}",
                path.display()
            ))
        })?;
        reject_build_rustflags(&configuration, &path)?;
        let Some(targets) = configuration.get("target").and_then(toml::Value::as_table) else {
            continue;
        };
        for (selector, settings) in targets {
            if !settings
                .as_table()
                .is_some_and(|settings| settings.contains_key("rustflags"))
            {
                continue;
            }
            let platform = selector.parse::<Platform>().map_err(|error| {
                failure(format!(
                    "cannot evaluate target selector {selector:?} in {}: {error}",
                    path.display()
                ))
            })?;
            let applies = platform.matches(&target.triple, &target.cfg);
            if applies {
                return Err(failure(format!(
                    "target.{selector}.rustflags in {} overrides release path remapping; pass those flags through RUSTFLAGS or CARGO_ENCODED_RUSTFLAGS",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

fn reject_build_rustflags(configuration: &toml::Value, path: &Path) -> Result<()> {
    let has_build_rustflags = configuration
        .get("build")
        .and_then(toml::Value::as_table)
        .is_some_and(|build| build.contains_key("rustflags"));
    if has_build_rustflags {
        return Err(failure(format!(
            "build.rustflags in {} conflicts with release rustflags; pass supported flags through RUSTFLAGS or CARGO_ENCODED_RUSTFLAGS",
            path.display()
        )));
    }
    Ok(())
}

fn reject_release_profile_overrides(root: &Path, cargo_home: &Path) -> Result<()> {
    let mut environment_overrides = std::env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| name.as_bytes().starts_with(b"CARGO_PROFILE_RELEASE_"))
        .collect::<Vec<_>>();
    environment_overrides.sort();
    if let Some(name) = environment_overrides.first() {
        return Err(failure(format!(
            "release profile override environment variable {} is unsupported",
            name.to_string_lossy()
        )));
    }

    let manifest = root.join("Cargo.toml");
    if let Some(configuration) = read_toml_file(&manifest, "Cargo manifest")? {
        reject_release_profile_keys(&configuration, &manifest)?;
    }
    let mut paths = root
        .ancestors()
        .flat_map(|directory| {
            [
                directory.join(".cargo/config.toml"),
                directory.join(".cargo/config"),
            ]
        })
        .collect::<Vec<_>>();
    paths.extend([cargo_home.join("config.toml"), cargo_home.join("config")]);
    paths.sort();
    paths.dedup();
    for path in paths {
        if let Some(configuration) = read_toml_file(&path, "Cargo configuration")? {
            reject_release_profile_keys(&configuration, &path)?;
        }
    }
    Ok(())
}

fn reject_release_profile_keys(configuration: &toml::Value, path: &Path) -> Result<()> {
    let Some(release) = configuration
        .get("profile")
        .and_then(|profile| profile.get("release"))
        .and_then(toml::Value::as_table)
    else {
        return Ok(());
    };
    if let Some(key) = release.keys().next() {
        return Err(failure(format!(
            "profile.release.{key} in {} is unsupported for release packaging",
            path.display()
        )));
    }
    Ok(())
}

fn read_toml_file(path: &Path, description: &str) -> Result<Option<toml::Value>> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("cannot read {description} {}: {error}", path.display()),
            )
            .into());
        }
    };
    contents.parse::<toml::Value>().map(Some).map_err(|error| {
        failure(format!(
            "cannot parse {description} {}: {error}",
            path.display()
        ))
    })
}

fn prepare_xcrun_wrapper(directory: &Path) -> Result<()> {
    const WRAPPER: &str = r#"#!/bin/sh
if [ "$#" -ge 3 ] && [ "$1" = "-sdk" ] && [ "$3" = "metal" ]; then
    exec /usr/bin/xcrun "$@" \
        "-ffile-prefix-map=$NEO_RELEASE_ROOT=/source" \
        "-fdebug-prefix-map=$NEO_RELEASE_ROOT=/source" \
        "-ffile-prefix-map=$NEO_RELEASE_HOME=/build" \
        "-fdebug-prefix-map=$NEO_RELEASE_HOME=/build" \
        "-ffile-prefix-map=$NEO_RELEASE_CARGO_HOME=/cargo" \
        "-fdebug-prefix-map=$NEO_RELEASE_CARGO_HOME=/cargo" \
        "-ffile-prefix-map=$NEO_RELEASE_CARGO_ALIAS=/cargo" \
        "-fdebug-prefix-map=$NEO_RELEASE_CARGO_ALIAS=/cargo" \
        "-ffile-prefix-map=$NEO_RELEASE_TARGET=/target" \
        "-fdebug-prefix-map=$NEO_RELEASE_TARGET=/target" \
        "-ffile-prefix-map=$NEO_RELEASE_TARGET_ALIAS=/target" \
        "-fdebug-prefix-map=$NEO_RELEASE_TARGET_ALIAS=/target"
fi
if [ "$#" -eq 6 ] && [ "$1" = "-sdk" ] && [ "$3" = "metallib" ] && [ "$5" = "-o" ] && [ "$(dirname "$4")" = "$(dirname "$6")" ]; then
    directory=$(dirname "$6") || exit 1
    input=$(basename "$4") || exit 1
    output=$(basename "$6") || exit 1
    cd "$directory" || exit 1
    exec /usr/bin/xcrun -sdk "$2" metallib "$input" -o "$output"
fi
exec /usr/bin/xcrun "$@"
"#;
    fs::create_dir_all(directory)?;
    let path = directory.join("xcrun");
    let replace = match fs::read(&path) {
        Ok(contents) => contents != WRAPPER.as_bytes(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("cannot read xcrun wrapper {}: {error}", path.display()),
            )
            .into());
        }
    };
    if replace {
        fs::write(&path, WRAPPER).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot write xcrun wrapper {}: {error}", path.display()),
            )
        })?;
    }
    let mut permissions = fs::metadata(&path)
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot inspect xcrun wrapper {}: {error}", path.display()),
            )
        })?
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "cannot make xcrun wrapper executable {}: {error}",
                path.display()
            ),
        )
    })?;
    Ok(())
}

fn ensure_symlink(target: &Path, alias: &Path) -> Result<()> {
    match fs::symlink_metadata(alias) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            if fs::read_link(alias)? != target {
                fs::remove_file(alias)?;
                symlink(target, alias)?;
            }
        }
        Ok(_) => {
            return Err(failure(format!(
                "release build alias is not a symlink: {}",
                alias.display()
            )));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => symlink(target, alias)?,
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn package(profile: PackageProfile) -> Result<()> {
    let root = repository_root();
    std::env::set_current_dir(&root)?;
    reject_packager_signing_identity(&root)?;
    require_tool_version("packager", PACKAGER_VERSION)?;
    require_tool_version("about", ABOUT_VERSION)?;
    generate_notices()?;
    let build_environment = match profile {
        PackageProfile::Release => Some(ReleaseBuildEnvironment::new(&root)?),
        PackageProfile::Debug => None,
    };
    let release_executable = if let Some(environment) = &build_environment {
        Some(run_release_build(environment)?)
    } else {
        let mut build = Command::new("cargo");
        build.args(["build", "--locked", "--package", "neo"]);
        run_command(&mut build)?;
        None
    };
    let mut packager = Command::new("cargo");
    if let Some(environment) = &build_environment {
        environment.apply(&mut packager);
    }
    packager.args(["packager", "--packages", "neo"]);
    profile.apply(&mut packager);
    run_command(&mut packager)?;

    let app = root.join(APP);
    if !app.is_dir() {
        return Err(failure(format!(
            "cargo-packager did not create {}",
            app.display()
        )));
    }

    remove_carbon_key(&app.join("Contents/Info.plist"))?;
    let attribution_count = verify_bundle(&root, &app, release_executable.as_deref())?;
    println!(
        "verified {} with {attribution_count} crate attributions",
        app.display()
    );
    Ok(())
}

fn run_release_build(environment: &ReleaseBuildEnvironment) -> Result<PathBuf> {
    let mut command = Command::new("cargo");
    environment.apply(&mut command);
    command.args([
        "build",
        "--locked",
        "--package",
        "neo",
        "--release",
        "--message-format=json-render-diagnostics",
    ]);
    let display = display_command(&command);
    let output = command.output()?;
    io::stderr().write_all(&output.stderr)?;
    if !output.status.success() {
        io::stdout().write_all(&output.stdout)?;
        return Err(failure(format!("{display} exited with {}", output.status)));
    }
    release_executable_from_messages(&output.stdout)
}

fn release_executable_from_messages(messages: &[u8]) -> Result<PathBuf> {
    let mut artifact = None;
    for line in messages
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let message: serde_json::Value = serde_json::from_slice(line)
            .map_err(|error| failure(format!("invalid Cargo JSON message: {error}")))?;
        let is_neo_bin = message.get("reason").and_then(serde_json::Value::as_str)
            == Some("compiler-artifact")
            && message
                .pointer("/target/name")
                .and_then(serde_json::Value::as_str)
                == Some("neo")
            && message
                .pointer("/target/kind")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|kinds| kinds.iter().any(|kind| kind.as_str() == Some("bin")));
        if !is_neo_bin {
            continue;
        }
        let opt_level = message
            .pointer("/profile/opt_level")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| failure("neo compiler artifact has no profile.opt_level"))?;
        let debug_assertions = message
            .pointer("/profile/debug_assertions")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| failure("neo compiler artifact has no profile.debug_assertions"))?;
        let overflow_checks = message
            .pointer("/profile/overflow_checks")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| failure("neo compiler artifact has no profile.overflow_checks"))?;
        if opt_level != "3" || debug_assertions || overflow_checks {
            return Err(failure(format!(
                "neo compiler artifact has unexpected release profile: opt_level={opt_level:?}, debug_assertions={debug_assertions}, overflow_checks={overflow_checks}"
            )));
        }
        let executable = message
            .get("executable")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| failure("neo compiler artifact has no executable path"))?;
        artifact = Some(PathBuf::from(executable));
        println!(
            "verified release compiler artifact profile: opt_level={opt_level:?}, debug_assertions={debug_assertions}, overflow_checks={overflow_checks}; executable={executable}"
        );
    }
    artifact.ok_or_else(|| failure("Cargo emitted no compiler artifact for the neo bin target"))
}

fn configured_release_executable(root: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"))
        .join("release/neo")
}

fn sign() -> Result<()> {
    let root = repository_root();
    std::env::set_current_dir(&root)?;
    let app = root.join(APP);
    if !app.is_dir() {
        return Err(failure(format!("missing app bundle: {}", app.display())));
    }
    let release_executable = configured_release_executable(&root);
    let attribution_count = verify_bundle(&root, &app, Some(&release_executable))?;
    println!(
        "verified {} with {attribution_count} crate attributions",
        app.display()
    );
    let identity = signing_identity()?;
    let entitlements = root.join(ENTITLEMENTS);
    if !entitlements.is_file() {
        return Err(failure(format!(
            "missing entitlements file: {}",
            entitlements.display()
        )));
    }

    let nested = nested_code(&app)?;
    println!("found {} nested code items", nested.len());
    for item in nested {
        sign_item(&item, &identity, &entitlements)?;
    }
    sign_item(&app, &identity, &entitlements)?;
    run_command(
        Command::new("codesign")
            .args(["--verify", "--deep", "--strict", "--verbose=2"])
            .arg(&app),
    )?;
    let signature = command_output(Command::new("codesign").arg("-dvvv").arg(&app))?;
    let signature_details = format!(
        "{}{}",
        String::from_utf8_lossy(&signature.stdout),
        String::from_utf8_lossy(&signature.stderr)
    );
    validate_signature_details(&signature_details)?;

    let assessment = Command::new("spctl")
        .args(["-a", "-vvv", "-t", "install"])
        .arg(&app)
        .output()?
        .status;
    if assessment.success() {
        println!("Gatekeeper accepted the signed app before notarization");
    } else {
        println!("Gatekeeper assessment remains pending notarization: {assessment}");
    }
    Ok(())
}

fn signing_identity() -> Result<String> {
    if let Some(identity) = std::env::var_os(SIGNING_IDENTITY_ENV) {
        let identity = identity
            .into_string()
            .map_err(|_| failure(format!("{SIGNING_IDENTITY_ENV} must contain valid Unicode")))?;
        if identity.trim().is_empty() {
            return Err(failure(format!("{SIGNING_IDENTITY_ENV} is empty")));
        }
        validate_signing_identity(&identity)?;
        println!(
            "using signing identity from {SIGNING_IDENTITY_ENV}: {}",
            signing_identity_label(&identity)
        );
        return Ok(identity);
    }

    let output = command_output(Command::new("security").args([
        "find-identity",
        "-v",
        "-p",
        "codesigning",
    ]))?;
    let stdout = String::from_utf8(output.stdout)?;
    let identities = stdout
        .lines()
        .filter(|line| line.contains("\"Developer ID Application:"))
        .filter_map(|line| {
            let start = line.find('"')? + 1;
            let end = line.rfind('"')?;
            (end > start).then(|| line[start..end].to_owned())
        })
        .collect::<Vec<_>>();
    match identities.as_slice() {
        [identity] => {
            println!(
                "selected signing identity: {}",
                signing_identity_label(identity)
            );
            Ok(identity.clone())
        }
        [] => Err(failure(format!(
            "no Developer ID Application identity found; set {SIGNING_IDENTITY_ENV} to override"
        ))),
        _ => Err(failure(format!(
            "multiple Developer ID Application identities found; set {SIGNING_IDENTITY_ENV} to choose one"
        ))),
    }
}

fn validate_signing_identity(identity: &str) -> Result<()> {
    if !identity.starts_with("Developer ID Application:") {
        return Err(failure(format!(
            "{SIGNING_IDENTITY_ENV} must start with Developer ID Application:"
        )));
    }
    Ok(())
}

fn signing_identity_label(identity: &str) -> &str {
    identity
        .strip_suffix(')')
        .and_then(|identity| identity.rsplit_once(" (").map(|(label, _)| label))
        .unwrap_or(identity)
}

fn validate_signature_details(details: &str) -> Result<()> {
    let has_developer_id_authority = details.lines().any(|line| {
        line.trim_start()
            .starts_with("Authority=Developer ID Application:")
    });
    if !has_developer_id_authority {
        return Err(failure(
            "codesign output has no Developer ID Application authority",
        ));
    }
    let has_hardened_runtime = details.lines().any(|line| {
        line.split_ascii_whitespace()
            .any(|field| field.starts_with("flags=") && field.contains("runtime"))
    });
    if !has_hardened_runtime {
        return Err(failure("codesign output has no hardened runtime flag"));
    }
    Ok(())
}

fn nested_code(app: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    collect_nested_code(app, app, &mut paths)?;
    paths.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    Ok(paths)
}

fn collect_nested_code(app: &Path, directory: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_nested_code(app, &path, paths)?;
            if path != app && is_code_bundle(&path) {
                paths.push(path);
            }
        } else if file_type.is_file() && is_nested_executable(app, &path)? {
            paths.push(path);
        }
    }
    Ok(())
}

fn is_code_bundle(path: &Path) -> bool {
    matches!(
        path.extension().and_then(OsStr::to_str),
        Some("app" | "appex" | "framework" | "plugin" | "xpc")
    )
}

fn is_nested_executable(app: &Path, path: &Path) -> Result<bool> {
    let relative = path.strip_prefix(app)?;
    if relative.parent() == Some(Path::new("Contents/MacOS")) {
        return Ok(false);
    }
    let extension = path.extension().and_then(OsStr::to_str);
    if matches!(extension, Some("dylib" | "so")) {
        return Ok(true);
    }
    let in_code_directory = [
        "Contents/Frameworks",
        "Contents/Helpers",
        "Contents/Library/LoginItems",
        "Contents/PlugIns",
        "Contents/XPCServices",
    ]
    .iter()
    .any(|directory| relative.starts_with(directory));
    Ok(in_code_directory && fs::metadata(path)?.permissions().mode() & 0o111 != 0)
}

fn sign_item(path: &Path, identity: &str, entitlements: &Path) -> Result<()> {
    println!("signing {}", path.display());
    let label = signing_identity_label(identity);
    run_command_redacted(
        Command::new("codesign")
            .args([
                "--force",
                "--sign",
                identity,
                "--options",
                "runtime",
                "--timestamp",
                "--entitlements",
            ])
            .arg(entitlements)
            .arg(path),
        &[(OsStr::new(identity), OsStr::new(label))],
    )
}

fn notarize() -> Result<()> {
    let root = repository_root();
    std::env::set_current_dir(&root)?;
    let app = root.join(APP);
    if !app.is_dir() {
        return Err(failure(format!("missing app bundle: {}", app.display())));
    }
    let release_executable = configured_release_executable(&root);
    let attribution_count = verify_bundle(&root, &app, Some(&release_executable))?;
    println!(
        "verified {} with {attribution_count} crate attributions",
        app.display()
    );
    run_command(
        Command::new("codesign")
            .args(["--verify", "--deep", "--strict", "--verbose=2"])
            .arg(&app),
    )?;

    let archive = root.join(NOTARY_ARCHIVE);
    if archive.exists() {
        fs::remove_file(&archive)?;
    }
    run_command(
        Command::new("ditto")
            .args(["-c", "-k", "--keepParent"])
            .arg(&app)
            .arg(&archive),
    )?;

    let output = Command::new("xcrun")
        .args(["notarytool", "submit"])
        .arg(&archive)
        .args([
            "--keychain-profile",
            NOTARY_PROFILE,
            "--wait",
            "--output-format",
            "plist",
        ])
        .output()?;
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    let submission = Value::from_reader(Cursor::new(&output.stdout))?;
    let submission_id = submission_field(&submission, "id")?;
    let status = submission_field(&submission, "status")?;
    println!("notarization submission {submission_id}: {status}");
    if !output.status.success() || status != "Accepted" {
        let _ = run_command(Command::new("xcrun").args([
            "notarytool",
            "log",
            submission_id,
            "--keychain-profile",
            NOTARY_PROFILE,
        ]));
        return Err(failure(format!(
            "notarization submission {submission_id} finished with {status}"
        )));
    }

    run_command(Command::new("xcrun").args(["stapler", "staple"]).arg(&app))?;
    run_command(
        Command::new("xcrun")
            .args(["stapler", "validate"])
            .arg(&app),
    )?;
    let assessment = Command::new("spctl")
        .args(["-a", "-vvv", "-t", "install"])
        .arg(&app)
        .output()?;
    if !assessment.status.success() {
        return Err(failure(format!(
            "Gatekeeper assessment exited with {}",
            assessment.status
        )));
    }
    Ok(())
}

fn submission_field<'a>(submission: &'a Value, field: &str) -> Result<&'a str> {
    submission
        .as_dictionary()
        .and_then(|dictionary| dictionary.get(field))
        .and_then(Value::as_string)
        .ok_or_else(|| failure(format!("notarytool response has no string {field} field")))
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask must be a workspace member")
        .to_path_buf()
}

fn reject_packager_signing_identity(root: &Path) -> Result<()> {
    let manifest_path = root.join("Cargo.toml");
    let manifest = fs::read_to_string(&manifest_path)?.parse::<toml::Value>()?;
    let signing_identity = manifest
        .get("package")
        .and_then(|package| package.get("metadata"))
        .and_then(|metadata| metadata.get("packager"))
        .and_then(|packager| packager.get("macos"))
        .and_then(|macos| macos.get("signing-identity"));
    if signing_identity.is_some() {
        return Err(failure(format!(
            "remove package.metadata.packager.macos.signing-identity from {}",
            manifest_path.display()
        )));
    }
    Ok(())
}

fn require_tool_version(subcommand: &str, expected: &str) -> Result<()> {
    let output = command_output(Command::new("cargo").args([subcommand, "--version"]))?;
    let actual = String::from_utf8(output.stdout)?.trim().to_owned();
    if actual != expected {
        return Err(failure(format!(
            "{expected} is required, found {actual:?}; install it with `cargo install cargo-{subcommand} --version {} --locked`",
            expected
                .rsplit_once(' ')
                .map_or(expected, |(_, version)| version)
        )));
    }
    Ok(())
}

fn generate_notices() -> Result<()> {
    fs::create_dir_all("packaging/generated")?;
    run_command(Command::new("cargo").args([
        "about",
        "generate",
        "--locked",
        "--fail",
        "-c",
        "packaging/licenses/about.toml",
        "packaging/licenses/notices.md.hbs",
        "-o",
        NOTICES,
    ]))
}

fn remove_carbon_key(path: &Path) -> Result<()> {
    let mut plist = Value::from_file(path)?;
    let dictionary = plist
        .as_dictionary_mut()
        .ok_or_else(|| failure(format!("{} is not a plist dictionary", path.display())))?;
    dictionary.remove("LSRequiresCarbon");
    plist.to_file_xml(path)?;
    Ok(())
}

fn verify_bundle(root: &Path, app: &Path, release_executable: Option<&Path>) -> Result<usize> {
    let plist_path = app.join("Contents/Info.plist");
    let plist = Value::from_file(&plist_path)?;
    let dictionary = plist.as_dictionary().ok_or_else(|| {
        failure(format!(
            "{} is not a plist dictionary",
            plist_path.display()
        ))
    })?;
    let copyright = dictionary
        .get("NSHumanReadableCopyright")
        .and_then(Value::as_string)
        .unwrap_or_default();
    if copyright != EXPECTED_COPYRIGHT {
        return Err(failure(format!("unexpected bundle copyright: {copyright}")));
    }
    if dictionary.contains_key("LSRequiresCarbon") {
        return Err(failure(format!(
            "obsolete LSRequiresCarbon remains in {}",
            plist_path.display()
        )));
    }
    if let Some(release_executable) = release_executable {
        verify_bundled_executable(root, app, dictionary, release_executable)?;
    }

    require_identical(
        &root.join("LICENSE"),
        &app.join(BUNDLED_LICENSE),
        "bundled license differs from LICENSE",
    )?;

    let generated_notices = root.join(NOTICES);
    require_nonempty(&generated_notices, "third-party notices")?;
    let bundled_notices = app.join(BUNDLED_NOTICES);
    require_nonempty(&bundled_notices, "bundled third-party notices")?;
    require_identical(
        &generated_notices,
        &bundled_notices,
        "bundled notices differ from the generated notices",
    )?;

    let notices = fs::read_to_string(&bundled_notices)?;
    for entity in ["&quot;", "&#x27;", "&amp;"] {
        if notices.contains(entity) {
            return Err(failure(format!(
                "bundled notices contain escaped entity {entity}: {}",
                bundled_notices.display()
            )));
        }
    }

    let attribution_count = notices
        .lines()
        .filter(|line| line.starts_with("- ["))
        .count();
    if attribution_count == 0 {
        return Err(failure(format!(
            "third-party notices contain no crate attributions: {}",
            bundled_notices.display()
        )));
    }
    Ok(attribution_count)
}

fn verify_bundled_executable(
    root: &Path,
    app: &Path,
    dictionary: &plist::Dictionary,
    release_executable: &Path,
) -> Result<()> {
    let home = std::env::var_os("HOME").ok_or_else(|| failure("HOME is not set"))?;
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&home).join(".cargo"));
    reject_profile_codegen_rustflags(&ambient_release_rustflags())?;
    reject_target_rustflags(root, &cargo_home)?;
    reject_release_profile_overrides(root, &cargo_home)?;
    let executable_name = dictionary
        .get("CFBundleExecutable")
        .and_then(Value::as_string)
        .filter(|name| Path::new(name).file_name() == Some(OsStr::new(name)))
        .ok_or_else(|| failure("bundle has no valid CFBundleExecutable"))?;
    let executable = app.join("Contents/MacOS").join(executable_name);
    let metadata = fs::metadata(&executable).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "missing bundled executable: {}: {error}",
                executable.display()
            ),
        )
    })?;
    if !metadata.is_file() {
        return Err(failure(format!(
            "bundled executable is not a regular file: {}",
            executable.display()
        )));
    }
    if metadata.len() == 0 {
        return Err(failure(format!(
            "bundled executable is empty: {}",
            executable.display()
        )));
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(failure(format!(
            "bundled executable has no executable permission bits: {}",
            executable.display()
        )));
    }
    let bytes = fs::read(&executable).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "cannot read bundled executable: {}: {error}",
                executable.display()
            ),
        )
    })?;
    let bundled_identity = macho_identity(&bytes).map_err(|error| {
        failure(format!(
            "bundled executable is not a valid thin 64-bit Mach-O: {}: {error}",
            executable.display()
        ))
    })?;

    let release_metadata = fs::metadata(release_executable).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "missing release artifact: {}: {error}; run `cargo xtask package` first",
                release_executable.display()
            ),
        )
    })?;
    if !release_metadata.is_file() || release_metadata.len() == 0 {
        return Err(failure(format!(
            "release artifact is not a non-empty regular file: {}; run `cargo xtask package` first",
            release_executable.display()
        )));
    }
    let release_bytes = fs::read(release_executable).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "cannot read release artifact: {}: {error}",
                release_executable.display()
            ),
        )
    })?;
    let release_identity = macho_identity(&release_bytes).map_err(|error| {
        failure(format!(
            "release artifact is not a valid thin 64-bit Mach-O: {}: {error}; run `cargo xtask package` first",
            release_executable.display()
        ))
    })?;
    if bundled_identity.cpu_type != release_identity.cpu_type {
        return Err(failure(format!(
            "bundled executable CPU type {} does not match release artifact CPU type {}: {} != {}",
            cpu_type_name(bundled_identity.cpu_type),
            cpu_type_name(release_identity.cpu_type),
            executable.display(),
            release_executable.display()
        )));
    }
    // Code signing changes executable bytes and size, while LC_UUID survives cargo-packager's
    // copy and codesign's re-signing.
    if bundled_identity.uuid != release_identity.uuid {
        return Err(failure(format!(
            "bundled executable LC_UUID {} does not match release artifact LC_UUID {}: {} != {}",
            format_uuid(&bundled_identity.uuid),
            format_uuid(&release_identity.uuid),
            executable.display(),
            release_executable.display()
        )));
    }
    println!(
        "matched executable identity: {} UUID {} ({}); {} UUID {} ({})",
        executable.display(),
        format_uuid(&bundled_identity.uuid),
        cpu_type_name(bundled_identity.cpu_type),
        release_executable.display(),
        format_uuid(&release_identity.uuid),
        cpu_type_name(release_identity.cpu_type)
    );

    if home.is_empty() || home == "/" {
        return Err(failure("HOME is not a private path prefix"));
    }
    reject_executable_marker(
        &bytes,
        home.as_bytes(),
        "private home path prefix",
        &executable,
    )?;
    reject_executable_marker(
        &bytes,
        b"/var/folders",
        "Darwin temporary directory path",
        &executable,
    )?;
    let temporary_directory = std::env::temp_dir();
    if let Some(identifier) = darwin_temp_identifier(&temporary_directory) {
        reject_executable_marker(
            &bytes,
            identifier.as_bytes(),
            "Darwin temporary directory identifier",
            &executable,
        )?;
    } else {
        eprintln!(
            "warning: skipped the Darwin temporary directory identifier check because TMPDIR is outside /var/folders: {}",
            temporary_directory.display()
        );
    }
    Ok(())
}

struct MachOIdentity {
    cpu_type: u32,
    uuid: [u8; 16],
}

fn macho_identity(bytes: &[u8]) -> Result<MachOIdentity> {
    match bytes.get(..4) {
        Some([0xcf, 0xfa, 0xed, 0xfe]) => {}
        Some([0xfe, 0xed, 0xfa, 0xcf]) => {
            return Err(failure("byte-swapped Mach-O is unsupported"));
        }
        Some(
            [0xca, 0xfe, 0xba, 0xbe]
            | [0xbe, 0xba, 0xfe, 0xca]
            | [0xca, 0xfe, 0xba, 0xbf]
            | [0xbf, 0xba, 0xfe, 0xca],
        ) => return Err(failure("fat Mach-O is unsupported")),
        Some([0xce, 0xfa, 0xed, 0xfe] | [0xfe, 0xed, 0xfa, 0xce]) => {
            return Err(failure("32-bit Mach-O is unsupported"));
        }
        _ => return Err(failure("no supported Mach-O magic")),
    }
    if bytes.len() < 32 {
        return Err(failure("truncated Mach-O header"));
    }
    let cpu_type = read_macho_u32(bytes, 4)?;
    let command_count = read_macho_u32(bytes, 16)?;
    let command_bytes = read_macho_u32(bytes, 20)? as usize;
    let commands_end = 32usize
        .checked_add(command_bytes)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| failure("truncated Mach-O load command area"))?;
    let mut offset = 32usize;
    let mut uuid = None;
    let mut maximum_file_end = 0u64;
    for _ in 0..command_count {
        if offset.checked_add(8).is_none_or(|end| end > commands_end) {
            return Err(failure("truncated Mach-O load command header"));
        }
        let command = read_macho_u32(bytes, offset)?;
        let size = read_macho_u32(bytes, offset + 4)? as usize;
        if size < 8 {
            return Err(failure("invalid Mach-O load command size"));
        }
        let end = offset
            .checked_add(size)
            .filter(|end| *end <= commands_end)
            .ok_or_else(|| failure("truncated Mach-O load command"))?;
        if command == 0x19 {
            if size < 72 {
                return Err(failure("invalid LC_SEGMENT_64 load command size"));
            }
            let file_offset = read_macho_u64(bytes, offset + 40)?;
            let file_size = read_macho_u64(bytes, offset + 48)?;
            let file_end = file_offset
                .checked_add(file_size)
                .ok_or_else(|| failure("Mach-O segment file range overflows"))?;
            maximum_file_end = maximum_file_end.max(file_end);
        }
        if command == 0x1b {
            if size != 24 {
                return Err(failure("invalid LC_UUID load command size"));
            }
            if uuid.is_some() {
                return Err(failure("duplicate LC_UUID load command"));
            }
            uuid = Some(
                bytes[offset + 8..offset + 24]
                    .try_into()
                    .expect("LC_UUID size was checked"),
            );
        }
        offset = end;
    }
    if offset != commands_end {
        return Err(failure("Mach-O load command sizes do not match header"));
    }
    if maximum_file_end > bytes.len() as u64 {
        return Err(failure("truncated Mach-O segment data"));
    }
    let uuid = uuid.ok_or_else(|| failure("Mach-O has no LC_UUID load command"))?;
    Ok(MachOIdentity { cpu_type, uuid })
}

fn read_macho_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let value: [u8; 4] = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| failure("truncated Mach-O integer"))?
        .try_into()
        .expect("Mach-O integer size was checked");
    Ok(u32::from_le_bytes(value))
}

fn read_macho_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let value: [u8; 8] = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| failure("truncated Mach-O integer"))?
        .try_into()
        .expect("Mach-O integer size was checked");
    Ok(u64::from_le_bytes(value))
}

fn format_uuid(uuid: &[u8; 16]) -> String {
    format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        uuid[0],
        uuid[1],
        uuid[2],
        uuid[3],
        uuid[4],
        uuid[5],
        uuid[6],
        uuid[7],
        uuid[8],
        uuid[9],
        uuid[10],
        uuid[11],
        uuid[12],
        uuid[13],
        uuid[14],
        uuid[15]
    )
}

fn cpu_type_name(cpu_type: u32) -> String {
    match cpu_type {
        0x0100_000c => "arm64".into(),
        0x0100_0007 => "x86_64".into(),
        _ => format!("0x{cpu_type:08X}"),
    }
}

fn darwin_temp_identifier(path: &Path) -> Option<&OsStr> {
    path.starts_with("/var/folders")
        .then(|| path.parent()?.file_name())
        .flatten()
}

fn reject_executable_marker(
    bytes: &[u8],
    marker: &[u8],
    description: &str,
    executable: &Path,
) -> Result<()> {
    if !marker.is_empty() && bytes.windows(marker.len()).any(|window| window == marker) {
        return Err(failure(format!(
            "bundled executable contains {description}: {}",
            executable.display()
        )));
    }
    Ok(())
}

fn require_nonempty(path: &Path, description: &str) -> Result<()> {
    let metadata = fs::metadata(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("missing {description}: {}: {error}", path.display()),
        )
    })?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(failure(format!(
            "{description} is not a non-empty file: {}",
            path.display()
        )));
    }
    Ok(())
}

fn require_identical(expected: &Path, actual: &Path, message: &str) -> Result<()> {
    let expected_bytes = fs::read(expected)?;
    let actual_bytes = fs::read(actual).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("missing {}: {error}", actual.display()),
        )
    })?;
    if expected_bytes != actual_bytes {
        return Err(failure(format!("{message}: {}", actual.display())));
    }
    Ok(())
}

fn run_command(command: &mut Command) -> Result<()> {
    run_command_redacted(command, &[])
}

fn run_command_redacted(command: &mut Command, redactions: &[(&OsStr, &OsStr)]) -> Result<()> {
    let display = display_command_redacted(command, redactions);
    let output = command.output()?;
    io::stdout().write_all(&redact_bytes(&output.stdout, redactions))?;
    io::stderr().write_all(&redact_bytes(&output.stderr, redactions))?;
    if !output.status.success() {
        return Err(failure(format!("{display} exited with {}", output.status)));
    }
    Ok(())
}

fn redact_bytes(bytes: &[u8], redactions: &[(&OsStr, &OsStr)]) -> Vec<u8> {
    let mut result = bytes.to_vec();
    for (secret, replacement) in redactions {
        let secret = secret.as_bytes();
        if secret.is_empty() {
            continue;
        }
        let replacement = replacement.as_bytes();
        let mut redacted = Vec::with_capacity(result.len());
        let mut remainder = result.as_slice();
        while let Some(offset) = remainder
            .windows(secret.len())
            .position(|window| window == secret)
        {
            redacted.extend_from_slice(&remainder[..offset]);
            redacted.extend_from_slice(replacement);
            remainder = &remainder[offset + secret.len()..];
        }
        redacted.extend_from_slice(remainder);
        result = redacted;
    }
    result
}

fn command_output(command: &mut Command) -> Result<Output> {
    let display = display_command(command);
    let output = command.output()?;
    if !output.status.success() {
        return Err(failure(format!(
            "{display} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output)
}

fn display_command(command: &Command) -> String {
    display_command_redacted(command, &[])
}

fn display_command_redacted(command: &Command, redactions: &[(&OsStr, &OsStr)]) -> String {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|argument| {
            redactions
                .iter()
                .find_map(|(secret, replacement)| (argument == *secret).then_some(*replacement))
                .unwrap_or(argument)
                .to_string_lossy()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn failure(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(io::Error::other(message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use plist::Dictionary;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);
    const TEST_UUID: [u8; 16] = [
        0x92, 0x66, 0x81, 0x5d, 0xd7, 0x91, 0x30, 0x64, 0xb8, 0x09, 0x13, 0xb1, 0x4e, 0xec, 0xdc,
        0x2e,
    ];

    fn test_macho(cpu_type: u32, uuid: [u8; 16]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0xfeed_facfu32.to_le_bytes());
        bytes.extend_from_slice(&cpu_type.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&24u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0x1bu32.to_le_bytes());
        bytes.extend_from_slice(&24u32.to_le_bytes());
        bytes.extend_from_slice(&uuid);
        bytes
    }

    fn test_byte_swapped_macho(cpu_type: u32, uuid: [u8; 16]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0xfeed_facfu32.to_be_bytes());
        bytes.extend_from_slice(&cpu_type.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&2u32.to_be_bytes());
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(&24u32.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&0x1bu32.to_be_bytes());
        bytes.extend_from_slice(&24u32.to_be_bytes());
        bytes.extend_from_slice(&uuid);
        bytes
    }

    fn test_macho_with_segment(file_size: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0xfeed_facfu32.to_le_bytes());
        bytes.extend_from_slice(&0x0100_000cu32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&96u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0x19u32.to_le_bytes());
        bytes.extend_from_slice(&72u32.to_le_bytes());
        bytes.extend_from_slice(&[0; 16]);
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&file_size.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0x1bu32.to_le_bytes());
        bytes.extend_from_slice(&24u32.to_le_bytes());
        bytes.extend_from_slice(&TEST_UUID);
        bytes.resize(file_size as usize, 0);
        bytes
    }

    struct Fixture {
        root: PathBuf,
        app: PathBuf,
    }

    impl Fixture {
        fn valid() -> Self {
            let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!("neo-xtask-{}-{id}", std::process::id()));
            let app = root.join("dist/NEO.app");
            fs::create_dir_all(app.join("Contents/Resources/Legal")).unwrap();
            fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
            fs::create_dir_all(root.join("packaging/generated")).unwrap();
            fs::create_dir_all(root.join("target/release")).unwrap();
            fs::write(root.join("LICENSE"), b"license\n").unwrap();
            fs::write(app.join(BUNDLED_LICENSE), b"license\n").unwrap();
            fs::write(root.join(NOTICES), b"# Notices\n\n- [crate 1.0.0](url)\n").unwrap();
            fs::write(
                app.join(BUNDLED_NOTICES),
                b"# Notices\n\n- [crate 1.0.0](url)\n",
            )
            .unwrap();
            let executable = app.join("Contents/MacOS/neo");
            let release = test_macho(0x0100_000c, TEST_UUID);
            fs::write(&executable, &release).unwrap();
            fs::write(root.join("target/release/neo"), release).unwrap();
            let mut permissions = fs::metadata(&executable).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(executable, permissions).unwrap();
            let mut dictionary = Dictionary::new();
            dictionary.insert("CFBundleExecutable".into(), "neo".into());
            dictionary.insert("NSHumanReadableCopyright".into(), EXPECTED_COPYRIGHT.into());
            Value::Dictionary(dictionary)
                .to_file_xml(app.join("Contents/Info.plist"))
                .unwrap();
            Self { root, app }
        }

        fn verify(&self) -> Result<usize> {
            let release_executable = self.release_executable();
            verify_bundle(&self.root, &self.app, Some(&release_executable))
        }

        fn verify_debug(&self) -> Result<usize> {
            verify_bundle(&self.root, &self.app, None)
        }

        fn executable(&self) -> PathBuf {
            self.app.join("Contents/MacOS/neo")
        }

        fn release_executable(&self) -> PathBuf {
            self.root.join("target/release/neo")
        }

        fn append_to_executable(&self, bytes: &[u8]) {
            let mut executable = fs::read(self.executable()).unwrap();
            executable.extend_from_slice(bytes);
            fs::write(self.executable(), executable).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }

    #[test]
    fn accepts_developer_id_signing_identity() {
        assert!(validate_signing_identity("Developer ID Application: Example Corporation").is_ok());
    }

    #[test]
    fn rejects_non_developer_id_signing_identity() {
        assert!(validate_signing_identity("-").is_err());
        assert!(validate_signing_identity("Apple Development: Example Corporation").is_err());
    }

    #[test]
    fn validates_developer_id_signature_details() {
        let valid = "CodeDirectory v=20500 flags=0x10000(runtime) hashes=1+1 location=embedded\nAuthority=Developer ID Application: Example Corporation (TEAMID1234)\n";
        assert!(validate_signature_details(valid).is_ok());

        let ad_hoc = "CodeDirectory v=20400 flags=0x2(adhoc) hashes=1+1 location=embedded\nSignature=adhoc\n";
        assert!(validate_signature_details(ad_hoc).is_err());

        let development = "CodeDirectory v=20500 flags=0x10000(runtime) hashes=1+1 location=embedded\nAuthority=Apple Development: Example Corporation (TEAMID1234)\n";
        assert!(validate_signature_details(development).is_err());

        let without_runtime = "CodeDirectory v=20500 flags=0x0(none) hashes=1+1 location=embedded\nAuthority=Developer ID Application: Example Corporation (TEAMID1234)\n";
        assert!(validate_signature_details(without_runtime).is_err());
    }

    #[test]
    fn redacts_team_id_from_signing_identity() {
        assert_eq!(
            signing_identity_label("Developer ID Application: Example Corporation (TEAMID1234)"),
            "Developer ID Application: Example Corporation"
        );
        assert_eq!(
            signing_identity_label("Developer ID Application: Example Corporation"),
            "Developer ID Application: Example Corporation"
        );
    }

    #[test]
    fn redacts_team_id_from_command_failure() {
        let identity = "Developer ID Application: Nonexistent Corp (ZZZZZZZZZZ)";
        let label = signing_identity_label(identity);
        let error = run_command_redacted(
            Command::new("/usr/bin/false").arg(identity),
            &[(OsStr::new(identity), OsStr::new(label))],
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains(label));
        assert!(!error.contains("ZZZZZZZZZZ"));
    }

    #[test]
    fn redacts_team_id_from_command_output() {
        let identity = OsStr::new("Developer ID Application: Nonexistent Corp (ZZZZZZZZZZ)");
        let label = OsStr::new("Developer ID Application: Nonexistent Corp");
        let output = redact_bytes(identity.as_bytes(), &[(identity, label)]);
        assert_eq!(output, label.as_bytes());
    }

    #[test]
    fn accepts_release_compiler_artifact_profile() {
        let messages = serde_json::json!({
            "reason": "compiler-artifact",
            "target": { "name": "neo", "kind": ["bin"] },
            "profile": { "opt_level": "3", "debug_assertions": false, "overflow_checks": false },
            "executable": "/target/release/neo"
        })
        .to_string();
        assert_eq!(
            release_executable_from_messages(messages.as_bytes()).unwrap(),
            Path::new("/target/release/neo")
        );
    }

    #[test]
    fn rejects_unoptimized_release_compiler_artifact_profile() {
        let messages = serde_json::json!({
            "reason": "compiler-artifact",
            "target": { "name": "neo", "kind": ["bin"] },
            "profile": { "opt_level": "0", "debug_assertions": false, "overflow_checks": false },
            "executable": "/target/release/neo"
        })
        .to_string();
        let error = release_executable_from_messages(messages.as_bytes())
            .unwrap_err()
            .to_string();
        assert!(error.contains("opt_level=\"0\""));
    }

    #[test]
    fn rejects_unexpected_optimized_release_compiler_artifact_profile() {
        let messages = serde_json::json!({
            "reason": "compiler-artifact",
            "target": { "name": "neo", "kind": ["bin"] },
            "profile": { "opt_level": "1", "debug_assertions": false, "overflow_checks": false },
            "executable": "/target/release/neo"
        })
        .to_string();
        let error = release_executable_from_messages(messages.as_bytes())
            .unwrap_err()
            .to_string();
        assert!(error.contains("opt_level=\"1\""));
    }

    #[test]
    fn rejects_debug_assertions_in_release_compiler_artifact_profile() {
        let messages = serde_json::json!({
            "reason": "compiler-artifact",
            "target": { "name": "neo", "kind": ["bin"] },
            "profile": { "opt_level": "3", "debug_assertions": true, "overflow_checks": false },
            "executable": "/target/release/neo"
        })
        .to_string();
        let error = release_executable_from_messages(messages.as_bytes())
            .unwrap_err()
            .to_string();
        assert!(error.contains("debug_assertions=true"));
    }

    #[test]
    fn rejects_overflow_checks_in_release_compiler_artifact_profile() {
        let messages = serde_json::json!({
            "reason": "compiler-artifact",
            "target": { "name": "neo", "kind": ["bin"] },
            "profile": { "opt_level": "3", "debug_assertions": false, "overflow_checks": true },
            "executable": "/target/release/neo"
        })
        .to_string();
        let error = release_executable_from_messages(messages.as_bytes())
            .unwrap_err()
            .to_string();
        assert!(error.contains("overflow_checks=true"));
    }

    #[test]
    fn rejects_release_profile_keys_in_root_manifest() {
        for key in ["opt-level", "debug-assertions", "overflow-checks", "lto"] {
            let fixture = Fixture::valid();
            let cargo_home = fixture.root.join("cargo-home");
            fs::write(
                fixture.root.join("Cargo.toml"),
                format!("[profile.release]\n{key} = false\n"),
            )
            .unwrap();
            let error = reject_release_profile_overrides(&fixture.root, &cargo_home)
                .unwrap_err()
                .to_string();
            assert!(error.contains(&format!("profile.release.{key}")));
            assert!(error.contains("Cargo.toml"));
        }
    }

    #[test]
    fn rejects_package_release_profile_override() {
        let fixture = Fixture::valid();
        let cargo_home = fixture.root.join("cargo-home");
        fs::write(
            fixture.root.join("Cargo.toml"),
            "[profile.release.package.neo]\nopt-level = 1\n",
        )
        .unwrap();
        let error = reject_release_profile_overrides(&fixture.root, &cargo_home)
            .unwrap_err()
            .to_string();
        assert!(error.contains("profile.release.package"));
    }

    #[test]
    fn rejects_release_profile_keys_in_cargo_configuration() {
        let fixture = Fixture::valid();
        let cargo_home = fixture.root.join("cargo-home");
        fs::create_dir_all(fixture.root.join(".cargo")).unwrap();
        fs::write(
            fixture.root.join(".cargo/config.toml"),
            "[profile.release]\ndebug-assertions = true\n",
        )
        .unwrap();
        let error = reject_release_profile_overrides(&fixture.root, &cargo_home)
            .unwrap_err()
            .to_string();
        assert!(error.contains("profile.release.debug-assertions"));
        assert!(error.contains(".cargo/config.toml"));
    }

    #[test]
    fn encoded_release_rustflags_override_plain_rustflags() {
        let rustflags = release_rustflags(
            Some(OsStr::new("-C\u{1f}target-cpu=native")),
            Some(OsStr::new("--cfg ignored")),
        );
        assert_eq!(rustflags, ["-C", "target-cpu=native"].map(OsString::from));
    }

    #[test]
    fn plain_release_rustflags_split_only_on_spaces() {
        let rustflags = release_rustflags(
            None,
            Some(OsStr::new("  -C  debuginfo=0\t--cfg release_probe ")),
        );
        assert_eq!(
            rustflags,
            ["-C", "debuginfo=0\t--cfg", "release_probe"].map(OsString::from)
        );
    }

    #[test]
    fn rejects_profile_codegen_rustflags() {
        for option in ["opt-level=0", "debug-assertions=on", "overflow-checks=on"] {
            for rustflags in [
                vec![OsString::from("-C"), OsString::from(option)],
                vec![OsString::from(format!("-C{option}"))],
            ] {
                let error = reject_profile_codegen_rustflags(&rustflags)
                    .unwrap_err()
                    .to_string();
                assert!(error.contains(option.split_once('=').unwrap().0));
            }
        }
    }

    #[test]
    fn accepts_non_profile_codegen_rustflags() {
        let rustflags = ["-C", "target-cpu=native", "--cfg", "release_probe"].map(OsString::from);
        reject_profile_codegen_rustflags(&rustflags).unwrap();
    }

    fn test_target() -> TargetContext {
        TargetContext {
            triple: "aarch64-apple-darwin".into(),
            cfg: ["target_arch=\"aarch64\"", "target_os=\"macos\"", "unix"]
                .map(|cfg| cfg.parse().unwrap())
                .into(),
        }
    }

    #[test]
    fn rejects_build_rustflags() {
        let configuration = "[build]\nrustflags = [\"--cfg\", \"from_config\"]"
            .parse::<toml::Value>()
            .unwrap();
        let error = reject_build_rustflags(&configuration, Path::new(".cargo/config.toml"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("build.rustflags"));
        assert!(error.contains("release rustflags"));
    }

    #[test]
    fn rejects_host_target_rustflags_in_cargo_configuration() {
        let fixture = Fixture::valid();
        let cargo_home = fixture.root.join("cargo-home");
        fs::create_dir_all(fixture.root.join(".cargo")).unwrap();
        fs::write(
            fixture.root.join(".cargo/config.toml"),
            "[target.aarch64-apple-darwin]\nrustflags = [\"-C\", \"target-cpu=native\"]\n",
        )
        .unwrap();
        let error = reject_target_rustflags_for(&fixture.root, &cargo_home, &test_target())
            .unwrap_err()
            .to_string();
        assert!(error.contains("target.aarch64-apple-darwin.rustflags"));
        assert!(error.contains("overrides release path remapping"));
    }

    #[test]
    fn accepts_non_host_target_rustflags_in_cargo_configuration() {
        let fixture = Fixture::valid();
        let cargo_home = fixture.root.join("cargo-home");
        fs::create_dir_all(fixture.root.join(".cargo")).unwrap();
        fs::write(
            fixture.root.join(".cargo/config.toml"),
            "[target.x86_64-unknown-linux-gnu]\nrustflags = [\"-C\", \"linker=clang\"]\n",
        )
        .unwrap();
        reject_target_rustflags_for(&fixture.root, &cargo_home, &test_target()).unwrap();
    }

    #[test]
    fn rejects_matching_cfg_rustflags_in_cargo_configuration() {
        let fixture = Fixture::valid();
        let cargo_home = fixture.root.join("cargo-home");
        fs::create_dir_all(fixture.root.join(".cargo")).unwrap();
        fs::write(
            fixture.root.join(".cargo/config.toml"),
            "[target.'cfg(all(unix, target_os = \"macos\"))']\nrustflags = [\"-C\", \"target-cpu=native\"]\n",
        )
        .unwrap();
        let error = reject_target_rustflags_for(&fixture.root, &cargo_home, &test_target())
            .unwrap_err()
            .to_string();
        assert!(error.contains("cfg(all(unix, target_os = \"macos\"))"));
    }

    #[test]
    fn accepts_nonmatching_cfg_rustflags_in_cargo_configuration() {
        let fixture = Fixture::valid();
        let cargo_home = fixture.root.join("cargo-home");
        fs::create_dir_all(fixture.root.join(".cargo")).unwrap();
        fs::write(
            fixture.root.join(".cargo/config.toml"),
            "[target.'cfg(any(target_os = \"linux\", not(unix)))']\nrustflags = [\"-C\", \"linker=clang\"]\n",
        )
        .unwrap();
        reject_target_rustflags_for(&fixture.root, &cargo_home, &test_target()).unwrap();
    }

    #[test]
    fn rejects_packager_signing_identity() {
        let fixture = Fixture::valid();
        fs::write(
            fixture.root.join("Cargo.toml"),
            "[package.metadata.packager.macos]\nsigning-identity = \"Developer ID Application: Example Corporation\"\n",
        )
        .unwrap();
        assert!(reject_packager_signing_identity(&fixture.root).is_err());
    }

    #[test]
    fn accepts_packager_metadata_without_signing_identity() {
        let fixture = Fixture::valid();
        fs::write(
            fixture.root.join("Cargo.toml"),
            "[package.metadata.packager.macos]\nminimum-system-version = \"26.1\"\n",
        )
        .unwrap();
        assert!(reject_packager_signing_identity(&fixture.root).is_ok());
    }

    #[test]
    fn reads_notary_submission_fields() {
        let mut dictionary = Dictionary::new();
        dictionary.insert("id".into(), "submission-id".into());
        dictionary.insert("status".into(), "Accepted".into());
        let submission = Value::Dictionary(dictionary);
        assert_eq!(
            submission_field(&submission, "id").unwrap(),
            "submission-id"
        );
        assert_eq!(submission_field(&submission, "status").unwrap(), "Accepted");
    }

    #[test]
    fn accepts_valid_bundle() {
        let fixture = Fixture::valid();
        assert_eq!(fixture.verify().unwrap(), 1);
    }

    #[test]
    fn removes_carbon_key() {
        let fixture = Fixture::valid();
        let path = fixture.app.join("Contents/Info.plist");
        let mut dictionary = Value::from_file(&path).unwrap().into_dictionary().unwrap();
        dictionary.insert("LSRequiresCarbon".into(), true.into());
        Value::Dictionary(dictionary).to_file_xml(&path).unwrap();
        remove_carbon_key(&path).unwrap();
        assert!(
            !Value::from_file(path)
                .unwrap()
                .as_dictionary()
                .unwrap()
                .contains_key("LSRequiresCarbon")
        );
    }

    #[test]
    fn rejects_empty_executable() {
        let fixture = Fixture::valid();
        fs::write(fixture.executable(), b"").unwrap();
        let error = fixture.verify().unwrap_err().to_string();
        assert!(error.contains("bundled executable is empty"));
    }

    #[test]
    fn rejects_nonexecutable_file() {
        let fixture = Fixture::valid();
        let mut permissions = fs::metadata(fixture.executable()).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(fixture.executable(), permissions).unwrap();
        let error = fixture.verify().unwrap_err().to_string();
        assert!(error.contains("no executable permission bits"));
    }

    #[test]
    fn rejects_non_mach_o_executable() {
        let fixture = Fixture::valid();
        fs::write(fixture.executable(), b"#!/bin/sh\nexit 0\n").unwrap();
        let error = fixture.verify().unwrap_err().to_string();
        assert!(error.contains("no supported Mach-O magic"));
    }

    #[test]
    fn rejects_truncated_mach_o_executable() {
        let fixture = Fixture::valid();
        fs::write(
            fixture.executable(),
            &test_macho(0x0100_000c, TEST_UUID)[..40],
        )
        .unwrap();
        let error = fixture.verify().unwrap_err().to_string();
        assert!(error.contains("truncated Mach-O load command area"));
    }

    #[test]
    fn rejects_truncated_mach_o_segment_data() {
        let fixture = Fixture::valid();
        let executable = test_macho_with_segment(256);
        fs::write(fixture.executable(), &executable[..200]).unwrap();
        let error = fixture.verify().unwrap_err().to_string();
        assert!(error.contains("truncated Mach-O segment data"));
    }

    #[test]
    fn rejects_byte_swapped_mach_o_executable() {
        let fixture = Fixture::valid();
        fs::write(
            fixture.executable(),
            test_byte_swapped_macho(0x0100_000c, TEST_UUID),
        )
        .unwrap();
        let error = fixture.verify().unwrap_err().to_string();
        assert!(error.contains("byte-swapped Mach-O is unsupported"));
    }

    #[test]
    fn rejects_fat_mach_o_executable() {
        let fixture = Fixture::valid();
        fs::write(fixture.executable(), b"\xca\xfe\xba\xbe fat executable").unwrap();
        let error = fixture.verify().unwrap_err().to_string();
        assert!(error.contains("fat Mach-O is unsupported"));
    }

    #[test]
    fn rejects_executable_with_different_cpu_type() {
        let fixture = Fixture::valid();
        fs::write(fixture.executable(), test_macho(0x0100_0007, TEST_UUID)).unwrap();
        let error = fixture.verify().unwrap_err().to_string();
        assert!(error.contains("CPU type x86_64"));
        assert!(error.contains("CPU type arm64"));
    }

    #[test]
    fn rejects_executable_with_different_uuid() {
        let fixture = Fixture::valid();
        fs::write(fixture.executable(), test_macho(0x0100_000c, [0x55; 16])).unwrap();
        let error = fixture.verify().unwrap_err().to_string();
        assert!(error.contains("bundled executable LC_UUID"));
        assert!(error.contains("does not match release artifact LC_UUID"));
    }

    #[test]
    fn rejects_missing_release_artifact() {
        let fixture = Fixture::valid();
        fs::remove_file(fixture.release_executable()).unwrap();
        let error = fixture.verify().unwrap_err().to_string();
        assert!(error.contains("missing release artifact"));
        assert!(error.contains("run `cargo xtask package` first"));
    }

    #[test]
    fn rejects_private_home_path_in_executable() {
        let fixture = Fixture::valid();
        let home = std::env::var_os("HOME").unwrap();
        fixture.append_to_executable(home.as_bytes());
        let error = fixture.verify().unwrap_err().to_string();
        assert!(error.contains("private home path prefix"));
    }

    #[test]
    fn rejects_darwin_temp_identifier_in_executable() {
        let fixture = Fixture::valid();
        let temporary_directory = std::env::temp_dir();
        let identifier = darwin_temp_identifier(&temporary_directory).unwrap();
        fixture.append_to_executable(identifier.as_bytes());
        let error = fixture.verify().unwrap_err().to_string();
        assert!(error.contains("Darwin temporary directory identifier"));
    }

    #[test]
    fn permits_debug_executable_during_packaging() {
        let fixture = Fixture::valid();
        fs::write(
            fixture.executable(),
            b"debug executable attempt to add with overflow",
        )
        .unwrap();
        assert_eq!(fixture.verify_debug().unwrap(), 1);
    }

    #[test]
    fn rejects_wrong_copyright() {
        let fixture = Fixture::valid();
        let path = fixture.app.join("Contents/Info.plist");
        let mut dictionary = Dictionary::new();
        dictionary.insert("NSHumanReadableCopyright".into(), "wrong".into());
        Value::Dictionary(dictionary).to_file_xml(path).unwrap();
        assert!(fixture.verify().is_err());
    }

    #[test]
    fn rejects_carbon_key() {
        let fixture = Fixture::valid();
        let path = fixture.app.join("Contents/Info.plist");
        let mut dictionary = Value::from_file(&path).unwrap().into_dictionary().unwrap();
        dictionary.insert("LSRequiresCarbon".into(), false.into());
        Value::Dictionary(dictionary).to_file_xml(path).unwrap();
        assert!(fixture.verify().is_err());
    }

    #[test]
    fn rejects_different_license() {
        let fixture = Fixture::valid();
        fs::write(fixture.app.join(BUNDLED_LICENSE), b"different\n").unwrap();
        assert!(fixture.verify().is_err());
    }

    #[test]
    fn rejects_missing_notices() {
        let fixture = Fixture::valid();
        fs::remove_file(fixture.app.join(BUNDLED_NOTICES)).unwrap();
        assert!(fixture.verify().is_err());
    }

    #[test]
    fn rejects_empty_notices() {
        let fixture = Fixture::valid();
        fs::write(fixture.root.join(NOTICES), b"").unwrap();
        assert!(fixture.verify().is_err());
    }

    #[test]
    fn rejects_notices_without_attributions() {
        let fixture = Fixture::valid();
        fs::write(fixture.root.join(NOTICES), b"# Notices\n").unwrap();
        fs::write(fixture.app.join(BUNDLED_NOTICES), b"# Notices\n").unwrap();
        assert!(fixture.verify().is_err());
    }

    #[test]
    fn rejects_different_notices() {
        let fixture = Fixture::valid();
        fs::write(fixture.app.join(BUNDLED_NOTICES), b"different\n").unwrap();
        assert!(fixture.verify().is_err());
    }

    #[test]
    fn rejects_escaped_entities() {
        for entity in ["&quot;", "&#x27;", "&amp;"] {
            let fixture = Fixture::valid();
            let notices = format!("# Notices\n\n- [crate 1.0.0](url)\n{entity}\n");
            fs::write(fixture.root.join(NOTICES), &notices).unwrap();
            fs::write(fixture.app.join(BUNDLED_NOTICES), notices).unwrap();
            assert!(fixture.verify().is_err());
        }
    }
}
