//! Running external commands and building components from source.
//!
//! All process spawning goes through [`CommandRunner`] so `build_from_source`
//! can be exercised with a fake in tests.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::manifest::ComponentManifest;
use crate::platform::Platform;

/// Thin abstraction over "is this program on PATH?" and "run it".
pub trait CommandRunner {
    fn which(&self, program: &str) -> Option<PathBuf>;
    fn run(&self, program: &str, args: &[String], cwd: &Path) -> Result<(), String>;
}

/// The real runner backed by `std::process`.
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn which(&self, program: &str) -> Option<PathBuf> {
        which(program)
    }

    fn run(&self, program: &str, args: &[String], cwd: &Path) -> Result<(), String> {
        let status = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .status()
            .map_err(|e| format!("failed to run `{}`: {}", program, e))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!(
                "`{} {}` exited with {}",
                program,
                args.join(" "),
                status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_string())
            ))
        }
    }
}

/// Locate an executable on `PATH`, honouring `PATHEXT` on Windows.
pub fn which(program: &str) -> Option<PathBuf> {
    let candidate = Path::new(program);
    if candidate.components().count() > 1 {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }

    let path = env::var_os("PATH")?;
    let extensions: Vec<String> = if cfg!(windows) {
        env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_string())
            .split(';')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
            .collect()
    } else {
        vec![String::new()]
    };

    for dir in env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for ext in &extensions {
            let file = dir.join(format!("{}{}", program, ext));
            if file.is_file() {
                return Some(file);
            }
        }
    }
    None
}

/// Return the path to Homebrew's `brew` if the runner can find it.
pub fn detect_brew(runner: &dyn CommandRunner) -> Option<PathBuf> {
    runner.which("brew")
}

fn rust_remediation() -> String {
    let hint = match env::consts::OS {
        "windows" => {
            "install the Visual Studio C++ Build Tools, then Rust via rustup (https://rustup.rs)"
        }
        "macos" => {
            "install the Xcode Command Line Tools (`xcode-select --install`), then Rust via rustup (https://rustup.rs)"
        }
        _ => "install a C toolchain (e.g. `build-essential`), then Rust via rustup (https://rustup.rs)",
    };
    format!("`cargo` was not found: {}", hint)
}

fn build_tools_remediation(missing: &[String]) -> String {
    format!(
        "missing required build tools: {}. Install them with your package manager \
         (on macOS: `brew install {}`; on Debian/Ubuntu: `apt install {}`)",
        missing.join(", "),
        missing.join(" "),
        missing.join(" ")
    )
}

/// Install `packages` with Homebrew, but only with consent: automatically when
/// `assume_yes`, otherwise after an interactive `[y/N]` prompt (when
/// `interactive`), otherwise fail with `remediation`.
///
/// This is the single place that runs `brew install`, so no code path can
/// install anything without the operator having agreed to it.
fn brew_install_or_err(
    runner: &dyn CommandRunner,
    source: &Path,
    allow_brew: bool,
    assume_yes: bool,
    interactive: bool,
    packages: &[String],
    remediation: impl FnOnce() -> String,
) -> Result<(), String> {
    let Some(brew) = allow_brew.then(|| detect_brew(runner)).flatten() else {
        return Err(remediation());
    };
    let consented = assume_yes
        || (interactive
            && confirm(&format!(
                "Homebrew is available. Install {}?",
                packages.join(" ")
            )));
    if !consented {
        return Err(remediation());
    }
    let brew = brew.to_string_lossy().to_string();
    let mut args = vec!["install".to_string()];
    args.extend(packages.iter().cloned());
    runner
        .run(&brew, &args, source)
        .map_err(|e| format!("`brew install {}` failed: {}", packages.join(" "), e))
}

/// Ask a yes/no question on the terminal. Anything but `y`/`yes` is a no, and a
/// read failure is a no — the safe default when we cannot get an answer.
fn confirm(question: &str) -> bool {
    use std::io::{self, Write};
    eprint!("{} [y/N] ", question);
    let _ = io::stderr().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Build `source` and return the path to the produced binary.
///
/// * `allow_brew` — whether the caller opted into Homebrew-assisted installs.
/// * `assume_yes` — whether we may run a brew install without a prompt.
/// * `interactive` — whether we may prompt the user (stdin is a TTY and the
///   caller did not pass `--yes`). Only consulted when `assume_yes` is false.
pub fn build_from_source(
    runner: &dyn CommandRunner,
    source: &Path,
    manifest: &ComponentManifest,
    allow_brew: bool,
    assume_yes: bool,
    interactive: bool,
) -> Result<PathBuf, String> {
    if runner.which("cargo").is_none() {
        brew_install_or_err(
            runner,
            source,
            allow_brew,
            assume_yes,
            interactive,
            &["rustup".to_string()],
            || format!("{}; or run `brew install rustup`", rust_remediation()),
        )?;
        if runner.which("cargo").is_none() {
            return Err(format!(
                "{} (brew install rustup ran but cargo is still not on PATH; \
                 you may need to open a new shell)",
                rust_remediation()
            ));
        }
    }

    let mut missing: Vec<String> = Vec::new();
    if runner.which("cmake").is_none() {
        missing.push("cmake".to_string());
    }
    if env::consts::OS == "linux" && runner.which("pkg-config").is_none() {
        missing.push("pkg-config".to_string());
    }
    if !missing.is_empty() {
        brew_install_or_err(
            runner,
            source,
            allow_brew,
            assume_yes,
            interactive,
            &missing,
            || build_tools_remediation(&missing),
        )?;
    }

    let (program, args) = match &manifest.build_command {
        Some(command) => (command.clone(), manifest.build_flags.clone()),
        None => (
            "cargo".to_string(),
            vec!["build".to_string(), "--release".to_string()],
        ),
    };
    runner
        .run(&program, &args, source)
        .map_err(|e| format!("build failed for {}: {}", manifest.name, e))?;

    resolve_built_binary(source, manifest, &Platform::current())
}

/// Locate the binary produced by a build, preferring the manifest's per-platform
/// `binary` route and falling back to conventional `target/release/<name>`
/// locations.
pub fn resolve_built_binary(
    source: &Path,
    manifest: &ComponentManifest,
    platform: &Platform,
) -> Result<PathBuf, String> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Some(route) = manifest.binary_for(platform) {
        let route_path = Path::new(route);
        candidates.push(source.join(route_path));
        if !route.starts_with("target/") {
            if let Some(name) = route_path.file_name() {
                candidates.push(source.join("target").join("release").join(name));
            }
        }
    }

    let names = [manifest.name.clone(), manifest.name.replace('-', "_")];
    for name in &names {
        candidates.push(source.join("target").join("release").join(name));
        candidates.push(
            source
                .join("target")
                .join("release")
                .join(format!("{}.exe", name)),
        );
    }

    for candidate in &candidates {
        if candidate.is_file() {
            return Ok(candidate.clone());
        }
    }

    let searched = candidates
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "build succeeded but no binary was found for {}; searched: {}",
        manifest.name, searched
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeRunner {
        present: Vec<String>,
        runs: Mutex<Vec<(String, Vec<String>)>>,
        create: Option<(String, Vec<u8>)>,
    }

    impl CommandRunner for FakeRunner {
        fn which(&self, program: &str) -> Option<PathBuf> {
            self.present
                .iter()
                .any(|p| p == program)
                .then(|| PathBuf::from(program))
        }

        fn run(&self, program: &str, args: &[String], cwd: &Path) -> Result<(), String> {
            self.runs
                .lock()
                .unwrap()
                .push((program.to_string(), args.to_vec()));
            if let Some((rel, bytes)) = &self.create {
                let path = cwd.join(rel);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).unwrap();
                }
                std::fs::write(path, bytes).unwrap();
            }
            Ok(())
        }
    }

    fn manifest_with_binary() -> ComponentManifest {
        serde_json::from_str(
            r#"{
                "name": "engine", "version": "0.1.0", "kind": "engine",
                "build_command": "cargo", "build_flags": ["build", "--release"],
                "binary": { "macos": { "aarch64": "target/release/cockatiel-engine-rs" } }
            }"#,
        )
        .unwrap()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cockatiel_build_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn build_runs_manifest_command_and_finds_binary() {
        let source = temp_dir("ok");
        let runner = FakeRunner {
            present: vec!["cargo".to_string(), "cmake".to_string()],
            create: Some((
                "target/release/cockatiel-engine-rs".to_string(),
                b"bin".to_vec(),
            )),
            ..Default::default()
        };
        let manifest = manifest_with_binary();
        let platform = Platform::new("macos", "aarch64");
        let _ = platform;
        let bin = build_from_source(&runner, &source, &manifest, false, false, false).unwrap();
        assert!(bin.ends_with("target/release/cockatiel-engine-rs"));
        let runs = runner.runs.lock().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].0, "cargo");
        assert_eq!(runs[0].1, vec!["build", "--release"]);
        let _ = std::fs::remove_dir_all(&source);
    }

    #[test]
    fn missing_cargo_returns_rustup_remediation() {
        let source = temp_dir("nocargo");
        let runner = FakeRunner::default();
        let err = build_from_source(&runner, &source, &manifest_with_binary(), false, false, false)
            .unwrap_err();
        assert!(err.contains("rustup.rs"), "{}", err);
        let _ = std::fs::remove_dir_all(&source);
    }

    #[test]
    fn missing_cargo_with_brew_yes_runs_brew_install() {
        let source = temp_dir("brewrust");
        let runner = FakeRunner {
            present: vec!["brew".to_string()],
            ..Default::default()
        };
        let err = build_from_source(&runner, &source, &manifest_with_binary(), true, true, false)
            .unwrap_err();
        assert!(err.contains("cargo"), "{}", err);
        let runs = runner.runs.lock().unwrap();
        assert_eq!(runs[0].0, "brew");
        assert_eq!(runs[0].1, vec!["install", "rustup"]);
        let _ = std::fs::remove_dir_all(&source);
    }

    #[test]
    fn missing_cmake_returns_package_remediation() {
        let source = temp_dir("nocmake");
        let runner = FakeRunner {
            present: vec!["cargo".to_string()],
            ..Default::default()
        };
        let err =
            build_from_source(&runner, &source, &manifest_with_binary(), false, false, false).unwrap_err();
        assert!(err.contains("cmake"), "{}", err);
        let _ = std::fs::remove_dir_all(&source);
    }

    #[test]
    fn missing_cmake_with_brew_installs_it() {
        let source = temp_dir("brewcmake");
        let runner = FakeRunner {
            present: vec!["cargo".to_string(), "brew".to_string()],
            create: Some((
                "target/release/cockatiel-engine-rs".to_string(),
                b"bin".to_vec(),
            )),
            ..Default::default()
        };
        build_from_source(&runner, &source, &manifest_with_binary(), true, true, false).unwrap();
        let runs = runner.runs.lock().unwrap();
        assert_eq!(runs[0].0, "brew");
        assert_eq!(runs[0].1, vec!["install", "cmake"]);
        assert_eq!(runs[1].0, "cargo");
        let _ = std::fs::remove_dir_all(&source);
    }

    #[test]
    fn missing_cmake_with_brew_but_no_consent_does_not_install() {
        // Homebrew is present and allowed, but the operator neither passed
        // --yes nor is on an interactive stdin: nothing may be installed.
        let source = temp_dir("brewcmake_noconsent");
        let runner = FakeRunner {
            present: vec!["cargo".to_string(), "brew".to_string()],
            ..Default::default()
        };
        let err =
            build_from_source(&runner, &source, &manifest_with_binary(), true, false, false)
                .unwrap_err();
        assert!(err.contains("cmake"), "{}", err);
        assert!(
            runner.runs.lock().unwrap().is_empty(),
            "brew must not run without consent"
        );
        let _ = std::fs::remove_dir_all(&source);
    }

    #[test]
    fn resolve_reports_what_it_searched() {
        let source = temp_dir("nosearch");
        let manifest: ComponentManifest =
            serde_json::from_str(r#"{"name":"thing","version":"1"}"#).unwrap();
        let err =
            resolve_built_binary(&source, &manifest, &Platform::new("linux", "x86_64")).unwrap_err();
        assert!(err.contains("no binary was found"), "{}", err);
        assert!(err.contains("thing"), "{}", err);
        let _ = std::fs::remove_dir_all(&source);
    }

    #[test]
    fn which_finds_a_real_program() {
        #[cfg(unix)]
        assert!(which("sh").is_some());
    }
}
