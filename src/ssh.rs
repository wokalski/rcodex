use crate::remote::{Connection, Directory, Reply, Request};
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::Write,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
};
use tempfile::TempDir;

#[derive(Clone)]
pub struct Client {
    pub host: String,
    state: Arc<State>,
    helper: String,
}
struct State {
    host: String,
    dir: TempDir,
}
impl Drop for State {
    fn drop(&mut self) {
        cleanup(
            &self.host,
            &self.dir.path().join("ssh").to_string_lossy(),
            &self.dir.path().to_string_lossy(),
        );
    }
}
pub fn cleanup(host: &str, control: &str, directory: &str) {
    let _ = Command::new("ssh")
        .args(["-S", control, "-O", "exit", host])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = fs::remove_dir_all(directory);
}
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}
fn checked(cmd: &mut Command) -> Result<Vec<u8>> {
    let out = cmd.output().context("run SSH")?;
    ensure!(
        out.status.success(),
        "SSH failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(out.stdout)
}
impl Client {
    pub fn control(&self) -> PathBuf {
        self.directory().join("ssh")
    }
    pub fn directory(&self) -> &Path {
        self.state.dir.path()
    }
    fn ssh(&self) -> Command {
        let mut cmd = Command::new("ssh");
        cmd.arg("-S").arg(self.control());
        cmd
    }
    pub fn connect(host: String) -> Result<Self> {
        let dir = tempfile::Builder::new().prefix("rcodex-").tempdir()?;
        let exe = fs::read(env::current_exe()?)?;
        let hash = format!("{:x}", Sha256::digest(&exe));
        let client = Self {
            host: host.clone(),
            state: Arc::new(State { host, dir }),
            helper: format!("\"$HOME/.cache/rcodex/{}\"", &hash[..24]),
        };
        let status = client
            .ssh()
            .args([
                "-fNT",
                "-M",
                "-o",
                "ControlPersist=no",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                &client.host,
            ])
            .status()?;
        ensure!(status.success(), "could not connect to {}", client.host);
        Ok(client)
    }
    pub fn install(&self) -> Result<()> {
        let platform = checked(self.ssh().args([&self.host, "uname -sm"]))?;
        let platform = String::from_utf8(platform)?;
        let arch = env::consts::ARCH;
        ensure!(
            env::consts::OS == "linux" && platform.trim() == format!("Linux {arch}"),
            "this build needs a Linux/{arch} remote, got {}",
            platform.trim()
        );
        if self
            .ssh()
            .args([&self.host, &format!("test -x {}", self.helper)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?
            .success()
        {
            return Ok(());
        }
        let script = format!(
            r#"set -eu; umask 077; mkdir -p "$HOME/.cache/rcodex"; t=$(mktemp "$HOME/.cache/rcodex/upload.XXXXXX"); trap 'rm -f "$t"' EXIT; cat > "$t"; chmod 700 "$t"; mv "$t" {}"#,
            self.helper
        );
        let mut child = self
            .ssh()
            .args([&self.host, &script])
            .stdin(Stdio::piped())
            .spawn()?;
        let data = fs::read(env::current_exe()?)?;
        child.stdin.take().context("SSH stdin")?.write_all(&data)?;
        ensure!(child.wait()?.success(), "could not install remote helper");
        Ok(())
    }
    pub fn browse(&self, path: &str) -> Result<Directory> {
        let command = format!("{} __browse {}", self.helper, quote(path));
        let out = checked(self.ssh().args([&self.host, &command]))?;
        let reply: Result<Directory, String> =
            serde_json::from_slice(&out).context("decode remote directory")?;
        reply.map_err(anyhow::Error::msg)
    }
    pub fn call(&self, request: Request) -> Result<Vec<Connection>> {
        let command = format!(
            "{} __remote {}",
            self.helper,
            quote(&serde_json::to_string(&request)?)
        );
        let out = checked(self.ssh().args([&self.host, &command]))?;
        let reply: Reply = serde_json::from_slice(&out).context("decode remote response")?;
        if let Some(error) = reply.error {
            bail!("{error}");
        }
        Ok(reply.rows)
    }
    pub fn endpoint(&self, conn: &Connection, direct: bool) -> Result<String> {
        if direct {
            ensure!(
                conn.direct,
                "server is loopback-only; reconnect without --direct"
            );
            let config = checked(Command::new("ssh").args(["-G", &self.host]))?;
            let config = String::from_utf8(config)?;
            let host = config
                .lines()
                .find_map(|l| l.strip_prefix("hostname "))
                .context("SSH hostname missing")?;
            let host = if host.contains(':') && !host.starts_with('[') {
                format!("[{host}]")
            } else {
                host.to_owned()
            };
            return Ok(format!("ws://{host}:{}", conn.port));
        }
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        drop(listener);
        checked(self.ssh().args([
            "-O",
            "forward",
            "-o",
            "ExitOnForwardFailure=yes",
            "-L",
            &format!("127.0.0.1:{port}:127.0.0.1:{}", conn.port),
            &self.host,
        ]))?;
        Ok(format!("ws://127.0.0.1:{port}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shell_quote_preserves_literal_paths() {
        let path = "a'b $(touch nope);\n日本語";
        let out = Command::new("sh")
            .args(["-c", &format!("printf %s {}", quote(path))])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), path);
    }
}
