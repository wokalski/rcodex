//! Attach to Codex's own daemon, also used by the desktop app's SSH clients.
//! Never bootstrap, install, update, restart, or stop this shared service.
use crate::remote::{Connection, Request};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    env,
    io::ErrorKind,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    status: String,
    socket_path: String,
    pid: Option<u32>,
}

fn command(operation: &str) -> Result<Status> {
    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            tokio::time::timeout(
                Duration::from_secs(15),
                tokio::process::Command::new("codex")
                    .args(["app-server", "daemon", operation])
                    .stdin(Stdio::null())
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .context("Codex daemon command timed out")?
            .context("run codex app-server daemon")
        })?;
    ensure!(
        output.status.success(),
        "codex app-server daemon {operation} failed (requires Codex 0.160 or newer): {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).context("decode Codex daemon status")
}

pub fn handle(request: &Request, home: &Path) -> Result<Option<Vec<Connection>>> {
    let codex_home = env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"));
    ensure!(codex_home.is_absolute(), "CODEX_HOME must be absolute");
    let socket = codex_home.join("app-server-control/app-server-control.sock");
    let running = match UnixStream::connect(&socket) {
        Ok(_) => true,
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => false,
        Err(e) => return Err(e).context("probe Codex app daemon socket"),
    };
    // An unprovisioned machine retains rcodex's standalone server. Once the
    // app provisions a daemon, it becomes the authority, even while stopped.
    let installed = [
        "packages/app-server-daemon/current/bin/codex",
        "packages/standalone/current/bin/codex",
        "packages/standalone/current/codex",
    ]
    .iter()
    .any(|path| codex_home.join(path).is_file());
    if !running && !installed {
        return Ok(None);
    }
    let mut status = if running {
        let status = command("version")?;
        ensure!(
            status.status == "running",
            "unexpected Codex daemon status: {}",
            status.status
        );
        status
    } else {
        Status {
            status: "notRunning".into(),
            socket_path: socket.to_string_lossy().into(),
            pid: None,
        }
    };
    if let Request::Ensure { direct, .. } = request {
        ensure!(
            !direct,
            "the Codex app daemon uses SSH; reconnect without --direct to share its live conversations"
        );
        if status.status == "notRunning" {
            status = command("start")?;
            ensure!(
                matches!(status.status.as_str(), "started" | "alreadyRunning"),
                "Codex daemon did not start: {}",
                status.status
            );
        }
    }
    if matches!(request, Request::Stop { .. } | Request::Rename { .. }) {
        bail!(
            "this host uses the shared Codex app daemon; rcodex will not stop or rename it. Manage it on the host with codex app-server daemon (stopping interrupts app and terminal conversations)"
        );
    }
    if status.status == "notRunning" {
        return Ok(Some(vec![]));
    }
    ensure!(
        Path::new(&status.socket_path).is_absolute(),
        "Codex daemon returned a non-absolute socket path"
    );
    let id = format!("{:x}", Sha256::digest(status.socket_path.as_bytes()));
    Ok(Some(vec![Connection {
        id: id[..32].into(),
        name: Some("Codex app daemon".into()),
        path: home.to_string_lossy().into(),
        pid: status.pid.unwrap_or(0),
        start: String::new(),
        port: 0,
        socket: Some(status.socket_path),
        token: None,
        certificate: None,
        log: String::new(),
        created: 0,
    }]))
}
