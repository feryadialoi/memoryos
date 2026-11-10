//! `memoryos restart`: stops the `memoryos serve` listening on `MEMORYOS_BIND`, starts a new one in
//! the background with its output appended to `~/.local/share/memoryos/server.log`, and waits for
//! `/readyz`. For a local install; a port held by anything other than `memoryos serve`, such as
//! Docker, is refused instead of stopped.

use std::fs::{self, File};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::DEFAULT_BIND;
use crate::settings::Lookup;

/// Above the server's 25 second drain, so a stop is never cut short.
const STOP_TIMEOUT: Duration = Duration::from_secs(35);
/// Room for the first start, which downloads the embedding model.
const READY_TIMEOUT: Duration = Duration::from_secs(300);
const POLL: Duration = Duration::from_millis(250);

pub fn run() -> Result<(), String> {
    let lookup = Lookup::load()?;
    let bind = lookup
        .get("MEMORYOS_BIND")
        .unwrap_or_else(|| DEFAULT_BIND.to_owned());
    let address: SocketAddr = bind
        .trim()
        .parse()
        .map_err(|_| format!("MEMORYOS_BIND {bind:?} is not an address like 127.0.0.1:8080"))?;
    stop(address.port())?;
    let log = log_path()?;
    let mut child = start(&log)?;
    let url = ready_url(address);
    eprintln!("waiting for {url}");
    let started = Instant::now();
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "the server exited ({status}), see {}",
                log.display()
            ));
        }
        if is_ready(&url) {
            println!("memoryos is running on http://{bind}");
            return Ok(());
        }
        if started.elapsed() > READY_TIMEOUT {
            return Err(format!(
                "the server is not ready after {} s, see {}",
                READY_TIMEOUT.as_secs(),
                log.display()
            ));
        }
        std::thread::sleep(POLL);
    }
}

/// Stops every `memoryos serve` listening on `port`, and refuses when something else holds it.
fn stop(port: u16) -> Result<(), String> {
    for pid in listeners(port)? {
        let command = command_of(&pid)?;
        if !is_memoryos_serve(&command) {
            return Err(format!(
                "port {port} is held by `{command}`, not a local memoryos server; stop it first"
            ));
        }
        eprintln!("stopping the server (pid {pid})");
        signal(&pid, "-TERM")?;
        let started = Instant::now();
        while signal(&pid, "-0").is_ok() {
            if started.elapsed() > STOP_TIMEOUT {
                return Err(format!(
                    "the server (pid {pid}) did not stop within {} s",
                    STOP_TIMEOUT.as_secs()
                ));
            }
            std::thread::sleep(POLL);
        }
    }
    Ok(())
}

fn listeners(port: u16) -> Result<Vec<String>, String> {
    let output = Command::new("lsof")
        .args(["-nP", "-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
        .output()
        .map_err(|e| format!("cannot run lsof: {e}"))?;
    // lsof exits 1 when nothing matches.
    let mut pids: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    pids.dedup();
    Ok(pids)
}

fn command_of(pid: &str) -> Result<String, String> {
    let output = Command::new("ps")
        .args(["-o", "command=", "-p", pid])
        .output()
        .map_err(|e| format!("cannot run ps: {e}"))?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// `/path/to/memoryos serve`, with nothing after `serve`.
fn is_memoryos_serve(command: &str) -> bool {
    let mut words = command.split_whitespace();
    let program = words.next().unwrap_or_default();
    let is_memoryos = program.rsplit('/').next() == Some("memoryos");
    is_memoryos && words.next() == Some("serve") && words.next().is_none()
}

fn signal(pid: &str, signal: &str) -> Result<(), String> {
    let status = Command::new("kill")
        .args([signal, pid])
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("cannot run kill: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("kill {signal} {pid} failed"))
    }
}

fn log_path() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    let dir = PathBuf::from(home).join(".local/share/memoryos");
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    Ok(dir.join("server.log"))
}

/// In its own process group, so it outlives this command and the terminal it ran in.
fn start(log: &PathBuf) -> Result<std::process::Child, String> {
    let open = || {
        File::options()
            .create(true)
            .append(true)
            .open(log)
            .map_err(|e| format!("cannot open {}: {e}", log.display()))
    };
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this program: {e}"))?;
    let mut command = Command::new(exe);
    command
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(open()?)
        .stderr(open()?);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command
        .spawn()
        .map_err(|e| format!("cannot start the server: {e}"))
}

/// A server bound to every address is asked on loopback.
pub(crate) fn ready_url(address: SocketAddr) -> String {
    let host = match address {
        SocketAddr::V4(v4) if v4.ip().is_unspecified() => "127.0.0.1".to_owned(),
        SocketAddr::V4(v4) => v4.ip().to_string(),
        SocketAddr::V6(v6) if v6.ip().is_unspecified() => "[::1]".to_owned(),
        SocketAddr::V6(v6) => format!("[{}]", v6.ip()),
    };
    format!("http://{host}:{}/readyz", address.port())
}

fn is_ready(url: &str) -> bool {
    Command::new("curl")
        .args(["-fs", "-o", "/dev/null", "--max-time", "2", url])
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_bare_memoryos_serve_counts_as_the_server() {
        assert!(is_memoryos_serve("/Users/a/.cargo/bin/memoryos serve"));
        assert!(is_memoryos_serve("memoryos serve"));
        assert!(!is_memoryos_serve("memoryos mcp"));
        assert!(!is_memoryos_serve("/x/memoryos-eval serve"));
        assert!(!is_memoryos_serve("memoryos serve --other"));
        assert!(!is_memoryos_serve("com.docker.backend"));
    }

    #[test]
    fn an_unspecified_bind_is_checked_on_loopback() {
        let url = |bind: &str| ready_url(bind.parse().expect("address"));
        assert_eq!(url("0.0.0.0:8080"), "http://127.0.0.1:8080/readyz");
        assert_eq!(url("127.0.0.1:9000"), "http://127.0.0.1:9000/readyz");
        assert_eq!(url("[::]:8080"), "http://[::1]:8080/readyz");
    }
}
