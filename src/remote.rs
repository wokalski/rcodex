use anyhow::{Context, Result, bail, ensure};
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use serde::{Deserialize, Serialize};
use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    net::{TcpListener, TcpStream},
    os::unix::{
        fs::{DirBuilderExt, OpenOptionsExt},
        process::CommandExt,
    },
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Connection {
    pub id: String,
    pub path: String,
    pub pid: u32,
    pub start: String,
    pub port: u16,
    pub direct: bool,
    pub token: Option<String>,
    pub log: String,
    pub created: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum Request {
    List,
    Start { path: String, direct: bool },
    Stop { id: String },
}
#[derive(Serialize, Deserialize)]
pub struct Reply {
    pub rows: Vec<Connection>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Directory {
    pub path: String,
    pub parent: Option<String>,
    pub folders: Vec<String>,
}

pub fn browse(path: &str) -> Result<Directory> {
    let path = if path == "~" {
        PathBuf::from(env::var("HOME").context("HOME is unset")?)
    } else {
        PathBuf::from(path)
    };
    let path = path.canonicalize().context("open remote directory")?;
    let mut folders = Vec::new();
    for entry in fs::read_dir(&path).context("read remote directory")? {
        let entry = entry?;
        if entry.path().is_dir()
            && let Ok(name) = entry.file_name().into_string()
        {
            folders.push(name);
        }
    }
    folders.sort_by(|a, b| a.starts_with('.').cmp(&b.starts_with('.')).then(a.cmp(b)));
    Ok(Directory {
        parent: path.parent().map(|p| p.to_string_lossy().into_owned()),
        path: path.to_str().context("directory path is not UTF-8")?.into(),
        folders,
    })
}

#[cfg(not(target_os = "macos"))]
pub fn identity(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, fields) = stat.rsplit_once(')')?;
    let fields: Vec<_> = fields.split_whitespace().collect();
    if fields.first() == Some(&"Z") {
        return None;
    }
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    fields.get(19).map(|s| format!("{}:{s}", boot.trim()))
}
#[cfg(target_os = "macos")]
#[link(name = "proc")]
unsafe extern "C" {}

#[cfg(target_os = "macos")]
pub fn identity(pid: u32) -> Option<String> {
    use nix::libc::{PROC_PIDTBSDINFO, SZOMB, proc_bsdinfo, proc_pidinfo};
    let mut info = std::mem::MaybeUninit::<proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<proc_bsdinfo>() as i32;
    // proc_pidinfo writes the entire struct only when it returns its full size.
    let written = unsafe {
        proc_pidinfo(
            pid.try_into().ok()?,
            PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    let info = unsafe { info.assume_init() };
    if info.pbi_status == SZOMB {
        return None;
    }
    Some(format!(
        "{}:{}",
        info.pbi_start_tvsec, info.pbi_start_tvusec
    ))
}
impl Connection {
    pub fn active(&self) -> bool {
        !self.start.is_empty() && identity(self.pid).as_deref() == Some(&self.start)
    }
}
fn private_write(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(bytes)?;
    Ok(())
}
fn terminate(c: &Connection, signal: Signal) -> Result<()> {
    match killpg(Pid::from_raw(c.pid as i32), signal) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
pub fn handle(request: Request) -> Result<Vec<Connection>> {
    let home = PathBuf::from(env::var("HOME").context("HOME is unset")?);
    let root = env::var_os("RCODEX_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state/rcodex"));
    ensure!(root.is_absolute(), "RCODEX_STATE_DIR must be absolute");
    handle_at(request, root, home)
}
fn handle_at(request: Request, root: PathBuf, home: PathBuf) -> Result<Vec<Connection>> {
    let conns = root.join("conns");
    let logs = root.join("logs");
    for dir in [&root, &conns, &logs] {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    match request {
        Request::List => {
            let mut rows = vec![];
            for entry in fs::read_dir(conns)? {
                let file = entry?.path();
                if file.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                let c: Connection = serde_json::from_slice(&fs::read(&file)?)
                    .with_context(|| format!("read {}", file.display()))?;
                if c.active() {
                    rows.push(c);
                }
            }
            rows.sort_by(|a, b| a.path.cmp(&b.path).then(a.created.cmp(&b.created)));
            Ok(rows)
        }
        Request::Stop { id } => {
            ensure!(
                id.len() == 32 && id.bytes().all(|c| c.is_ascii_hexdigit()),
                "invalid connection ID"
            );
            let file = conns.join(format!("{id}.json"));
            let c: Connection = serde_json::from_slice(&fs::read(&file)?)?;
            if c.active() {
                terminate(&c, Signal::SIGTERM)?;
                for _ in 0..50 {
                    if !c.active() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                if c.active() {
                    terminate(&c, Signal::SIGKILL)?;
                }
            }
            fs::remove_file(file)?;
            let _ = fs::remove_file(conns.join(format!("{id}.token")));
            Ok(vec![])
        }
        Request::Start { path, direct } => {
            let path = if path == "~" {
                home
            } else if let Some(p) = path.strip_prefix("~/") {
                home.join(p)
            } else {
                PathBuf::from(path)
            };
            let path = path
                .canonicalize()
                .context("project directory does not exist")?;
            ensure!(path.is_dir(), "project path is not a directory");
            let host = if direct { "0.0.0.0" } else { "127.0.0.1" };
            let listener = TcpListener::bind((host, 0))?;
            let port = listener.local_addr()?.port();
            drop(listener);
            let id = Uuid::new_v4().simple().to_string();
            let log = logs.join(format!("{id}.log"));
            let token_file = conns.join(format!("{id}.token"));
            let token =
                direct.then(|| format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()));
            if let Some(token) = &token {
                private_write(&token_file, token.as_bytes())?;
            }
            let result = (|| -> Result<Vec<Connection>> {
                let output = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(0o600)
                    .open(&log)?;
                let mut cmd = Command::new("nohup");
                cmd.args([
                    "codex",
                    "app-server",
                    "--listen",
                    &format!("ws://{host}:{port}"),
                ])
                .current_dir(&path)
                .stdin(Stdio::null())
                .stderr(output.try_clone()?)
                .stdout(output);
                if direct {
                    cmd.args(["--ws-auth", "capability-token", "--ws-token-file"])
                        .arg(&token_file);
                }
                unsafe {
                    cmd.pre_exec(|| {
                        nix::unistd::setsid().map_err(std::io::Error::from)?;
                        Ok(())
                    });
                }
                let mut child = cmd.spawn().context("launch nohup codex app-server")?;
                let c = Connection {
                    id: id.clone(),
                    path: path.to_string_lossy().into(),
                    pid: child.id(),
                    start: identity(child.id()).unwrap_or_default(),
                    port,
                    direct,
                    token,
                    log: log.to_string_lossy().into(),
                    created: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
                };
                let started = (|| -> Result<()> {
                    let mut ready = false;
                    for _ in 0..150 {
                        if child.try_wait()?.is_some() {
                            break;
                        }
                        if TcpStream::connect_timeout(
                            &format!("127.0.0.1:{port}").parse()?,
                            Duration::from_millis(100),
                        )
                        .is_ok()
                        {
                            thread::sleep(Duration::from_millis(150));
                            ready = child.try_wait()?.is_none() && c.active();
                            break;
                        }
                        thread::sleep(Duration::from_millis(100));
                    }
                    if !ready {
                        bail!(
                            "app-server failed to start; log {}:\n{}",
                            log.display(),
                            fs::read_to_string(&log).unwrap_or_default()
                        );
                    }
                    let tmp = conns.join(format!("{id}.tmp"));
                    private_write(&tmp, &serde_json::to_vec(&c)?)?;
                    fs::rename(tmp, conns.join(format!("{id}.json")))?;
                    Ok(())
                })();
                if let Err(error) = started {
                    let _ = terminate(&c, Signal::SIGKILL);
                    let _ = child.wait();
                    return Err(error);
                }
                Ok(vec![c])
            })();
            if result.is_err() {
                let _ = fs::remove_file(token_file);
            }
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identity_tracks_a_child_until_exit() {
        let mut child = Command::new("sh")
            .args(["-c", "read value"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let before = identity(child.id()).expect("identify running child");
        assert_eq!(identity(child.id()).as_deref(), Some(before.as_str()));
        drop(child.stdin.take());
        child.wait().unwrap();
        assert!(identity(child.id()).is_none());
    }
    #[test]
    fn browse_lists_only_directories_and_follows_directory_links() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["zeta", "a café's project", ".hidden"] {
            fs::create_dir(dir.path().join(name)).unwrap();
        }
        fs::write(dir.path().join("regular-file"), "").unwrap();
        std::os::unix::fs::symlink(dir.path().join("zeta"), dir.path().join("linked")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("missing"), dir.path().join("broken")).unwrap();
        let listing = browse(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(
            listing.folders,
            ["a café's project", "linked", "zeta", ".hidden"]
        );
        assert_eq!(
            browse(&format!("{}/linked", listing.path)).unwrap().path,
            format!("{}/zeta", listing.path)
        );
        assert!(browse(&format!("{}/regular-file", listing.path)).is_err());
        assert!(browse("/").unwrap().parent.is_none());
    }
    #[test]
    fn stale_pid_is_not_listed_or_signaled() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_owned();
        let home = root.clone();
        handle_at(Request::List, root.clone(), home.clone()).unwrap();
        let c = Connection {
            id: "a".repeat(32),
            path: "/test".into(),
            pid: std::process::id(),
            start: "wrong-start-time".into(),
            port: 1234,
            direct: false,
            token: None,
            log: "log".into(),
            created: 0,
        };
        fs::write(
            root.join("conns").join(format!("{}.json", c.id)),
            serde_json::to_vec(&c).unwrap(),
        )
        .unwrap();
        assert!(
            handle_at(Request::List, root.clone(), home.clone())
                .unwrap()
                .is_empty()
        );
        handle_at(Request::Stop { id: c.id }, root, home).unwrap();
        assert!(identity(std::process::id()).is_some());
    }
    #[test]
    fn rejects_path_traversal_and_missing_project() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            handle_at(
                Request::Stop {
                    id: "../outside".into()
                },
                dir.path().into(),
                dir.path().into()
            )
            .is_err()
        );
        assert!(
            handle_at(
                Request::Start {
                    path: dir.path().join("missing").to_string_lossy().into(),
                    direct: false
                },
                dir.path().into(),
                dir.path().into()
            )
            .is_err()
        );
    }
}
