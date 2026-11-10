//! `memoryos update`: replaces this program with the latest GitHub release built for this machine,
//! checked against the release's SHA-256, then migrates and restarts the server of a local install.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::VERSION;

const REPO: &str = "feryadialoi/memoryos";
const TIMEOUT: Duration = Duration::from_secs(300);

pub fn run() -> Result<(), String> {
    let asset = asset_name()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the runtime: {e}"))?;
    let http = reqwest::Client::builder()
        .user_agent(format!("memoryos/{VERSION}"))
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| format!("cannot start the HTTP client: {e}"))?;

    let latest = runtime.block_on(latest_tag(&http))?;
    let current = format!("v{VERSION}");
    if latest == current {
        println!("memoryos {current} is the latest release");
        return Ok(());
    }
    println!("updating memoryos {current} to {latest}");
    let base = format!("https://github.com/{REPO}/releases/download/{latest}/{asset}");
    let archive = runtime.block_on(download(&http, &base))?;
    let sums = runtime.block_on(download(&http, &format!("{base}.sha256")))?;
    verify(&archive, &String::from_utf8_lossy(&sums))?;

    let exe = std::env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|e| format!("cannot find this program: {e}"))?;
    replace(&exe, &archive)?;
    println!("installed {latest} at {}", exe.display());

    let local = memoryos_hooks::settings::path().is_some_and(|path| path.is_file());
    if !local {
        println!("no server.env, so there is no local server to restart");
        return Ok(());
    }
    run_new(&exe, "migrate")?;
    run_new(&exe, "restart")
}

/// The release asset for this machine, as the release workflow names it.
fn asset_name() -> Result<String, String> {
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    match (os, arch) {
        ("macos", "aarch64") | ("linux", "x86_64") | ("linux", "aarch64") => {
            Ok(format!("memoryos-{os}-{arch}.tar.gz"))
        }
        _ => Err(format!("there is no release built for {os} {arch}")),
    }
}

async fn latest_tag(http: &reqwest::Client) -> Result<String, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let response = http
        .get(&url)
        .header("accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("cannot reach {url}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("{url} answered {}", response.status()));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("cannot read the latest release: {e}"))?;
    body["tag_name"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "the latest release has no tag".to_owned())
}

async fn download(http: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    let response = http
        .get(url)
        .send()
        .await
        .map_err(|e| format!("cannot download {url}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("{url} answered {}", response.status()));
    }
    response
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|e| format!("cannot download {url}: {e}"))
}

/// `sums` is a `shasum -a 256` line: the hex digest, then the file name.
fn verify(archive: &[u8], sums: &str) -> Result<(), String> {
    let expected = sums
        .split_whitespace()
        .next()
        .ok_or("the .sha256 file is empty")?;
    let actual: String = Sha256::digest(archive)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(format!(
            "the download's SHA-256 is {actual}, the release says {expected}; nothing was changed"
        ))
    }
}

/// Unpacks next to `exe` and renames over it, so the swap is one step on the same file system.
fn replace(exe: &Path, archive: &[u8]) -> Result<(), String> {
    let dir = exe
        .parent()
        .ok_or_else(|| format!("{} has no directory", exe.display()))?;
    let work = dir.join(format!(".memoryos-update-{}", std::process::id()));
    let result = unpack_and_swap(exe, &work, archive);
    let _ = fs::remove_dir_all(&work);
    result
}

fn unpack_and_swap(exe: &Path, work: &Path, archive: &[u8]) -> Result<(), String> {
    fs::create_dir_all(work).map_err(|e| format!("cannot write to {}: {e}", work.display()))?;
    let tarball = work.join("memoryos.tar.gz");
    fs::write(&tarball, archive).map_err(|e| format!("cannot write {}: {e}", tarball.display()))?;
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(work)
        .status()
        .map_err(|e| format!("cannot run tar: {e}"))?;
    if !status.success() {
        return Err("tar could not unpack the release".to_owned());
    }
    let new: PathBuf = work.join("memoryos");
    if !new.is_file() {
        return Err("the release archive has no memoryos program".to_owned());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&new, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("cannot make {} executable: {e}", new.display()))?;
    }
    fs::rename(&new, exe).map_err(|e| format!("cannot replace {}: {e}", exe.display()))
}

/// Runs a command of the program just installed, not of this process's old image.
fn run_new(exe: &Path, command: &str) -> Result<(), String> {
    let status = Command::new(exe)
        .arg(command)
        .status()
        .map_err(|e| format!("cannot run memoryos {command}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("memoryos {command} failed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_matching_digest_passes_and_any_other_fails() {
        let archive = b"release";
        let digest: String = Sha256::digest(archive)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert!(verify(
            archive,
            &format!("{digest}  memoryos-macos-aarch64.tar.gz\n")
        )
        .is_ok());
        assert!(verify(archive, &digest.to_uppercase()).is_ok());
        assert!(verify(b"tampered", &digest).is_err());
        assert!(verify(archive, "").is_err());
    }

    #[test]
    fn the_swap_puts_the_archived_program_in_place() {
        let dir = tempfile::tempdir().expect("dir");
        let exe = dir.path().join("memoryos");
        fs::write(&exe, "old").expect("old");
        let source = dir.path().join("source");
        fs::create_dir_all(&source).expect("source");
        fs::write(source.join("memoryos"), "new").expect("new");
        let archive = dir.path().join("a.tar.gz");
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&source)
            .arg("memoryos")
            .status()
            .expect("tar");
        assert!(status.success());

        replace(&exe, &fs::read(&archive).expect("read")).expect("replace");

        assert_eq!(fs::read_to_string(&exe).expect("exe"), "new");
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .expect("list")
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".memoryos-update")
            })
            .collect();
        assert!(leftovers.is_empty());
    }
}
