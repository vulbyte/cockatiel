//! Downloading release assets by shelling out to a system downloader, plus
//! SHA-256 verification.
//!
//! The bootstrapper deliberately avoids an HTTP/TLS crate: it uses whichever
//! of `curl`, `wget` or `python3` is already on the machine. The command
//! execution is routed through a [`CommandRunner`](crate::build::CommandRunner)
//! so the selection logic is testable.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::build::{CommandRunner, SystemRunner};

/// Fetch `url` into `dest`.
pub trait Fetcher {
    fn fetch(&self, url: &str, dest: &Path) -> Result<(), String>;
}

/// The real fetcher: picks the first available system downloader.
pub struct SystemFetcher;

impl Fetcher for SystemFetcher {
    fn fetch(&self, url: &str, dest: &Path) -> Result<(), String> {
        fetch_with(&SystemRunner, url, dest)
    }
}

/// Downloader selection, parameterised over the runner so it can be faked.
///
/// Tries, in order:
/// 1. `curl -fsSL --retry 3 -o <dest> <url>` (`-sS` keeps the progress meter off
///    stderr but still prints errors)
/// 2. `wget -q -O <dest> <url>`
/// 3. `python3 -c "import urllib.request,sys; urllib.request.urlretrieve(sys.argv[1], sys.argv[2])" <url> <dest>`
pub fn fetch_with<R: CommandRunner + ?Sized>(
    runner: &R,
    url: &str,
    dest: &Path,
) -> Result<(), String> {
    let dest_str = dest.to_string_lossy().to_string();

    if runner.which("curl").is_some() {
        let args = vec![
            "-fsSL".to_string(),
            "--retry".to_string(),
            "3".to_string(),
            "-o".to_string(),
            dest_str.clone(),
            url.to_string(),
        ];
        return runner
            .run("curl", &args, Path::new("."))
            .map_err(|e| format!("curl download failed: {}", e));
    }

    if runner.which("wget").is_some() {
        let args = vec![
            "-q".to_string(),
            "-O".to_string(),
            dest_str.clone(),
            url.to_string(),
        ];
        return runner
            .run("wget", &args, Path::new("."))
            .map_err(|e| format!("wget download failed: {}", e));
    }

    if runner.which("python3").is_some() {
        let script = "import urllib.request,sys; urllib.request.urlretrieve(sys.argv[1], sys.argv[2])";
        let args = vec![
            "-c".to_string(),
            script.to_string(),
            url.to_string(),
            dest_str.clone(),
        ];
        return runner
            .run("python3", &args, Path::new("."))
            .map_err(|e| format!("python3 download failed: {}", e));
    }

    Err(format!(
        "no downloader found: install one of `curl`, `wget` or `python3` \
         (tried `curl -fsSL --retry 3 -o <dest> <url>`, `wget -q -O <dest> <url>`, \
         and `python3 -c \"...urlretrieve...\" <url> <dest>`); url was {}",
        url
    ))
}

/// SHA-256 of a file as lowercase hex.
pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file =
        File::open(path).map_err(|e| format!("open {} for hashing: {}", path.display(), e))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("read {}: {}", path.display(), e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{:02x}", byte));
    }
    Ok(hex)
}

/// Verify a file's SHA-256 against `expected` (case-insensitive).
pub fn verify_sha256(path: &Path, expected: &str) -> Result<(), String> {
    let actual = sha256_file(path)?;
    if actual.eq_ignore_ascii_case(expected.trim()) {
        Ok(())
    } else {
        Err(format!(
            "sha256 mismatch for {}: expected {}, got {}",
            path.display(),
            expected.trim(),
            actual
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    const ABC_SHA: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn temp_file(tag: &str, bytes: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "cockatiel_fetch_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn sha256_matches_known_vector() {
        let path = temp_file("abc", b"abc");
        assert_eq!(sha256_file(&path).unwrap(), ABC_SHA);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn verify_accepts_and_rejects() {
        let path = temp_file("verify", b"abc");
        assert!(verify_sha256(&path, ABC_SHA).is_ok());
        assert!(verify_sha256(&path, &ABC_SHA.to_uppercase()).is_ok());
        let err = verify_sha256(&path, "deadbeef").unwrap_err();
        assert!(err.contains("sha256 mismatch"), "{}", err);
        let _ = std::fs::remove_file(&path);
    }

    #[derive(Default)]
    struct FakeRunner {
        present: Vec<&'static str>,
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl CommandRunner for FakeRunner {
        fn which(&self, program: &str) -> Option<PathBuf> {
            self.present
                .contains(&program)
                .then(|| PathBuf::from(program))
        }

        fn run(&self, program: &str, args: &[String], _cwd: &Path) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push((program.to_string(), args.to_vec()));
            Ok(())
        }
    }

    #[test]
    fn prefers_curl_over_the_others() {
        let runner = FakeRunner {
            present: vec!["curl", "wget", "python3"],
            ..Default::default()
        };
        fetch_with(&runner, "https://example.com/a", Path::new("/tmp/a")).unwrap();
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "curl");
        assert!(calls[0].1.contains(&"https://example.com/a".to_string()));
    }

    #[test]
    fn falls_back_to_wget_then_python3() {
        let wget = FakeRunner {
            present: vec!["wget"],
            ..Default::default()
        };
        fetch_with(&wget, "u", Path::new("/tmp/d")).unwrap();
        assert_eq!(wget.calls.lock().unwrap()[0].0, "wget");

        let python = FakeRunner {
            present: vec!["python3"],
            ..Default::default()
        };
        fetch_with(&python, "u", Path::new("/tmp/d")).unwrap();
        assert_eq!(python.calls.lock().unwrap()[0].0, "python3");
    }

    #[test]
    fn errors_when_no_downloader_exists() {
        let runner = FakeRunner::default();
        let err = fetch_with(&runner, "https://x/y", Path::new("/tmp/y")).unwrap_err();
        assert!(err.contains("no downloader found"), "{}", err);
        assert!(err.contains("curl"), "{}", err);
        assert!(err.contains("wget"), "{}", err);
        assert!(err.contains("python3"), "{}", err);
    }
}
