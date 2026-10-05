use crate::remote::{Connection, Directory, Reply, Request};
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::{IsTerminal, Write},
    net::TcpListener,
    os::fd::AsFd,
    os::unix::process::CommandExt,
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
    batch: bool,
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
fn shell_script(path: &str) -> String {
    format!("cd -- {} && exec \"${{SHELL:-/bin/sh}}\" -i", quote(path))
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
fn helper_binary() -> Result<Vec<u8>> {
    #[cfg(target_os = "macos")]
    {
        Ok(include_bytes!("../bin/rcodex-linux-x86_64").to_vec())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(fs::read(env::current_exe()?)?)
    }
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
        if self.batch {
            cmd.args([
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "ConnectTimeout=5",
            ]);
        }
        cmd
    }
    pub fn connect(host: String) -> Result<Self> {
        Self::connect_mode(host, false)
    }
    pub fn connect_background(host: String) -> Result<Self> {
        Self::connect_mode(host, true)
    }
    fn connect_mode(host: String, batch: bool) -> Result<Self> {
        ensure!(
            !host.is_empty() && !host.starts_with('-') && !host.chars().any(char::is_control),
            "invalid SSH address"
        );
        let dir = tempfile::Builder::new().prefix("rcodex-").tempdir()?;
        let exe = helper_binary()?;
        let hash = format!("{:x}", Sha256::digest(&exe));
        let client = Self {
            host: host.clone(),
            state: Arc::new(State { host, dir }),
            helper: format!("\"$HOME/.cache/rcodex/{}\"", &hash[..24]),
            batch,
        };
        // The watcher survives exec and cancellation of a background probe.
        let pid = std::process::id();
        let start = crate::remote::identity(pid).context("identify local process")?;
        let mut watcher = Command::new(env::current_exe()?);
        watcher
            .args([
                "__watch",
                &pid.to_string(),
                &start,
                &client.host,
                &client.control().to_string_lossy(),
                &client.directory().to_string_lossy(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            watcher.pre_exec(|| {
                nix::unistd::setsid().map_err(std::io::Error::from)?;
                Ok(())
            });
        }
        watcher.spawn().context("start SSH cleanup watcher")?;
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
    pub fn ensure_login(&self) -> Result<()> {
        let check = "command -v codex >/dev/null || exit 127; codex login status";
        let status = self
            .ssh()
            .args([&self.host, check])
            .output()
            .context("check remote Codex login")?;
        if status.status.success() {
            return Ok(());
        }
        ensure!(
            status.status.code() != Some(127),
            "install Codex on the remote host first (codex must be on its noninteractive shell PATH)"
        );
        ensure!(
            status.status.code() == Some(1),
            "remote Codex login check failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        eprintln!(
            "Codex is not logged in on {}. Complete the device login below.",
            self.host
        );
        let mut login = self.ssh();
        if std::io::stdin().is_terminal() {
            login.arg("-t");
        }
        let status = login
            .args([&self.host, "codex login --device-auth"])
            .stdout(std::io::stderr().as_fd().try_clone_to_owned()?)
            .status()
            .context("run remote Codex login")?;
        ensure!(
            status.success(),
            "remote Codex login failed or was cancelled; no server was started"
        );
        let status = self.ssh().args([&self.host, check]).output()?;
        ensure!(
            status.status.success(),
            "remote Codex is still not authenticated; no server was started"
        );
        Ok(())
    }
    pub fn install(&self) -> Result<()> {
        let platform = checked(self.ssh().args([&self.host, "uname -sm"]))?;
        let platform = String::from_utf8(platform)?;
        let arch = if cfg!(target_os = "macos") {
            "x86_64"
        } else {
            env::consts::ARCH
        };
        ensure!(
            platform.trim() == format!("Linux {arch}"),
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
        let data = helper_binary()?;
        child.stdin.take().context("SSH stdin")?.write_all(&data)?;
        ensure!(child.wait()?.success(), "could not install remote helper");
        Ok(())
    }
    pub fn browse(&self, path: &str) -> Result<Directory> {
        self.query("__browse", &[path])
    }
    pub fn snapshot(&self) -> Result<crate::sessions::Snapshot> {
        self.query("__snapshot", &[])
    }
    pub fn start_thread(&self, server: &str, path: &str) -> Result<crate::sessions::Conversation> {
        self.query("__thread_start", &[server, path])
    }
    pub fn read_thread(&self, server: &str, id: &str) -> Result<crate::sessions::Conversation> {
        self.query("__thread_read", &[server, id])
    }
    pub fn mkdir(&self, parent: &str, name: &str) -> Result<Directory> {
        self.query("__mkdir", &[parent, name])
    }
    pub fn logs(&self, id: &str, lines: usize) -> Result<String> {
        self.query("__logs", &[id, &lines.to_string()])
    }
    pub fn inspect(&self, connection: &Connection) -> String {
        let git = self
            .query::<String>("__git", &[&connection.path])
            .unwrap_or_else(|error| format!("{error:#}"));
        connection.details(&git)
    }
    pub fn shell(&self, path: &str) -> Command {
        let mut command = self.ssh();
        command.args(["-t", &self.host, &shell_script(path)]);
        command
    }
    fn query<T: serde::de::DeserializeOwned>(&self, operation: &str, args: &[&str]) -> Result<T> {
        let command = format!(
            "{} {operation} {}",
            self.helper,
            args.iter().map(|s| quote(s)).collect::<Vec<_>>().join(" ")
        );
        let out = checked(self.ssh().args([&self.host, &command]))?;
        let reply: Result<T, String> =
            serde_json::from_slice(&out).context("decode remote response")?;
        reply.map_err(anyhow::Error::msg)
    }
    fn hostname(&self) -> Result<String> {
        let config = checked(Command::new("ssh").args(["-G", &self.host]))?;
        let config = String::from_utf8(config)?;
        Ok(config
            .lines()
            .find_map(|l| l.strip_prefix("hostname "))
            .context("SSH hostname missing")?
            .trim_matches(['[', ']'])
            .to_owned())
    }
    pub fn call(&self, mut request: Request) -> Result<Vec<Connection>> {
        if let Request::Ensure {
            direct: true,
            server_name,
        } = &mut request
        {
            *server_name = Some(self.hostname()?);
        }
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
        if let Some(socket) = &conn.socket {
            ensure!(!direct, "the Codex app daemon requires SSH; omit --direct");
            ensure!(
                Path::new(socket).is_absolute()
                    && !socket.contains(':')
                    && !socket.chars().any(char::is_control),
                "Codex daemon socket path cannot be forwarded by OpenSSH"
            );
            let local = self.directory().join("codex.sock");
            checked(self.ssh().args([
                "-O",
                "forward",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "StreamLocalBindMask=0177",
                "-L",
                &format!("{}:{socket}", local.display()),
                &self.host,
            ]))?;
            return Ok(format!("unix://{}", local.display()));
        }
        if direct {
            let certificate = conn
                .certificate
                .as_deref()
                .context("server is loopback-only; reconnect without --direct")?;
            let host = self.hostname()?;
            crate::tls::check(&host, conn.port, certificate).with_context(|| format!(
                "cannot reach direct TLS server at {host}:{}; check firewall/routing or reconnect without --direct. The remote server is still running",
                conn.port
            ))?;
            let host = if host.contains(':') && !host.starts_with('[') {
                format!("[{host}]")
            } else {
                host.to_owned()
            };
            return Ok(format!("wss://{host}:{}", conn.port));
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
        let scheme = if conn.certificate.is_some() {
            "wss"
        } else {
            "ws"
        };
        Ok(format!("{scheme}://127.0.0.1:{port}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shell_enters_literal_directory_and_does_not_fall_back_on_failure() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project ' $(touch PWNED)");
        fs::create_dir(&project).unwrap();
        let shell = root.path().join("shell");
        fs::write(&shell, "#!/bin/sh\nprintf '%s\\n%s' \"$PWD\" \"$1\"\n").unwrap();
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
        let run = |path: &Path| {
            Command::new("sh")
                .current_dir(root.path())
                .env("SHELL", &shell)
                .args(["-c", &shell_script(path.to_str().unwrap())])
                .output()
                .unwrap()
        };
        let output = run(&project);
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("{}\n-i", project.display())
        );
        assert!(!root.path().join("PWNED").exists());
        let output = run(&root.path().join("missing"));
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }

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
