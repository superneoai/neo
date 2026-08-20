use plist::Value;
use std::error::Error;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const EXPECTED_COPYRIGHT: &str = "Copyright © 2026 ACTUAL LTD.";
const PACKAGER_VERSION: &str = "cargo-packager 0.11.8";
const ABOUT_VERSION: &str = "cargo-about 0.8.2";
const NOTICES: &str = "packaging/generated/THIRD-PARTY-NOTICES.md";
const BUNDLED_NOTICES: &str = "Contents/Resources/Legal/THIRD-PARTY-NOTICES.md";
const BUNDLED_LICENSE: &str = "Contents/Resources/Legal/AGPL-3.0-or-later.txt";

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
    match (arguments.next().as_deref(), arguments.next()) {
        (Some(command), None) if command == "package" => package(),
        _ => Err(failure("usage: cargo xtask package")),
    }
}

fn package() -> Result<()> {
    let root = repository_root();
    std::env::set_current_dir(&root)?;
    require_tool_version("packager", PACKAGER_VERSION)?;
    require_tool_version("about", ABOUT_VERSION)?;
    generate_notices()?;
    run_command(Command::new("cargo").args(["packager", "--packages", "neo"]))?;

    let app = root.join("dist/NEO.app");
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

    Ok(notices
        .lines()
        .filter(|line| line.starts_with("- ["))
        .count())
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
