mod build;
mod doctor;
mod extract;
mod fetch;
mod install;
mod lockfile;
mod manifest;
mod package;
mod paths;
mod platform;
mod resolve;

use std::path::{Path, PathBuf};
use std::process::Command;

use build::SystemRunner;
use doctor::{DoctorOptions, DoctorReport, Mode};
use extract::ArchiveExtractor;
use fetch::SystemFetcher;
use install::{install_all, InstallOptions, Report};
use lockfile::Lock;
use manifest::ComponentManifest;
use package::{package, PackageOptions};
use paths::{default_install_root, Layout, Target};
use platform::Platform;
use resolve::{resolve, Overrides};

const USAGE: &str = r#"cockatiel — bootstrap launcher (phase 3)

USAGE:
  cockatiel --help
  cockatiel list --lock <path>
  cockatiel plan --manifest <path> --lock <path> [--component <name>]
                 [--os <os>] [--arch <arch>]
                 [--release-base <url>] [--component-version <v>] [--source <dir>]
  cockatiel package --manifest <path> --binary <path> --out-dir <dir>
                    [--os <os>] [--arch <arch>] [--version <v>] [--write]
  cockatiel install [--root <dir>] [--lock <path>] [--repo-root <dir>]
                    [--component <name>]... [--force] [--yes] [--no-brew]
                    [--os <os>] [--arch <arch>]
                    [--release-base <url>] [--component-version <v>] [--source <dir>]
  cockatiel run [--root <dir>] [--lock <path>] [--repo-root <dir>]
                [install flags...] [-- <tui args>...]
  cockatiel doctor [--root <dir>] [--test-runner <path>] [--modules-dir <dir>]
                   [--quick] [--soak] [--duration-secs <n>] [--iterations <n>]
                   [--mode paused|unpaused|both] [--json] [--keep]

COMMANDS:
  list     Print the components recorded in a lockfile.
  plan     Resolve one component to a download/build plan and print it as JSON.
  package  Archive a built binary, hash it, and emit its release.assets entry.
  install  Download (or build) and install every locked component.
  run      Install, then launch <root>/bin/cockatiel-tui-v2 with any args after `--`.
  doctor   Boot an isolated stack, run the compliance suites, and report.

INSTALL ROOT (highest precedence first):
  --root <dir>
  $COCKATIEL_HOME
  macOS   ~/Library/Application Support/cockatiel
  Linux   $XDG_DATA_HOME/cockatiel, else ~/.local/share/cockatiel
  Windows %APPDATA%\cockatiel

OPTIONS:
  --root <dir>             install root (default: see above)
  --lock <path>            path to cockatiel.lock (default: <repo-root>/cockatiel.lock)
  --repo-root <dir>        checkout containing component dirs (default: current dir)
  --manifest <path>        path to cockatiel_module_info.json (plan/package)
  --component <name>       restrict install to this component (repeatable)
  --binary <path>          package: built component binary to archive
  --out-dir <dir>          package: directory to write the archive into
  --version <v>            package: override the manifest version
  --write                  package: patch the manifest's release.assets in place
  --force                  reinstall even when already up to date
  --yes                    do not prompt (allows `brew install` when permitted)
  --no-brew                never use Homebrew to satisfy build prerequisites
  --os <os>                override the platform OS (macos/windows/linux)
  --arch <arch>            override the platform arch (aarch64/x86_64/arm/x86)
  --release-base <url>     override the release base_url
  --component-version <v>  override the component version
  --source <dir>           force a local build from this directory
  --test-runner <path>     doctor: test-runner binary (default <root>/bin/...)
  --modules-dir <dir>      doctor: modules directory (default <root>/modules)
  --quick                  doctor: run only the `screening` suite
  --soak                   doctor: also run the `soak` suite
  --duration-secs <n>      doctor: soak window in seconds (default 30)
  --iterations <n>         doctor: messages per burst (default 100)
  --mode <mode>            doctor: paused | unpaused | both (default both)
  --json                   doctor: also print the full report as JSON
  --keep                   doctor: keep the sandbox for inspection
  --help, -h               show this help
"#;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("--help") | Some("-h") | None => {
            print!("{}", USAGE);
            0
        }
        Some("list") => report_exit(cmd_list(&args[1..])),
        Some("plan") => report_exit(cmd_plan(&args[1..])),
        Some("package") => match cmd_package(&args[1..]) {
            Ok(code) => code,
            Err(err) => {
                eprintln!("error: {}", err);
                1
            }
        },
        Some("install") => match cmd_install(&args[1..]) {
            Ok(code) => code,
            Err(err) => {
                eprintln!("error: {}", err);
                1
            }
        },
        Some("run") => match cmd_run(&args[1..]) {
            Ok(code) => code,
            Err(err) => {
                eprintln!("error: {}", err);
                1
            }
        },
        Some("doctor") => match cmd_doctor(&args[1..]) {
            Ok(code) => code,
            Err(err) => {
                eprintln!("error: {}", err);
                1
            }
        },
        Some(other) => {
            eprintln!("error: unknown command: {}", other);
            1
        }
    };
    std::process::exit(code);
}

fn report_exit(result: Result<(), String>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("error: {}", err);
            1
        }
    }
}

fn value_after(args: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
    match args.get(*i + 1) {
        Some(value) => {
            *i += 2;
            Ok(value.clone())
        }
        None => Err(format!("{} requires a value", flag)),
    }
}

fn cmd_list(args: &[String]) -> Result<(), String> {
    let mut lock_path: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--lock" => lock_path = Some(PathBuf::from(value_after(args, &mut i, "--lock")?)),
            "--help" | "-h" => {
                print!("{}", USAGE);
                return Ok(());
            }
            other => return Err(format!("unknown argument: {}", other)),
        }
    }

    let lock_path = lock_path.ok_or("list requires --lock <path>")?;
    let lock = Lock::load(&lock_path)?;
    for (key, component) in &lock.components {
        let sha = component.sha.as_deref().unwrap_or("-");
        println!("{:<16} {:<10} {}", key, component.version, sha);
    }
    Ok(())
}

fn cmd_plan(args: &[String]) -> Result<(), String> {
    let mut manifest_path: Option<PathBuf> = None;
    let mut lock_path: Option<PathBuf> = None;
    let mut component: Option<String> = None;
    let mut os: Option<String> = None;
    let mut arch: Option<String> = None;
    let mut overrides = Overrides::default();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--manifest" => {
                manifest_path = Some(PathBuf::from(value_after(args, &mut i, "--manifest")?))
            }
            "--lock" => lock_path = Some(PathBuf::from(value_after(args, &mut i, "--lock")?)),
            "--component" => component = Some(value_after(args, &mut i, "--component")?),
            "--os" => os = Some(value_after(args, &mut i, "--os")?),
            "--arch" => arch = Some(value_after(args, &mut i, "--arch")?),
            "--release-base" => {
                overrides.release_base = Some(value_after(args, &mut i, "--release-base")?)
            }
            "--component-version" => {
                overrides.component_version = Some(value_after(args, &mut i, "--component-version")?)
            }
            "--source" => overrides.source = Some(PathBuf::from(value_after(args, &mut i, "--source")?)),
            "--help" | "-h" => {
                print!("{}", USAGE);
                return Ok(());
            }
            other => return Err(format!("unknown argument: {}", other)),
        }
    }

    let manifest_path = manifest_path.ok_or("plan requires --manifest <path>")?;
    let lock_path = lock_path.ok_or("plan requires --lock <path>")?;

    let manifest = ComponentManifest::load(&manifest_path)?;
    let lock = Lock::load(&lock_path)?;
    let key = component.unwrap_or_else(|| manifest.name.clone());
    let locked = lock.components.get(&key);

    let platform = platform_from(os, arch);

    let manifest_dir = manifest_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    let plan = resolve(&manifest, manifest_dir, locked, &platform, &overrides);
    let json = serde_json::to_string_pretty(&plan).map_err(|e| e.to_string())?;
    println!("{}", json);
    Ok(())
}

fn cmd_package(args: &[String]) -> Result<i32, String> {
    if wants_help(args) {
        print!("{}", USAGE);
        return Ok(0);
    }

    let mut manifest_path: Option<PathBuf> = None;
    let mut binary: Option<PathBuf> = None;
    let mut out_dir: Option<PathBuf> = None;
    let mut os: Option<String> = None;
    let mut arch: Option<String> = None;
    let mut version: Option<String> = None;
    let mut write = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--manifest" => {
                manifest_path = Some(PathBuf::from(value_after(args, &mut i, "--manifest")?))
            }
            "--binary" => binary = Some(PathBuf::from(value_after(args, &mut i, "--binary")?)),
            "--out-dir" => out_dir = Some(PathBuf::from(value_after(args, &mut i, "--out-dir")?)),
            "--os" => os = Some(value_after(args, &mut i, "--os")?),
            "--arch" => arch = Some(value_after(args, &mut i, "--arch")?),
            "--version" => version = Some(value_after(args, &mut i, "--version")?),
            "--write" => {
                write = true;
                i += 1;
            }
            "--help" | "-h" => {
                print!("{}", USAGE);
                return Ok(0);
            }
            other => return Err(format!("unknown argument: {}", other)),
        }
    }

    let manifest_path = manifest_path.ok_or("package requires --manifest <path>")?;
    let binary = binary.ok_or("package requires --binary <path>")?;
    let out_dir = out_dir.ok_or("package requires --out-dir <dir>")?;

    let manifest = ComponentManifest::load(&manifest_path)?;
    let effective_version = version
        .clone()
        .unwrap_or_else(|| manifest.version.clone());

    let opts = PackageOptions {
        manifest_path,
        binary,
        out_dir,
        platform: platform_from(os, arch),
        version,
        write_manifest: write,
    };
    let output = package(&opts)?;

    println!(
        "packaged {} {} -> {} ({})",
        manifest.name,
        effective_version,
        output.archive.display(),
        output.sha256
    );
    println!("__PACKAGE_JSON__");
    println!(
        "{}",
        serde_json::to_string_pretty(&output).map_err(|e| e.to_string())?
    );
    Ok(0)
}

fn platform_from(os: Option<String>, arch: Option<String>) -> Platform {
    match (os, arch) {
        (Some(os), Some(arch)) => Platform::new(&os, &arch),
        (Some(os), None) => Platform {
            os,
            arch: Platform::current().arch,
        },
        (None, Some(arch)) => Platform {
            os: Platform::current().os,
            arch,
        },
        (None, None) => Platform::current(),
    }
}

fn parse_install(args: &[String]) -> Result<(InstallOptions, Vec<String>), String> {
    let mut root: Option<PathBuf> = None;
    let mut lock: Option<PathBuf> = None;
    let mut repo_root: Option<PathBuf> = None;
    let mut only: Vec<String> = Vec::new();
    let mut force = false;
    let mut assume_yes = false;
    let mut allow_brew = true;
    let mut os: Option<String> = None;
    let mut arch: Option<String> = None;
    let mut overrides = Overrides::default();
    let mut trailing: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--" => {
                trailing = args[i + 1..].to_vec();
                break;
            }
            "--root" => root = Some(PathBuf::from(value_after(args, &mut i, "--root")?)),
            "--lock" => lock = Some(PathBuf::from(value_after(args, &mut i, "--lock")?)),
            "--repo-root" => {
                repo_root = Some(PathBuf::from(value_after(args, &mut i, "--repo-root")?))
            }
            "--component" => only.push(value_after(args, &mut i, "--component")?),
            "--force" => {
                force = true;
                i += 1;
            }
            "--yes" => {
                assume_yes = true;
                i += 1;
            }
            "--no-brew" => {
                allow_brew = false;
                i += 1;
            }
            "--os" => os = Some(value_after(args, &mut i, "--os")?),
            "--arch" => arch = Some(value_after(args, &mut i, "--arch")?),
            "--release-base" => {
                overrides.release_base = Some(value_after(args, &mut i, "--release-base")?)
            }
            "--component-version" => {
                overrides.component_version = Some(value_after(args, &mut i, "--component-version")?)
            }
            "--source" => {
                overrides.source = Some(PathBuf::from(value_after(args, &mut i, "--source")?))
            }
            other => return Err(format!("unknown argument: {}", other)),
        }
    }

    let repo_root = match repo_root {
        Some(dir) => dir,
        None => std::env::current_dir().map_err(|e| format!("current dir: {}", e))?,
    };
    let lock_path = lock.unwrap_or_else(|| repo_root.join("cockatiel.lock"));
    let root = root.unwrap_or_else(default_install_root);

    Ok((
        InstallOptions {
            root,
            lock_path,
            repo_root,
            only,
            force,
            assume_yes,
            allow_brew,
            overrides,
            platform: platform_from(os, arch),
        },
        trailing,
    ))
}

fn cmd_install(args: &[String]) -> Result<i32, String> {
    if wants_help(args) {
        print!("{}", USAGE);
        return Ok(0);
    }
    let (opts, trailing) = parse_install(args)?;
    if !trailing.is_empty() {
        return Err("install does not accept arguments after `--`".to_string());
    }
    let report = install_all(
        &opts,
        &SystemFetcher,
        &ArchiveExtractor,
        &SystemRunner,
    )?;
    print_report(&report);
    Ok(if report.has_failures() { 1 } else { 0 })
}

fn cmd_run(args: &[String]) -> Result<i32, String> {
    if wants_help(args) {
        print!("{}", USAGE);
        return Ok(0);
    }
    let (opts, tui_args) = parse_install(args)?;

    let report = install_all(
        &opts,
        &SystemFetcher,
        &ArchiveExtractor,
        &SystemRunner,
    )?;
    print_report(&report);
    if report.has_failures() {
        eprintln!("error: not launching the TUI because some components failed to install");
        return Ok(1);
    }

    let layout = Layout::new(opts.root.clone());
    let tui = match layout.component_target("tui", "tui") {
        Target::Bin { dir, name } => dir.join(name),
        Target::Dir(_) => return Err("internal error: tui target is not a binary".to_string()),
    };

    let status = Command::new(&tui)
        .args(&tui_args)
        .status()
        .map_err(|e| format!("launch {}: {}", tui.display(), e))?;
    Ok(status.code().unwrap_or(1))
}

fn cmd_doctor(args: &[String]) -> Result<i32, String> {
    if wants_help(args) {
        print!("{}", USAGE);
        return Ok(0);
    }

    let mut root: Option<PathBuf> = None;
    let mut test_runner: Option<PathBuf> = None;
    let mut modules_dir: Option<PathBuf> = None;
    let mut modes: Vec<Mode> = vec![Mode::Paused, Mode::Unpaused];
    let mut quick = false;
    let mut soak = false;
    let mut iterations: u64 = 100;
    let mut duration_secs: u64 = 30;
    let mut json = false;
    let mut keep = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => root = Some(PathBuf::from(value_after(args, &mut i, "--root")?)),
            "--test-runner" => {
                test_runner = Some(PathBuf::from(value_after(args, &mut i, "--test-runner")?))
            }
            "--modules-dir" => {
                modules_dir = Some(PathBuf::from(value_after(args, &mut i, "--modules-dir")?))
            }
            "--quick" => {
                quick = true;
                i += 1;
            }
            "--soak" => {
                soak = true;
                i += 1;
            }
            "--iterations" => {
                iterations = value_after(args, &mut i, "--iterations")?
                    .parse()
                    .map_err(|_| "--iterations requires a number".to_string())?;
            }
            "--duration-secs" => {
                duration_secs = value_after(args, &mut i, "--duration-secs")?
                    .parse()
                    .map_err(|_| "--duration-secs requires a number".to_string())?;
            }
            "--mode" => modes = Mode::parse(&value_after(args, &mut i, "--mode")?)?,
            "--json" => {
                json = true;
                i += 1;
            }
            "--keep" => {
                keep = true;
                i += 1;
            }
            other => return Err(format!("unknown argument: {}", other)),
        }
    }

    let opts = DoctorOptions {
        root: root.unwrap_or_else(default_install_root),
        test_runner,
        modules_dir,
        modes,
        quick,
        soak,
        iterations,
        duration_secs,
        keep,
    };

    let report = doctor::run(&opts)?;
    print_doctor_summary(&report);

    if json {
        println!("__DOCTOR_JSON__");
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
    }

    Ok(if report.ok { 0 } else { 1 })
}

fn print_doctor_summary(report: &DoctorReport) {
    for mode in &report.modes {
        if !mode.booted {
            eprintln!(
                "mode {}: FAILED TO BOOT: {}",
                mode.mode,
                mode.error.as_deref().unwrap_or("unknown error")
            );
            continue;
        }
        println!("mode {}: booted", mode.mode);
        for suite in &mode.suites {
            let status = if suite.failed == 0 { "ok" } else { "FAIL" };
            println!(
                "  {:<12} {:<5} passed={} failed={}",
                suite.suite, status, suite.passed, suite.failed
            );
            if let Some(note) = suite.raw.get("error").and_then(|v| v.as_str()) {
                eprintln!("      {}", note.lines().next().unwrap_or(""));
            }
        }
    }
    println!("doctor: {}", if report.ok { "OK" } else { "FAILED" });
}

fn wants_help(args: &[String]) -> bool {
    args.iter().any(|a| a == "--help" || a == "-h")
}

fn print_report(report: &Report) {
    for key in &report.installed {
        println!("installed  {}", key);
    }
    for key in &report.built {
        println!("built      {}", key);
    }
    for key in &report.skipped {
        println!("skipped    {}", key);
    }
    for (key, err) in &report.failed {
        eprintln!("failed     {}: {}", key, err);
    }
}
