use plist::Value;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Cursor};
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

        let mut rustflags = std::env::var_os("CARGO_ENCODED_RUSTFLAGS")
            .map(|flags| {
                flags
                    .as_bytes()
                    .split(|byte| *byte == 0x1f)
                    .filter(|flag| !flag.is_empty())
                    .map(|flag| OsStr::from_bytes(flag).to_os_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
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
            .env("NEO_RELEASE_CARGO_HOME", &self.cargo_home)
            .env("NEO_RELEASE_CARGO_ALIAS", &self.cargo_home_alias)
            .env("NEO_RELEASE_TARGET", &self.target)
            .env("NEO_RELEASE_TARGET_ALIAS", &self.target_alias)
            .env("PATH", &self.path)
            .env_remove("CARGO_ENCODED_RUSTFLAGS");
    }
}

fn prepare_xcrun_wrapper(directory: &Path) -> Result<()> {
    const WRAPPER: &str = r#"#!/bin/sh
if [ "$#" -ge 3 ] && [ "$1" = "-sdk" ] && [ "$3" = "metal" ]; then
    exec /usr/bin/xcrun "$@" \
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
    if !matches!(fs::read(&path), Ok(contents) if contents == WRAPPER.as_bytes()) {
        fs::write(&path, WRAPPER)?;
    }
    let mut permissions = fs::metadata(&path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)?;
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
    let mut build = Command::new("cargo");
    if let Some(environment) = &build_environment {
        environment.apply(&mut build);
    }
    build.args(["build", "--locked", "--package", "neo"]);
    profile.apply(&mut build);
    run_command(&mut build)?;
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
    let attribution_count = verify_bundle(&root, &app)?;
    println!(
        "verified {} with {attribution_count} crate attributions",
        app.display()
    );
    Ok(())
}

fn sign() -> Result<()> {
    let root = repository_root();
    std::env::set_current_dir(&root)?;
    let app = root.join(APP);
    if !app.is_dir() {
        return Err(failure(format!("missing app bundle: {}", app.display())));
    }
    let attribution_count = verify_bundle(&root, &app)?;
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
    let attribution_count = verify_bundle(&root, &app)?;
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

fn verify_bundle(root: &Path, app: &Path) -> Result<usize> {
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
    verify_bundled_executable(app, dictionary)?;

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

fn verify_bundled_executable(app: &Path, dictionary: &plist::Dictionary) -> Result<()> {
    let executable_name = dictionary
        .get("CFBundleExecutable")
        .and_then(Value::as_string)
        .filter(|name| Path::new(name).file_name() == Some(OsStr::new(name)))
        .ok_or_else(|| failure("bundle has no valid CFBundleExecutable"))?;
    let executable = app.join("Contents/MacOS").join(executable_name);
    let bytes = fs::read(&executable).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "missing bundled executable: {}: {error}",
                executable.display()
            ),
        )
    })?;
    let home = std::env::var_os("HOME").ok_or_else(|| failure("HOME is not set"))?;
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
    if let Some(identifier) = darwin_temp_identifier(&std::env::temp_dir()) {
        reject_executable_marker(
            &bytes,
            identifier.as_bytes(),
            "Darwin temporary directory identifier",
            &executable,
        )?;
    }
    for marker in [
        "attempt to add with overflow",
        "attempt to subtract with overflow",
        "attempt to multiply with overflow",
        "attempt to divide with overflow",
        "attempt to shift left with overflow",
    ] {
        reject_executable_marker(
            &bytes,
            marker.as_bytes(),
            &format!("debug-assertion panic string {marker:?}"),
            &executable,
        )?;
    }
    Ok(())
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
    let status = command.status()?;
    if !status.success() {
        return Err(failure(format!("{display} exited with {status}")));
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
            fs::write(root.join("LICENSE"), b"license\n").unwrap();
            fs::write(app.join(BUNDLED_LICENSE), b"license\n").unwrap();
            fs::write(root.join(NOTICES), b"# Notices\n\n- [crate 1.0.0](url)\n").unwrap();
            fs::write(
                app.join(BUNDLED_NOTICES),
                b"# Notices\n\n- [crate 1.0.0](url)\n",
            )
            .unwrap();
            fs::write(app.join("Contents/MacOS/neo"), b"release executable").unwrap();
            let mut dictionary = Dictionary::new();
            dictionary.insert("CFBundleExecutable".into(), "neo".into());
            dictionary.insert("NSHumanReadableCopyright".into(), EXPECTED_COPYRIGHT.into());
            Value::Dictionary(dictionary)
                .to_file_xml(app.join("Contents/Info.plist"))
                .unwrap();
            Self { root, app }
        }

        fn verify(&self) -> Result<usize> {
            verify_bundle(&self.root, &self.app)
        }

        fn executable(&self) -> PathBuf {
            self.app.join("Contents/MacOS/neo")
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
    fn rejects_private_home_path_in_executable() {
        let fixture = Fixture::valid();
        let home = std::env::var_os("HOME").unwrap();
        let mut bytes = b"release executable ".to_vec();
        bytes.extend_from_slice(home.as_bytes());
        fs::write(fixture.executable(), bytes).unwrap();
        let error = fixture.verify().unwrap_err().to_string();
        assert!(error.contains("private home path prefix"));
    }

    #[test]
    fn rejects_darwin_temp_identifier_in_executable() {
        let fixture = Fixture::valid();
        let temporary_directory = std::env::temp_dir();
        let identifier = darwin_temp_identifier(&temporary_directory).unwrap();
        let mut bytes = b"release executable ".to_vec();
        bytes.extend_from_slice(identifier.as_bytes());
        fs::write(fixture.executable(), bytes).unwrap();
        let error = fixture.verify().unwrap_err().to_string();
        assert!(error.contains("Darwin temporary directory identifier"));
    }

    #[test]
    fn rejects_debug_assertion_panic_string_in_executable() {
        let fixture = Fixture::valid();
        fs::write(
            fixture.executable(),
            b"release executable attempt to add with overflow",
        )
        .unwrap();
        let error = fixture.verify().unwrap_err().to_string();
        assert!(error.contains("debug-assertion panic string"));
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
