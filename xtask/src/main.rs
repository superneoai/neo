use cargo_platform::{Cfg, Platform};
use plist::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Cursor, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const PACKAGER_VERSION: &str = "cargo-packager 0.11.8";
const ABOUT_VERSION: &str = "cargo-about 0.8.2";
const NOTICES: &str = "packaging/generated/THIRD-PARTY-NOTICES.md";
const BUNDLED_NOTICES: &str = "Contents/Resources/Legal/THIRD-PARTY-NOTICES.md";
const BUNDLED_LICENSE: &str = "Contents/Resources/Legal/AGPL-3.0-or-later.txt";
const APP: &str = "dist/NEO.app";
const ENTITLEMENTS: &str = "packaging/NEO.entitlements";
const SIGNING_IDENTITY_ENV: &str = "NEO_SIGNING_IDENTITY";
const SIGNING_TEAM_IDENTIFIER_ENV: &str = "NEO_SIGNING_TEAM_IDENTIFIER";
const NOTARY_PROFILE: &str = "SUPERNEO_NOTARY";
const NOTARY_ARCHIVE: &str = "dist/NEO.zip";
const EXPECTED_METAL_TOOL_PREFIX: &str = "/private/var/run/com.apple.security.cryptexd/mnt/com.apple.MobileAsset.MetalToolchain-v17.6.109.0.";
const EXPECTED_METAL_TOOL_SUFFIX: &str =
    "/Metal.xctoolchain/usr/metal/32023/bin/metallib shaders.air -o shaders.metallib";
const LIBNEO_HTTPS_SOURCE: &str = "https://github.com/superneoai/libneo.git";
const LOCAL_LIBNEO_PATCH: &str =
    "patch.\"https://github.com/superneoai/libneo.git\".libneo.path=\"../libneo/crates/libneo\"";
const LOCAL_LIBNEO_GPUI_PATCH: &str = "patch.\"https://github.com/superneoai/libneo.git\".libneo-gpui.path=\"../libneo/crates/libneo-gpui\"";
const MINIMUM_SDK_MAJOR: u32 = 26;

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
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    match arguments.as_slice() {
        [command] if command == "check-source" => verify_pinned_source(&repository_root()),
        [command] if command == "check-sdk" => verify_sdk(),
        [command] if command == "package" => package(PackageProfile::Release),
        [command, option] if command == "package" && option == "--debug" => {
            package(PackageProfile::Debug)
        }
        [command] if command == "sign" => sign(),
        [command] if command == "notarize" => notarize(),
        [command, cargo_arguments @ ..]
            if command == "local-cargo" && !cargo_arguments.is_empty() =>
        {
            local_cargo(cargo_arguments)
        }
        _ => Err(failure(
            "usage: cargo xtask <check-source|check-sdk|package [--debug]|sign|notarize|local-cargo <arguments...>>",
        )),
    }
}

fn local_cargo(arguments: &[OsString]) -> Result<()> {
    let root = repository_root();
    let lockfile_path = root.join("Cargo.lock");
    let lockfile = fs::read(&lockfile_path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot read {}: {error}", lockfile_path.display()),
        )
    })?;
    let result = run_command(
        Command::new("cargo")
            .current_dir(&root)
            .args(["--config", LOCAL_LIBNEO_PATCH])
            .args(["--config", LOCAL_LIBNEO_GPUI_PATCH])
            .args(arguments),
    );
    fs::write(&lockfile_path, lockfile).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot restore {}: {error}", lockfile_path.display()),
        )
    })?;
    result
}

fn verify_pinned_source(root: &Path) -> Result<()> {
    let manifest_path = root.join("Cargo.toml");
    let lockfile_path = root.join("Cargo.lock");
    let manifest = fs::read(&manifest_path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot read {}: {error}", manifest_path.display()),
        )
    })?;
    let lockfile = fs::read(&lockfile_path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot read {}: {error}", lockfile_path.display()),
        )
    })?;
    verify_pinned_source_contents(&manifest, &lockfile)?;
    println!("verified libneo uses the pinned HTTPS source");
    Ok(())
}

fn verify_pinned_source_contents(manifest: &[u8], lockfile: &[u8]) -> Result<()> {
    if !contains_bytes(manifest, LIBNEO_HTTPS_SOURCE.as_bytes()) {
        return Err(failure(format!(
            "Cargo.toml does not contain the required libneo source {LIBNEO_HTTPS_SOURCE}"
        )));
    }
    for (name, contents) in [("Cargo.toml", manifest), ("Cargo.lock", lockfile)] {
        if contains_bytes(contents, b"ssh://") {
            return Err(failure(format!(
                "{name} contains an ssh:// URL; libneo sources must use HTTPS"
            )));
        }
    }
    Ok(())
}

fn verify_sdk() -> Result<()> {
    let output = Command::new("xcrun")
        .arg("--show-sdk-version")
        .output()
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot run xcrun --show-sdk-version: {error}"),
            )
        })?;
    if !output.status.success() {
        return Err(failure(format!(
            "xcrun --show-sdk-version exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let major = parse_sdk_major(&output.stdout)?;
    if major < MINIMUM_SDK_MAJOR {
        return Err(failure(format!(
            "macOS SDK major version {major} is below the required {MINIMUM_SDK_MAJOR}"
        )));
    }
    println!("verified macOS SDK major version {major}");
    Ok(())
}

fn parse_sdk_major(output: &[u8]) -> Result<u32> {
    let version = std::str::from_utf8(output)
        .map_err(|error| failure(format!("xcrun returned a non-UTF-8 SDK version: {error}")))?
        .trim();
    let major = version.split('.').next().unwrap_or_default();
    if major.is_empty() || !major.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(failure(format!(
            "xcrun returned an unparseable SDK version {version:?}"
        )));
    }
    major
        .parse()
        .map_err(|error| failure(format!("cannot parse SDK major version {major:?}: {error}")))
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
        let metadata = manifest_metadata(root)?;
        reject_ambient_release_environment(&metadata.minimum_system_version)?;
        let home = std::env::var_os("HOME").ok_or_else(|| failure("HOME is not set"))?;
        let cargo_home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(&home).join(".cargo"));
        verify_toolchain(root)?;
        let target_context = TargetContext::host()?;
        let configuration = scan_release_configuration(root, &cargo_home, &target_context)?;
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
        if configuration.has_matching_target_rustflags {
            rustflags.splice(0..0, configuration.in_tree_build_rustflags);
        }
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
        let rustflags_key = if configuration.has_matching_target_rustflags {
            format!("target.{:?}.rustflags", target_context.triple)
        } else {
            "build.rustflags".into()
        };
        let rustflags_config = format!("{rustflags_key}={}", toml::Value::Array(rustflags));

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
            b"opt-level"
                | b"debug-assertions"
                | b"overflow-checks"
                | b"debuginfo"
                | b"panic"
                | b"strip"
                | b"lto"
                | b"codegen-units"
        ) {
            return Err(failure(format!(
                "release rustflags cannot set {}; remove this profile-owned codegen option",
                String::from_utf8_lossy(name)
            )));
        }
    }
    Ok(())
}

fn verify_toolchain(root: &Path) -> Result<()> {
    let path = root.join("rust-toolchain.toml");
    let toolchain = fs::read_to_string(&path)?.parse::<toml::Value>()?;
    let expected = toolchain
        .get("toolchain")
        .and_then(|toolchain| toolchain.get("channel"))
        .and_then(toml::Value::as_str)
        .ok_or_else(|| failure(format!("{} has no toolchain.channel", path.display())))?;
    let version = command_output(Command::new("rustc").arg("-vV"))?;
    let version = String::from_utf8(version.stdout)?;
    let actual = version
        .lines()
        .find_map(|line| line.strip_prefix("release: "))
        .ok_or_else(|| failure("rustc -vV output has no release"))?;
    if actual != expected && !actual.starts_with(&format!("{expected}.")) {
        return Err(failure(format!(
            "rustc release {actual} does not match rust-toolchain.toml channel {expected}"
        )));
    }
    println!("verified Rust toolchain: release={actual}, channel={expected}");
    Ok(())
}

struct TargetContext {
    triple: String,
    cfg: Vec<Cfg>,
}

impl TargetContext {
    fn host() -> Result<Self> {
        let version = command_output(Command::new("rustc").arg("-vV"))?;
        let version = String::from_utf8(version.stdout)?;
        let triple = version
            .lines()
            .find_map(|line| line.strip_prefix("host: "))
            .filter(|triple| !triple.is_empty())
            .ok_or_else(|| failure("rustc -vV output has no host triple"))?
            .to_owned();
        let cfg = command_output(Command::new("rustc").args(["--print", "cfg"]))?;
        let cfg = String::from_utf8(cfg.stdout)?
            .lines()
            .map(str::parse)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Self { triple, cfg })
    }
}

#[derive(Debug, Default)]
struct ReleaseConfiguration {
    in_tree_build_rustflags: Vec<OsString>,
    has_matching_target_rustflags: bool,
}

struct CargoConfiguration {
    path: PathBuf,
    value: toml::Value,
    in_tree: bool,
}

fn reject_ambient_release_environment(minimum_system_version: &str) -> Result<()> {
    for name in [
        "RUSTC",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "SDKROOT",
        "DEVELOPER_DIR",
    ] {
        if std::env::var_os(name).is_some() {
            return Err(failure(format!(
                "ambient {name} is unsupported for release packaging"
            )));
        }
    }
    // Cargo applies force=true before it launches xtask, so inspect Cargo's own
    // environment to distinguish an ambient value from the in-tree value.
    if let Some(value) = parent_environment_value("MACOSX_DEPLOYMENT_TARGET")? {
        return Err(failure(format!(
            "ambient MACOSX_DEPLOYMENT_TARGET={} is unsupported; the in-tree value is {minimum_system_version}",
            value.to_string_lossy()
        )));
    }
    if let Some(value) = std::env::var_os("MACOSX_DEPLOYMENT_TARGET")
        && value != minimum_system_version
    {
        return Err(failure(format!(
            "MACOSX_DEPLOYMENT_TARGET={} does not match the in-tree value {minimum_system_version}",
            value.to_string_lossy()
        )));
    }
    if std::env::var_os("CARGO_BUILD_RUSTFLAGS").is_some() {
        return Err(failure(
            "CARGO_BUILD_RUSTFLAGS conflicts with release rustflags; pass supported flags through RUSTFLAGS or CARGO_ENCODED_RUSTFLAGS",
        ));
    }
    let mut profile_overrides = std::env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| name.as_bytes().starts_with(b"CARGO_PROFILE_RELEASE_"))
        .collect::<Vec<_>>();
    profile_overrides.sort();
    if let Some(name) = profile_overrides.first() {
        return Err(failure(format!(
            "release profile override environment variable {} is unsupported",
            name.to_string_lossy()
        )));
    }
    Ok(())
}

fn parent_environment_value(name: &str) -> Result<Option<OsString>> {
    let process_id = std::process::id().to_string();
    let parent = command_output(
        Command::new("ps")
            .args(["-o", "ppid=", "-p"])
            .arg(process_id),
    )?;
    let parent = String::from_utf8(parent.stdout)?.trim().to_owned();
    if parent.is_empty() {
        return Err(failure("cannot determine the parent process"));
    }
    let environment = command_output(
        Command::new("ps")
            .args(["eww", "-o", "command=", "-p"])
            .arg(parent),
    )?;
    let prefix = format!("{name}=");
    Ok(environment
        .stdout
        .split(|byte| byte.is_ascii_whitespace())
        .find_map(|field| {
            field
                .strip_prefix(prefix.as_bytes())
                .map(OsStr::from_bytes)
                .map(OsStr::to_os_string)
        }))
}

fn scan_release_configuration(
    root: &Path,
    cargo_home: &Path,
    target: &TargetContext,
) -> Result<ReleaseConfiguration> {
    let target_environment = format!(
        "CARGO_TARGET_{}_RUSTFLAGS",
        target.triple.replace('-', "_").to_ascii_uppercase()
    );
    if std::env::var_os(&target_environment).is_some() {
        return Err(failure(format!(
            "target-specific rustflags environment variable {target_environment} overrides release path remapping; pass those flags through RUSTFLAGS or CARGO_ENCODED_RUSTFLAGS"
        )));
    }

    let configurations = cargo_configurations(root, cargo_home)?;
    let mut release = ReleaseConfiguration::default();
    for configuration in configurations {
        let path = &configuration.path;
        let build = configuration
            .value
            .get("build")
            .and_then(toml::Value::as_table);
        if configuration.in_tree {
            if let Some(value) = build.and_then(|build| build.get("rustflags")) {
                let flags = configuration_rustflags(value, path, "build.rustflags")?;
                reject_profile_codegen_rustflags(&flags)?;
                release.in_tree_build_rustflags.extend(flags);
            }
        } else {
            for key in [
                "rustflags",
                "rustc",
                "rustc-wrapper",
                "rustc-workspace-wrapper",
            ] {
                if build.is_some_and(|build| build.contains_key(key)) {
                    return Err(failure(format!(
                        "build.{key} in out-of-tree Cargo configuration {} is unsupported for release packaging",
                        path.display()
                    )));
                }
            }
            reject_release_profile_keys(&configuration.value, path)?;
            reject_conflicting_config_environment(&configuration.value, path)?;
        }

        let Some(targets) = configuration
            .value
            .get("target")
            .and_then(toml::Value::as_table)
        else {
            continue;
        };
        for (selector, settings) in targets {
            let Some(rustflags) = settings
                .as_table()
                .and_then(|settings| settings.get("rustflags"))
            else {
                continue;
            };
            let platform = selector.parse::<Platform>().map_err(|error| {
                failure(format!(
                    "cannot evaluate target selector {selector:?} in {}: {error}",
                    path.display()
                ))
            })?;
            if !platform.matches(&target.triple, &target.cfg) {
                continue;
            }
            if !configuration.in_tree {
                return Err(failure(format!(
                    "target.{selector}.rustflags in out-of-tree Cargo configuration {} is unsupported for release packaging",
                    path.display()
                )));
            }
            let flags =
                configuration_rustflags(rustflags, path, &format!("target.{selector}.rustflags"))?;
            reject_profile_codegen_rustflags(&flags)?;
            release.has_matching_target_rustflags = true;
        }
    }
    Ok(release)
}

fn cargo_configurations(root: &Path, cargo_home: &Path) -> Result<Vec<CargoConfiguration>> {
    let root = fs::canonicalize(root)?;
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
    let mut seen = HashSet::new();
    let mut configurations = Vec::new();
    for path in paths {
        collect_cargo_configuration(&path, &root, &mut seen, &mut configurations)?;
    }
    Ok(configurations)
}

fn collect_cargo_configuration(
    path: &Path,
    root: &Path,
    seen: &mut HashSet<PathBuf>,
    configurations: &mut Vec<CargoConfiguration>,
) -> Result<()> {
    let value = match read_toml_file(path, "Cargo configuration")? {
        Some(value) => value,
        None => return Ok(()),
    };
    let path = fs::canonicalize(path)?;
    if !seen.insert(path.clone()) {
        return Ok(());
    }
    let includes = configuration_includes(&value, &path)?;
    configurations.push(CargoConfiguration {
        in_tree: path.starts_with(root),
        path: path.clone(),
        value,
    });
    for include in includes {
        collect_cargo_configuration(&include, root, seen, configurations)?;
    }
    Ok(())
}

fn configuration_includes(configuration: &toml::Value, path: &Path) -> Result<Vec<PathBuf>> {
    let Some(include) = configuration.get("include") else {
        return Ok(Vec::new());
    };
    let includes = match include {
        toml::Value::String(include) => vec![include.as_str()],
        toml::Value::Array(includes) => includes
            .iter()
            .map(|include| {
                include.as_str().ok_or_else(|| {
                    failure(format!(
                        "include entries in {} must be paths",
                        path.display()
                    ))
                })
            })
            .collect::<Result<Vec<_>>>()?,
        _ => {
            return Err(failure(format!(
                "include in {} must be a path or an array of paths",
                path.display()
            )));
        }
    };
    let directory = path.parent().ok_or_else(|| {
        failure(format!(
            "Cargo configuration has no parent: {}",
            path.display()
        ))
    })?;
    Ok(includes
        .into_iter()
        .map(|include| {
            let include = PathBuf::from(include);
            if include.is_absolute() {
                include
            } else {
                directory.join(include)
            }
        })
        .collect())
}

fn configuration_rustflags(value: &toml::Value, path: &Path, key: &str) -> Result<Vec<OsString>> {
    match value {
        toml::Value::String(flags) => Ok(flags.split_whitespace().map(OsString::from).collect()),
        toml::Value::Array(flags) => flags
            .iter()
            .map(|flag| {
                flag.as_str().map(OsString::from).ok_or_else(|| {
                    failure(format!(
                        "{key} in {} must contain only strings",
                        path.display()
                    ))
                })
            })
            .collect(),
        _ => Err(failure(format!(
            "{key} in {} must be a string or an array of strings",
            path.display()
        ))),
    }
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
            "profile.release.{key} in out-of-tree Cargo configuration {} is unsupported for release packaging",
            path.display()
        )));
    }
    Ok(())
}

fn reject_conflicting_config_environment(configuration: &toml::Value, path: &Path) -> Result<()> {
    let Some(environment) = configuration.get("env").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    for name in ["SDKROOT", "DEVELOPER_DIR", "MACOSX_DEPLOYMENT_TARGET"] {
        if environment.contains_key(name) {
            return Err(failure(format!(
                "env.{name} in out-of-tree Cargo configuration {} is unsupported for release packaging",
                path.display()
            )));
        }
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
    let build_environment = match profile {
        PackageProfile::Release => Some(ReleaseBuildEnvironment::new(&root)?),
        PackageProfile::Debug => None,
    };
    reject_packager_signing_identity(&root)?;
    require_tool_version("packager", PACKAGER_VERSION)?;
    require_tool_version("about", ABOUT_VERSION)?;
    generate_notices()?;
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
    let attribution_count = if let Some(release_executable) = release_executable.as_deref() {
        verify_release_bundle(&root, &app, release_executable, ExecutableMatch::Exact)?
    } else {
        verify_bundle(&root, &app, None)?
    };
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
    let output = command
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?
        .wait_with_output()?;
    if !output.status.success() {
        return Err(failure(format!("{display} exited with {}", output.status)));
    }
    let executable = release_executable_from_messages(&output.stdout)?;
    let bytes = fs::read(&executable)?;
    println!(
        "recorded built release artifact SHA-256 {}: {}",
        sha256_hex(&bytes),
        executable.display()
    );
    Ok(executable)
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
        let debuginfo = message
            .pointer("/profile/debuginfo")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| failure("neo compiler artifact has no profile.debuginfo"))?;
        if !matches!(opt_level, "2" | "3" | "s" | "z")
            || debug_assertions
            || overflow_checks
            || debuginfo != 0
        {
            return Err(failure(format!(
                "neo compiler artifact has unexpected release profile: opt_level={opt_level:?}, debug_assertions={debug_assertions}, overflow_checks={overflow_checks}, debuginfo={debuginfo}"
            )));
        }
        let executable = message
            .get("executable")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| failure("neo compiler artifact has no executable path"))?;
        artifact = Some(PathBuf::from(executable));
        println!(
            "verified release compiler artifact profile: opt_level={opt_level:?}, debug_assertions={debug_assertions}, overflow_checks={overflow_checks}, debuginfo={debuginfo}; executable={executable}"
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
    let expected_team_identifier = expected_signing_team_identifier()?;
    let root = repository_root();
    std::env::set_current_dir(&root)?;
    let app = root.join(APP);
    if !app.is_dir() {
        return Err(failure(format!("missing app bundle: {}", app.display())));
    }
    let release_executable = configured_release_executable(&root);
    let attribution_count =
        verify_release_bundle(&root, &app, &release_executable, ExecutableMatch::Exact)?;
    println!(
        "verified {} with {attribution_count} crate attributions",
        app.display()
    );
    let identity = signing_identity()?;
    let team_identifier = signing_team_identifier(&identity)?;
    verify_signing_team_identifier(&team_identifier, &expected_team_identifier)?;
    let metadata = manifest_metadata(&root)?;
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
        sign_item(&item, &identity, &team_identifier, None, &entitlements)?;
    }
    sign_item(
        &app,
        &identity,
        &team_identifier,
        Some(&metadata.identifier),
        &entitlements,
    )?;
    verify_release_bundle(&root, &app, &release_executable, ExecutableMatch::Stable)?;
    run_command(
        Command::new("codesign")
            .args(["--verify", "--deep", "--strict", "--verbose=2"])
            .arg(&app),
    )?;
    validate_bundle_signatures(&app, &metadata, &team_identifier)?;

    let assessment = Command::new("spctl")
        .args(["-a", "-vvv", "-t", "exec"])
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
        println!("using signing identity from {SIGNING_IDENTITY_ENV}: {identity}");
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
            println!("selected signing identity: {identity}");
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

fn signing_team_identifier(identity: &str) -> Result<String> {
    identity
        .strip_suffix(')')
        .and_then(|identity| identity.rsplit_once(" (").map(|(_, team)| team))
        .filter(|team| !team.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| failure("Developer ID Application identity has no team identifier"))
}

fn expected_signing_team_identifier() -> Result<String> {
    configured_signing_team_identifier(std::env::var_os(SIGNING_TEAM_IDENTIFIER_ENV))
}

fn configured_signing_team_identifier(value: Option<OsString>) -> Result<String> {
    let value = value.ok_or_else(|| {
        failure(format!(
            "{SIGNING_TEAM_IDENTIFIER_ENV} is not set; set it to the 10-character Apple Developer team identifier before signing or notarizing"
        ))
    })?;
    let value = value.into_string().map_err(|_| {
        failure(format!(
            "{SIGNING_TEAM_IDENTIFIER_ENV} must contain valid Unicode"
        ))
    })?;
    if value.len() != 10
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
    {
        return Err(failure(format!(
            "{SIGNING_TEAM_IDENTIFIER_ENV} must be a 10-character uppercase ASCII team identifier"
        )));
    }
    Ok(value)
}

fn verify_signing_team_identifier(actual: &str, expected: &str) -> Result<()> {
    if actual != expected {
        return Err(failure(format!(
            "signing identity team {actual} does not match {SIGNING_TEAM_IDENTIFIER_ENV}"
        )));
    }
    Ok(())
}

fn validate_signature_details(
    details: &str,
    expected_identifier: Option<&str>,
    expected_team_identifier: &str,
) -> Result<()> {
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
    if !details.lines().any(|line| line.starts_with("Timestamp=")) {
        return Err(failure("codesign output has no secure Timestamp line"));
    }
    if details.lines().any(|line| line.starts_with("Signed Time=")) {
        return Err(failure(
            "codesign output has Signed Time instead of a secure timestamp",
        ));
    }
    let team = details
        .lines()
        .find_map(|line| line.strip_prefix("TeamIdentifier="))
        .ok_or_else(|| failure("codesign output has no TeamIdentifier"))?;
    if team != expected_team_identifier {
        return Err(failure(format!(
            "signature TeamIdentifier {team} does not match {expected_team_identifier}"
        )));
    }
    let identifier = details
        .lines()
        .find_map(|line| line.strip_prefix("Identifier="))
        .ok_or_else(|| failure("codesign output has no Identifier"))?;
    if let Some(expected) = expected_identifier
        && identifier != expected
    {
        return Err(failure(format!(
            "signature Identifier {identifier} does not match CFBundleIdentifier {expected}"
        )));
    }
    Ok(())
}

fn validate_signature_executable(details: &str, app: &Path) -> Result<()> {
    let executable = details
        .lines()
        .find_map(|line| line.strip_prefix("Executable="))
        .ok_or_else(|| failure("codesign output has no Executable path"))?;
    let executable = fs::canonicalize(executable)?;
    let app = fs::canonicalize(app)?;
    if !executable.starts_with(&app) {
        return Err(failure(format!(
            "signed executable resolves outside the app bundle: {}",
            executable.display()
        )));
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

fn sign_item(
    path: &Path,
    identity: &str,
    team_identifier: &str,
    expected_identifier: Option<&str>,
    entitlements: &Path,
) -> Result<()> {
    println!("signing {}", path.display());
    run_command(
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
    )?;
    let signature = command_output(Command::new("codesign").arg("-dvvv").arg(path))?;
    let details = format!(
        "{}{}",
        String::from_utf8_lossy(&signature.stdout),
        String::from_utf8_lossy(&signature.stderr)
    );
    validate_signature_details(&details, expected_identifier, team_identifier)
}

fn validate_bundle_signatures(
    app: &Path,
    metadata: &ManifestMetadata,
    team_identifier: &str,
) -> Result<()> {
    let mut items = nested_code(app)?;
    items.push(app.to_path_buf());
    for item in items {
        let signature = command_output(Command::new("codesign").arg("-dvvv").arg(&item))?;
        let details = format!(
            "{}{}",
            String::from_utf8_lossy(&signature.stdout),
            String::from_utf8_lossy(&signature.stderr)
        );
        let expected_identifier = (item == app).then_some(metadata.identifier.as_str());
        validate_signature_details(&details, expected_identifier, team_identifier)?;
        if item == app {
            validate_signature_executable(&details, app)?;
        }
    }
    Ok(())
}

fn notarize() -> Result<()> {
    let expected_team_identifier = expected_signing_team_identifier()?;
    let root = repository_root();
    std::env::set_current_dir(&root)?;
    let app = root.join(APP);
    if !app.is_dir() {
        return Err(failure(format!("missing app bundle: {}", app.display())));
    }
    let release_executable = configured_release_executable(&root);
    let attribution_count =
        verify_release_bundle(&root, &app, &release_executable, ExecutableMatch::Stable)?;
    println!(
        "verified {} with {attribution_count} crate attributions",
        app.display()
    );
    run_command(
        Command::new("codesign")
            .args(["--verify", "--deep", "--strict", "--verbose=2"])
            .arg(&app),
    )?;
    let metadata = manifest_metadata(&root)?;
    validate_bundle_signatures(&app, &metadata, &expected_team_identifier)?;

    let archive = root.join(NOTARY_ARCHIVE);
    if archive.exists() {
        fs::remove_file(&archive)?;
    }
    run_command(
        Command::new("ditto")
            .args(["-c", "-k", "--keepParent", "--sequesterRsrc"])
            .arg(&app)
            .arg(&archive),
    )?;
    verify_notary_archive(&archive)?;

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
    let log = command_output(Command::new("xcrun").args([
        "notarytool",
        "log",
        submission_id,
        "--keychain-profile",
        NOTARY_PROFILE,
    ]))?;
    print!("{}", String::from_utf8_lossy(&log.stdout));
    eprint!("{}", String::from_utf8_lossy(&log.stderr));
    validate_notary_log(&log.stdout)?;
    if !output.status.success() || status != "Accepted" {
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
        .args(["-a", "-vvv", "-t", "exec"])
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

fn validate_notary_log(log: &[u8]) -> Result<()> {
    let log: serde_json::Value = serde_json::from_slice(log)
        .map_err(|error| failure(format!("invalid notary log JSON: {error}")))?;
    match log.get("issues") {
        None | Some(serde_json::Value::Null) => Ok(()),
        Some(serde_json::Value::Array(issues)) if issues.is_empty() => Ok(()),
        Some(issues) => Err(failure(format!("notary log contains issues: {issues}"))),
    }
}

fn verify_notary_archive(archive: &Path) -> Result<()> {
    let entries = command_output(Command::new("zipinfo").arg("-1").arg(archive))?;
    let interleaved = String::from_utf8(entries.stdout)?
        .lines()
        .filter(|entry| {
            !entry.starts_with("__MACOSX/")
                && entry
                    .split('/')
                    .any(|component| component.starts_with("._"))
        })
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if !interleaved.is_empty() {
        return Err(failure(format!(
            "notary archive contains interleaved AppleDouble entries: {interleaved:?}"
        )));
    }
    println!("verified notary archive has zero interleaved AppleDouble entries");
    Ok(())
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask must be a workspace member")
        .to_path_buf()
}

struct ManifestMetadata {
    version: String,
    copyright: String,
    identifier: String,
    minimum_system_version: String,
}

fn manifest_metadata(root: &Path) -> Result<ManifestMetadata> {
    let path = root.join("Cargo.toml");
    let manifest = fs::read_to_string(&path)?.parse::<toml::Value>()?;
    let string = |keys: &[&str]| {
        keys.iter()
            .try_fold(&manifest, |value, key| value.get(*key))
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                failure(format!(
                    "{} has no string {}",
                    path.display(),
                    keys.join(".")
                ))
            })
    };
    Ok(ManifestMetadata {
        version: string(&["package", "version"])?,
        copyright: string(&["package", "metadata", "packager", "copyright"])?,
        identifier: string(&["package", "metadata", "packager", "identifier"])?,
        minimum_system_version: string(&[
            "package",
            "metadata",
            "packager",
            "macos",
            "minimum-system-version",
        ])?,
    })
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

fn verify_release_bundle(
    root: &Path,
    app: &Path,
    release_executable: &Path,
    executable_match: ExecutableMatch,
) -> Result<usize> {
    let attribution_count = verify_bundle(root, app, None)?;
    let plist = Value::from_file(app.join("Contents/Info.plist"))?;
    let dictionary = plist
        .as_dictionary()
        .ok_or_else(|| failure("bundle Info.plist is not a dictionary"))?;
    verify_bundled_executable(root, app, dictionary, release_executable, executable_match)?;
    let executable = bundled_executable(app, dictionary)?;
    let metadata = manifest_metadata(root)?;
    verify_effective_release_artifact(&executable)?;
    verify_deployment_target(&executable, &metadata.minimum_system_version)?;
    verify_bundle_hygiene(root, app)?;
    Ok(attribution_count)
}

fn bundled_executable(app: &Path, dictionary: &plist::Dictionary) -> Result<PathBuf> {
    let name = dictionary
        .get("CFBundleExecutable")
        .and_then(Value::as_string)
        .filter(|name| Path::new(name).file_name() == Some(OsStr::new(name)))
        .ok_or_else(|| failure("bundle has no valid CFBundleExecutable"))?;
    Ok(app.join("Contents/MacOS").join(name))
}

fn verify_effective_release_artifact(executable: &Path) -> Result<()> {
    let symbols = command_output(Command::new("nm").arg("-pa").arg(executable))?;
    let oso_count = symbols
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| contains_bytes(line, b" OSO "))
        .count();
    if oso_count != 0 {
        return Err(failure(format!(
            "release artifact contains {oso_count} OSO entries: {}",
            executable.display()
        )));
    }

    let strings = command_output(Command::new("strings").arg("-a").arg(executable))?;
    let strings = String::from_utf8_lossy(&strings.stdout);
    let overflow_count = strings
        .lines()
        .filter(|line| line.contains("attempt to ") && line.contains(" with overflow"))
        .count();
    if overflow_count != 0 {
        return Err(failure(format!(
            "release artifact contains {overflow_count} overflow-check panic strings: {}",
            executable.display()
        )));
    }
    let remapped_paths = strings
        .lines()
        .filter(|line| line.starts_with("/cargo/") || line.starts_with("/source/"))
        .count();
    if remapped_paths == 0 {
        return Err(failure(format!(
            "release artifact contains no /cargo/ or /source/ paths; path remapping did not run: {}",
            executable.display()
        )));
    }
    let metal_commands = strings
        .lines()
        .filter(|line| line.contains("/private/var/run/com.apple.security.cryptexd/"))
        .collect::<Vec<_>>();
    if metal_commands.len() != 1 || !is_expected_metal_tool_command(metal_commands[0]) {
        return Err(failure(format!(
            "release artifact has an unexpected Metal toolchain fingerprint: {metal_commands:?}"
        )));
    }
    println!(
        "verified effective release artifact: OSO entries=0, overflow panic strings=0, remapped paths={remapped_paths}, Metal toolchain fingerprint=expected"
    );
    Ok(())
}

fn is_expected_metal_tool_command(command: &str) -> bool {
    command
        .strip_prefix(EXPECTED_METAL_TOOL_PREFIX)
        .and_then(|command| command.strip_suffix(EXPECTED_METAL_TOOL_SUFFIX))
        .is_some_and(|discriminator| {
            discriminator.len() == 6
                && discriminator
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric())
        })
}

fn verify_deployment_target(executable: &Path, expected: &str) -> Result<()> {
    let load_commands = command_output(Command::new("otool").arg("-l").arg(executable))?;
    let load_commands = String::from_utf8(load_commands.stdout)?;
    let mut in_build_version = false;
    let mut minimum = None;
    for line in load_commands.lines() {
        let line = line.trim();
        if line == "cmd LC_BUILD_VERSION" {
            in_build_version = true;
        } else if in_build_version {
            if let Some(value) = line.strip_prefix("minos ") {
                minimum = Some(value.trim().to_owned());
                break;
            }
            if line.starts_with("cmd ") {
                in_build_version = false;
            }
        }
    }
    let minimum = minimum.ok_or_else(|| failure("Mach-O has no LC_BUILD_VERSION minos value"))?;
    if minimum != expected {
        return Err(failure(format!(
            "LC_BUILD_VERSION minos {minimum} does not match LSMinimumSystemVersion {expected}"
        )));
    }
    println!("verified deployment target: LC_BUILD_VERSION minos={minimum}");
    Ok(())
}

fn verify_bundle_hygiene(root: &Path, app: &Path) -> Result<()> {
    let home = std::env::var_os("HOME").ok_or_else(|| failure("HOME is not set"))?;
    if home.is_empty() || home == "/" {
        return Err(failure("HOME is not a private path prefix"));
    }
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&home).join(".cargo"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    let temporary_directory = std::env::temp_dir();
    let temporary_identifier = darwin_temp_identifier(&temporary_directory).ok_or_else(|| {
        failure(format!(
            "TMPDIR is outside /var/folders; cannot verify its per-user identifier: {}",
            temporary_directory.display()
        ))
    })?;
    let hostname = command_output(&mut Command::new("hostname"))?;
    let hostname = String::from_utf8(hostname.stdout)?.trim().to_owned();
    if hostname.is_empty() {
        return Err(failure("hostname is empty"));
    }
    let canonical_root = fs::canonicalize(root)?;
    let canonical_cargo_home = fs::canonicalize(cargo_home)?;
    let canonical_target = fs::canonicalize(target)?;
    let markers = [
        (home.as_bytes(), "private home path"),
        (canonical_root.as_os_str().as_bytes(), "repository root"),
        (
            canonical_cargo_home.as_os_str().as_bytes(),
            "Cargo home path",
        ),
        (canonical_target.as_os_str().as_bytes(), "Cargo target path"),
        (b"/var/folders", "Darwin temporary directory path"),
        (
            b"/private/var/folders",
            "private Darwin temporary directory path",
        ),
        (
            temporary_identifier.as_bytes(),
            "Darwin temporary directory identifier",
        ),
        (hostname.as_bytes(), "build host name"),
    ];

    let mut files = Vec::new();
    collect_regular_files(app, &mut files)?;
    for file in &files {
        reject_file_markers(&fs::read(file)?, &markers, file, "contents")?;
        let attributes = command_output(Command::new("xattr").arg(file))?;
        for attribute in attributes
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|attribute| !attribute.is_empty())
        {
            let value = command_output(
                Command::new("xattr")
                    .arg("-p")
                    .arg(OsStr::from_bytes(attribute))
                    .arg(file),
            )?;
            reject_file_markers(&value.stdout, &markers, file, "extended attributes")?;
        }
    }
    println!(
        "verified release hygiene across {} regular bundle files and their extended attributes",
        files.len()
    );
    Ok(())
}

fn collect_regular_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_regular_files(&entry.path(), files)?;
        } else if file_type.is_file() {
            files.push(entry.path());
        }
    }
    Ok(())
}

fn reject_file_markers(
    bytes: &[u8],
    markers: &[(&[u8], &str)],
    file: &Path,
    source: &str,
) -> Result<()> {
    for (marker, description) in markers {
        if !marker.is_empty() && contains_bytes(bytes, marker) {
            return Err(failure(format!(
                "bundle file {source} contain {description}: {}",
                file.display()
            )));
        }
    }
    Ok(())
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn verify_bundle(root: &Path, app: &Path, release_executable: Option<&Path>) -> Result<usize> {
    let metadata = manifest_metadata(root)?;
    let plist_path = app.join("Contents/Info.plist");
    let plist = Value::from_file(&plist_path)?;
    let dictionary = plist.as_dictionary().ok_or_else(|| {
        failure(format!(
            "{} is not a plist dictionary",
            plist_path.display()
        ))
    })?;
    for (key, expected) in [
        ("CFBundlePackageType", "APPL"),
        ("CFBundleIdentifier", metadata.identifier.as_str()),
        ("CFBundleShortVersionString", metadata.version.as_str()),
        (
            "LSMinimumSystemVersion",
            metadata.minimum_system_version.as_str(),
        ),
        ("NSHumanReadableCopyright", metadata.copyright.as_str()),
    ] {
        let actual = dictionary
            .get(key)
            .and_then(Value::as_string)
            .unwrap_or_default();
        if actual != expected {
            return Err(failure(format!(
                "unexpected {key}: expected {expected:?}, found {actual:?}"
            )));
        }
    }
    if dictionary.contains_key("LSRequiresCarbon") {
        return Err(failure(format!(
            "obsolete LSRequiresCarbon remains in {}",
            plist_path.display()
        )));
    }
    if let Some(release_executable) = release_executable {
        verify_bundled_executable(
            root,
            app,
            dictionary,
            release_executable,
            ExecutableMatch::Exact,
        )?;
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

#[derive(Clone, Copy)]
enum ExecutableMatch {
    Exact,
    Stable,
}

fn verify_bundled_executable(
    root: &Path,
    app: &Path,
    dictionary: &plist::Dictionary,
    release_executable: &Path,
    executable_match: ExecutableMatch,
) -> Result<()> {
    let metadata = manifest_metadata(root)?;
    reject_ambient_release_environment(&metadata.minimum_system_version)?;
    let home = std::env::var_os("HOME").ok_or_else(|| failure("HOME is not set"))?;
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&home).join(".cargo"));
    reject_profile_codegen_rustflags(&ambient_release_rustflags())?;
    scan_release_configuration(root, &cargo_home, &TargetContext::host()?)?;
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
    if bundled_identity.cpu_subtype != release_identity.cpu_subtype {
        return Err(failure(format!(
            "bundled executable CPU subtype 0x{:08X} does not match release artifact CPU subtype 0x{:08X}: {} != {}",
            bundled_identity.cpu_subtype,
            release_identity.cpu_subtype,
            executable.display(),
            release_executable.display()
        )));
    }
    if matches!(executable_match, ExecutableMatch::Exact) && bytes != release_bytes {
        return Err(failure(format!(
            "bundled executable SHA-256 {} does not match release artifact SHA-256 {}: {} != {}",
            sha256_hex(&bytes),
            sha256_hex(&release_bytes),
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
        "matched executable identity: {} SHA-256 {} UUID {} ({} subtype 0x{:08X}); {} SHA-256 {} UUID {} ({} subtype 0x{:08X})",
        executable.display(),
        sha256_hex(&bytes),
        format_uuid(&bundled_identity.uuid),
        cpu_type_name(bundled_identity.cpu_type),
        bundled_identity.cpu_subtype,
        release_executable.display(),
        sha256_hex(&release_bytes),
        format_uuid(&release_identity.uuid),
        cpu_type_name(release_identity.cpu_type),
        release_identity.cpu_subtype
    );
    Ok(())
}

struct MachOIdentity {
    cpu_type: u32,
    cpu_subtype: u32,
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
    let cpu_subtype = read_macho_u32(bytes, 8)?;
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
    Ok(MachOIdentity {
        cpu_type,
        cpu_subtype,
        uuid,
    })
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

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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
    let display = display_command(command);
    let output = command.output()?;
    io::stdout().write_all(&output.stdout)?;
    io::stderr().write_all(&output.stderr)?;
    if !output.status.success() {
        return Err(failure(format!("{display} exited with {}", output.status)));
    }
    Ok(())
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
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|argument| argument.to_string_lossy())
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
            fs::write(
                root.join("Cargo.toml"),
                "[package]\nversion = \"0.0.1\"\n[package.metadata.packager]\ncopyright = \"Copyright © 2026 ACTUAL LTD.\"\nidentifier = \"com.superneo.neo\"\n[package.metadata.packager.macos]\nminimum-system-version = \"26.1\"\n",
            )
            .unwrap();
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
            dictionary.insert("CFBundlePackageType".into(), "APPL".into());
            dictionary.insert("CFBundleIdentifier".into(), "com.superneo.neo".into());
            dictionary.insert("CFBundleShortVersionString".into(), "0.0.1".into());
            dictionary.insert("LSMinimumSystemVersion".into(), "26.1".into());
            dictionary.insert(
                "NSHumanReadableCopyright".into(),
                "Copyright © 2026 ACTUAL LTD.".into(),
            );
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

        fn verify_stable_identity(&self) -> Result<()> {
            let plist = Value::from_file(self.app.join("Contents/Info.plist"))?;
            verify_bundled_executable(
                &self.root,
                &self.app,
                plist.as_dictionary().unwrap(),
                &self.release_executable(),
                ExecutableMatch::Stable,
            )
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
    fn accepts_pinned_https_source() {
        let manifest = format!("source = {LIBNEO_HTTPS_SOURCE:?}");
        verify_pinned_source_contents(manifest.as_bytes(), b"lockfile").unwrap();
    }

    #[test]
    fn rejects_missing_pinned_https_source() {
        let error = verify_pinned_source_contents(b"manifest", b"lockfile")
            .unwrap_err()
            .to_string();
        assert!(error.contains("Cargo.toml"));
        assert!(error.contains(LIBNEO_HTTPS_SOURCE));
    }

    #[test]
    fn rejects_ssh_urls_in_source_files() {
        let manifest = format!("source = {LIBNEO_HTTPS_SOURCE:?}");
        for (manifest, lockfile, expected) in [
            (
                format!("{manifest}\nother = \"ssh://example.invalid/repository\""),
                "lockfile".to_owned(),
                "Cargo.toml",
            ),
            (
                manifest.clone(),
                "source = \"ssh://example.invalid/repository\"".to_owned(),
                "Cargo.lock",
            ),
        ] {
            let error = verify_pinned_source_contents(manifest.as_bytes(), lockfile.as_bytes())
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected));
            assert!(error.contains("ssh://"));
        }
    }

    #[test]
    fn parses_sdk_major_version() {
        assert_eq!(parse_sdk_major(b"26.5\n").unwrap(), 26);
        assert_eq!(parse_sdk_major(b"27\n").unwrap(), 27);
    }

    #[test]
    fn rejects_missing_or_unparseable_sdk_version() {
        for output in [b"".as_slice(), b"not-a-version", b".5", b"25x.1"] {
            let error = parse_sdk_major(output).unwrap_err().to_string();
            assert!(error.contains("unparseable SDK version"));
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
        let valid = "Executable=/tmp/NEO.app/Contents/MacOS/neo\nIdentifier=com.superneo.neo\nCodeDirectory v=20500 flags=0x10000(runtime) hashes=1+1 location=embedded\nAuthority=Developer ID Application: Example Corporation (TEAMID1234)\nTimestamp=Aug 21, 2026\nTeamIdentifier=TEAMID1234\n";
        assert!(validate_signature_details(valid, Some("com.superneo.neo"), "TEAMID1234").is_ok());

        for invalid in [
            valid.replace("Timestamp=Aug 21, 2026", "Signed Time=Aug 21, 2026"),
            valid.replace("flags=0x10000(runtime)", "flags=0x0(none)"),
            valid.replace("Developer ID Application:", "Apple Development:"),
            valid.replace("TeamIdentifier=TEAMID1234", "TeamIdentifier=OTHERTEAM"),
            valid.replace(
                "Identifier=com.superneo.neo",
                "Identifier=com.example.other",
            ),
        ] {
            assert!(
                validate_signature_details(&invalid, Some("com.superneo.neo"), "TEAMID1234")
                    .is_err()
            );
        }
    }

    #[test]
    fn extracts_signing_team_identifier() {
        assert_eq!(
            signing_team_identifier("Developer ID Application: Example Corporation (TEAMID1234)")
                .unwrap(),
            "TEAMID1234"
        );
    }

    #[test]
    fn signing_team_configuration_fails_closed_when_unset() {
        let error = configured_signing_team_identifier(None)
            .unwrap_err()
            .to_string();
        assert!(error.contains(SIGNING_TEAM_IDENTIFIER_ENV));
        assert!(error.contains("is not set"));
    }

    #[test]
    fn rejects_invalid_signing_team_configuration() {
        for value in ["", "short", "lowercase1", "TEAM-ID123"] {
            let error = configured_signing_team_identifier(Some(value.into()))
                .unwrap_err()
                .to_string();
            assert!(error.contains(SIGNING_TEAM_IDENTIFIER_ENV));
        }
    }

    #[test]
    fn signing_team_assertion_rejects_wrong_identifier() {
        let error = verify_signing_team_identifier("TEAMID1234", "OTHER12345")
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not match"));
        assert!(error.contains(SIGNING_TEAM_IDENTIFIER_ENV));
    }

    #[test]
    fn accepts_matching_signing_team_identifier() {
        let expected = configured_signing_team_identifier(Some("TEAMID1234".into())).unwrap();
        verify_signing_team_identifier("TEAMID1234", &expected).unwrap();
    }

    #[test]
    fn accepts_variable_metal_mount_discriminators() {
        for discriminator in ["s18CDK", "UqzdZ2"] {
            assert!(is_expected_metal_tool_command(&format!(
                "{EXPECTED_METAL_TOOL_PREFIX}{discriminator}{EXPECTED_METAL_TOOL_SUFFIX}"
            )));
        }
    }

    #[test]
    fn rejects_unexpected_metal_tool_fingerprints() {
        for command in [
            format!("{EXPECTED_METAL_TOOL_PREFIX}short{EXPECTED_METAL_TOOL_SUFFIX}"),
            format!("{EXPECTED_METAL_TOOL_PREFIX}bad/ID{EXPECTED_METAL_TOOL_SUFFIX}"),
            format!(
                "{}s18CDK{EXPECTED_METAL_TOOL_SUFFIX}",
                EXPECTED_METAL_TOOL_PREFIX.replace("17.6.109.0", "17.7.0.0")
            ),
            format!("{EXPECTED_METAL_TOOL_PREFIX}s18CDK/other/metallib"),
        ] {
            assert!(!is_expected_metal_tool_command(&command));
        }
    }

    #[test]
    fn accepts_release_compiler_artifact_profile() {
        let messages = serde_json::json!({
            "reason": "compiler-artifact",
            "target": { "name": "neo", "kind": ["bin"] },
            "profile": { "opt_level": "3", "debug_assertions": false, "overflow_checks": false, "debuginfo": 0 },
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
            "profile": { "opt_level": "0", "debug_assertions": false, "overflow_checks": false, "debuginfo": 0 },
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
            "profile": { "opt_level": "1", "debug_assertions": false, "overflow_checks": false, "debuginfo": 0 },
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
            "profile": { "opt_level": "3", "debug_assertions": true, "overflow_checks": false, "debuginfo": 0 },
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
            "profile": { "opt_level": "3", "debug_assertions": false, "overflow_checks": true, "debuginfo": 0 },
            "executable": "/target/release/neo"
        })
        .to_string();
        let error = release_executable_from_messages(messages.as_bytes())
            .unwrap_err()
            .to_string();
        assert!(error.contains("overflow_checks=true"));
    }

    #[test]
    fn accepts_in_tree_release_profile_tuning() {
        let fixture = Fixture::valid();
        let cargo_home = fixture.root.join("cargo-home");
        fs::write(
            fixture.root.join("Cargo.toml"),
            "[profile.release]\nlto = \"thin\"\ncodegen-units = 1\nstrip = \"symbols\"\n",
        )
        .unwrap();
        scan_release_configuration(&fixture.root, &cargo_home, &test_target()).unwrap();
    }

    #[test]
    fn accepts_in_tree_release_profile_configuration() {
        let fixture = Fixture::valid();
        let cargo_home = fixture.root.join("cargo-home");
        fs::create_dir_all(fixture.root.join(".cargo")).unwrap();
        fs::write(
            fixture.root.join(".cargo/config.toml"),
            "[profile.release]\nlto = \"thin\"\n",
        )
        .unwrap();
        scan_release_configuration(&fixture.root, &cargo_home, &test_target()).unwrap();
    }

    #[test]
    fn rejects_out_of_tree_release_profile_configuration() {
        let fixture = Fixture::valid();
        let cargo_home = fixture.root.with_extension("cargo-home-profile");
        fs::create_dir_all(&cargo_home).unwrap();
        fs::write(
            cargo_home.join("config.toml"),
            "[profile.release]\ndebug-assertions = true\n",
        )
        .unwrap();
        let error = scan_release_configuration(&fixture.root, &cargo_home, &test_target())
            .unwrap_err()
            .to_string();
        fs::remove_dir_all(cargo_home).unwrap();
        assert!(error.contains("profile.release.debug-assertions"));
        assert!(error.contains("out-of-tree"));
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
        for option in [
            "opt-level=0",
            "debug-assertions=on",
            "overflow-checks=on",
            "debuginfo=2",
            "panic=abort",
            "strip=none",
            "lto=off",
            "codegen-units=256",
        ] {
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
    fn accepts_in_tree_rustflags_and_matches_target_source() {
        let fixture = Fixture::valid();
        let cargo_home = fixture.root.join("cargo-home");
        fs::create_dir_all(fixture.root.join(".cargo")).unwrap();
        fs::write(
            fixture.root.join(".cargo/config.toml"),
            "[build]\nrustflags = [\"--cfg\", \"from_config\"]\n[target.aarch64-apple-darwin]\nrustflags = [\"-C\", \"link-arg=-Wl,-oso_prefix,/source\"]\n",
        )
        .unwrap();
        let configuration =
            scan_release_configuration(&fixture.root, &cargo_home, &test_target()).unwrap();
        assert!(configuration.has_matching_target_rustflags);
        assert_eq!(
            configuration.in_tree_build_rustflags,
            ["--cfg", "from_config"].map(OsString::from)
        );
    }

    #[test]
    fn accepts_nonmatching_target_rustflags() {
        let fixture = Fixture::valid();
        let cargo_home = fixture.root.join("cargo-home");
        fs::create_dir_all(fixture.root.join(".cargo")).unwrap();
        fs::write(
            fixture.root.join(".cargo/config.toml"),
            "[target.x86_64-unknown-linux-gnu]\nrustflags = [\"-C\", \"linker=clang\"]\n",
        )
        .unwrap();
        let configuration =
            scan_release_configuration(&fixture.root, &cargo_home, &test_target()).unwrap();
        assert!(!configuration.has_matching_target_rustflags);
    }

    #[test]
    fn rejects_out_of_tree_build_rustflags_through_include() {
        let fixture = Fixture::valid();
        let cargo_home = fixture.root.with_extension("cargo-home-include");
        fs::create_dir_all(&cargo_home).unwrap();
        fs::write(
            cargo_home.join("config.toml"),
            "include = [\"other.toml\"]\n",
        )
        .unwrap();
        fs::write(
            cargo_home.join("other.toml"),
            "[build]\nrustflags = [\"-C\", \"opt-level=0\"]\n",
        )
        .unwrap();
        let error = scan_release_configuration(&fixture.root, &cargo_home, &test_target())
            .unwrap_err()
            .to_string();
        fs::remove_dir_all(cargo_home).unwrap();
        assert!(error.contains("build.rustflags"));
        assert!(error.contains("other.toml"));
    }

    #[test]
    fn rejects_profile_codegen_rustflags_in_tree() {
        let fixture = Fixture::valid();
        let cargo_home = fixture.root.join("cargo-home");
        fs::create_dir_all(fixture.root.join(".cargo")).unwrap();
        fs::write(
            fixture.root.join(".cargo/config.toml"),
            "[target.'cfg(all(unix, target_os = \"macos\"))']\nrustflags = [\"-C\", \"debuginfo=2\"]\n",
        )
        .unwrap();
        let error = scan_release_configuration(&fixture.root, &cargo_home, &test_target())
            .unwrap_err()
            .to_string();
        assert!(error.contains("debuginfo"));
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
    fn requires_an_empty_notary_issue_list() {
        assert!(validate_notary_log(br#"{"issues":[]}"#).is_ok());
        assert!(validate_notary_log(br#"{"issues":null}"#).is_ok());
        assert!(validate_notary_log(br#"{"issues":[{"severity":"warning"}]}"#).is_err());
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
    fn rejects_executable_with_different_cpu_subtype() {
        let fixture = Fixture::valid();
        let mut executable = test_macho(0x0100_000c, TEST_UUID);
        executable[8..12].copy_from_slice(&2u32.to_le_bytes());
        fs::write(fixture.executable(), executable).unwrap();
        let error = fixture.verify().unwrap_err().to_string();
        assert!(error.contains("CPU subtype"));
    }

    #[test]
    fn rejects_executable_with_different_uuid() {
        let fixture = Fixture::valid();
        fs::write(fixture.executable(), test_macho(0x0100_000c, [0x55; 16])).unwrap();
        let error = fixture.verify_stable_identity().unwrap_err().to_string();
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
    fn rejects_appended_executable_bytes_by_hash() {
        let fixture = Fixture::valid();
        fixture.append_to_executable(b"AAAAAAA");
        let error = fixture.verify().unwrap_err().to_string();
        assert!(error.contains("SHA-256"));
    }

    #[test]
    fn rejects_private_markers_in_bundle_files() {
        let home = std::env::var_os("HOME").unwrap();
        let markers = [(home.as_bytes(), "private home path")];
        let error = reject_file_markers(
            home.as_bytes(),
            &markers,
            Path::new("NEO.app/Contents/Info.plist"),
            "contents",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("private home path"));
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
    fn rejects_incorrect_required_plist_metadata() {
        for key in [
            "CFBundlePackageType",
            "CFBundleIdentifier",
            "CFBundleShortVersionString",
            "LSMinimumSystemVersion",
        ] {
            let fixture = Fixture::valid();
            let path = fixture.app.join("Contents/Info.plist");
            let mut dictionary = Value::from_file(&path).unwrap().into_dictionary().unwrap();
            dictionary.insert(key.into(), "wrong".into());
            Value::Dictionary(dictionary).to_file_xml(path).unwrap();
            let error = fixture.verify().unwrap_err().to_string();
            assert!(error.contains(key));
        }
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
