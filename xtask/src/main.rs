use plist::Value;
use std::error::Error;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Cursor};
use std::os::unix::fs::PermissionsExt;
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

fn package(profile: PackageProfile) -> Result<()> {
    let root = repository_root();
    std::env::set_current_dir(&root)?;
    require_tool_version("packager", PACKAGER_VERSION)?;
    require_tool_version("about", ABOUT_VERSION)?;
    generate_notices()?;
    let mut build = Command::new("cargo");
    build.args(["build", "--locked", "--package", "neo"]);
    profile.apply(&mut build);
    run_command(&mut build)?;
    let mut packager = Command::new("cargo");
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
    run_command(Command::new("codesign").args([
        "--verify",
        "--deep",
        "--strict",
        "--verbose=2",
        app.as_os_str().to_str().expect("UTF-8 app path"),
    ]))?;

    let assessment = Command::new("spctl")
        .args(["-a", "-vvv", "-t", "install"])
        .arg(&app)
        .status()?;
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

fn signing_identity_label(identity: &str) -> &str {
    identity
        .strip_suffix(')')
        .and_then(|identity| identity.rsplit_once(" (").map(|(label, _)| label))
        .unwrap_or(identity)
}

fn nested_code(app: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    collect_nested_code(app, app, &mut paths)?;
    paths.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    paths.dedup();
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
    run_command(Command::new("codesign").args([
        "--verify",
        "--deep",
        "--strict",
        "--verbose=2",
        app.as_os_str().to_str().expect("UTF-8 app path"),
    ]))?;

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
        .args([
            "notarytool",
            "submit",
            archive.as_os_str().to_str().expect("UTF-8 archive path"),
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
    run_command(
        Command::new("spctl")
            .args(["-a", "-vvv", "-t", "install"])
            .arg(&app),
    )?;
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
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(OsStr::to_string_lossy)
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
            fs::create_dir_all(root.join("packaging/generated")).unwrap();
            fs::write(root.join("LICENSE"), b"license\n").unwrap();
            fs::write(app.join(BUNDLED_LICENSE), b"license\n").unwrap();
            fs::write(root.join(NOTICES), b"# Notices\n\n- [crate 1.0.0](url)\n").unwrap();
            fs::write(
                app.join(BUNDLED_NOTICES),
                b"# Notices\n\n- [crate 1.0.0](url)\n",
            )
            .unwrap();
            let mut dictionary = Dictionary::new();
            dictionary.insert("NSHumanReadableCopyright".into(), EXPECTED_COPYRIGHT.into());
            Value::Dictionary(dictionary)
                .to_file_xml(app.join("Contents/Info.plist"))
                .unwrap();
            Self { root, app }
        }

        fn verify(&self) -> Result<usize> {
            verify_bundle(&self.root, &self.app)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).unwrap();
        }
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
