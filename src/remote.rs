use anyhow::{Context, Result, bail, ensure};
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use serde::{Deserialize, Serialize};
use std::{
    env,
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
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
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub favorite: bool,
    #[serde(default)]
    pub last_used: u64,
    pub path: String,
    pub pid: u32,
    pub start: String,
    pub port: u16,
    pub direct: bool,
    pub token: Option<String>,
    #[serde(default)]
    pub certificate: Option<String>,
    pub log: String,
    pub created: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum Request {
    List,
    Start {
        path: String,
        direct: bool,
        #[serde(default)]
        server_name: Option<String>,
        #[serde(default)]
        name: Option<String>,
    },
    Stop {
        id: String,
    },
    Rename {
        id: String,
        name: String,
    },
    Favorite {
        id: String,
        favorite: bool,
    },
    Visit {
        id: String,
    },
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

pub fn mkdir(parent: &str, name: &str) -> Result<Directory> {
    ensure!(
        !name.is_empty()
            && name != "."
            && name != ".."
            && !name.contains('/')
            && !name.chars().any(char::is_control),
        "enter a folder name, not a path"
    );
    let parent = PathBuf::from(parent)
        .canonicalize()
        .context("open parent directory")?;
    let path = parent.join(name);
    fs::create_dir(&path).context("create folder (existing folders are not overwritten)")?;
    browse(path.to_str().context("directory path is not UTF-8")?)
}

pub fn clean_text(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

fn validate_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 32 && id.bytes().all(|c| c.is_ascii_hexdigit()),
        "invalid connection ID"
    );
    Ok(())
}

fn normalized_name(name: &str) -> Result<Option<String>> {
    ensure!(
        !name.chars().any(char::is_control),
        "names cannot contain control characters"
    );
    let name = name.trim();
    ensure!(
        name.chars().count() <= 80,
        "names must be at most 80 characters"
    );
    Ok((!name.is_empty()).then(|| name.to_owned()))
}

fn state_dir(home: &std::path::Path) -> Result<PathBuf> {
    let root = env::var_os("RCODEX_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state/rcodex"));
    ensure!(root.is_absolute(), "RCODEX_STATE_DIR must be absolute");
    Ok(root)
}

pub fn logs(id: &str, lines: usize) -> Result<String> {
    let home = PathBuf::from(env::var("HOME").context("HOME is unset")?);
    logs_at(&state_dir(&home)?, id, lines)
}

fn logs_at(root: &std::path::Path, id: &str, lines: usize) -> Result<String> {
    validate_id(id)?;
    ensure!(
        (1..=2000).contains(&lines),
        "log lines must be between 1 and 2000"
    );
    let mut file =
        fs::File::open(root.join("logs").join(format!("{id}.log"))).context("open server log")?;
    // Never load an unbounded log into either the helper or the terminal.
    let offset = file.metadata()?.len().saturating_sub(128 * 1024);
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(128 * 1024).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let text = if offset > 0 {
        text.split_once('\n')
            .map_or(text.as_ref(), |(_, tail)| tail)
    } else {
        &text
    };
    let tail: Vec<_> = text.lines().rev().take(lines).collect();
    Ok(clean_text(
        &tail.into_iter().rev().collect::<Vec<_>>().join("\n"),
    ))
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
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or_else(|| {
            std::path::Path::new(&self.path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&self.path)
        })
    }

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
    let root = state_dir(&home)?;
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
    // A stable lock serializes atomic record replacement with stop/removal.
    let registry_lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(root.join("registry.lock"))?;
    registry_lock.lock()?;
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
            rows.sort_by(|a, b| {
                b.favorite
                    .cmp(&a.favorite)
                    .then(a.path.cmp(&b.path))
                    .then(a.created.cmp(&b.created))
                    .then(a.id.cmp(&b.id))
            });
            Ok(rows)
        }
        Request::Rename { ref id, .. }
        | Request::Favorite { ref id, .. }
        | Request::Visit { ref id } => {
            validate_id(id)?;
            let file = conns.join(format!("{id}.json"));
            let mut c: Connection = serde_json::from_slice(&fs::read(&file)?)?;
            ensure!(c.active(), "server is no longer running");
            match request {
                Request::Rename { name, .. } => c.name = normalized_name(&name)?,
                Request::Favorite { favorite, .. } => c.favorite = favorite,
                Request::Visit { .. } => {
                    c.last_used = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64
                }
                _ => unreachable!(),
            }
            let mut temp = tempfile::NamedTempFile::new_in(&conns)?;
            temp.write_all(&serde_json::to_vec(&c)?)?;
            temp.persist(&file)?;
            Ok(vec![c])
        }
        Request::Stop { id } => {
            validate_id(&id)?;
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
            let _ = fs::remove_file(conns.join(format!("{id}.pem")));
            let _ = fs::remove_file(conns.join(format!("{id}.key")));
            Ok(vec![])
        }
        Request::Start {
            path,
            direct,
            server_name,
            name,
        } => {
            let name = name.as_deref().map(normalized_name).transpose()?.flatten();
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
            let cert_file = conns.join(format!("{id}.pem"));
            let key_file = conns.join(format!("{id}.key"));
            let token =
                direct.then(|| format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()));
            if let Some(token) = &token {
                private_write(&token_file, token.as_bytes())?;
            }
            let result = (|| -> Result<Vec<Connection>> {
                let certificate = if direct {
                    let name =
                        server_name.context("direct mode needs an SSH-resolved server name")?;
                    let certified = rcgen::generate_simple_self_signed(vec![
                        name,
                        "localhost".into(),
                        "127.0.0.1".into(),
                    ])?;
                    let certificate = certified.cert.pem();
                    private_write(&cert_file, certificate.as_bytes())?;
                    private_write(&key_file, certified.signing_key.serialize_pem().as_bytes())?;
                    Some(certificate)
                } else {
                    None
                };
                let output = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(0o600)
                    .open(&log)?;
                let mut cmd = Command::new("nohup");
                if direct {
                    cmd.arg(env::current_exe()?)
                        .arg("__tls")
                        .arg(port.to_string())
                        .arg(&token_file)
                        .arg(&cert_file)
                        .arg(&key_file);
                } else {
                    cmd.args([
                        "codex",
                        "app-server",
                        "--listen",
                        &format!("ws://{host}:{port}"),
                    ]);
                }
                cmd.current_dir(&path)
                    .stdin(Stdio::null())
                    .stderr(output.try_clone()?)
                    .stdout(output);
                unsafe {
                    cmd.pre_exec(|| {
                        nix::unistd::setsid().map_err(std::io::Error::from)?;
                        Ok(())
                    });
                }
                let mut child = cmd.spawn().context("launch nohup codex app-server")?;
                let c = Connection {
                    id: id.clone(),
                    name,
                    favorite: false,
                    last_used: 0,
                    path: path.to_string_lossy().into(),
                    pid: child.id(),
                    start: identity(child.id()).unwrap_or_default(),
                    port,
                    direct,
                    token,
                    certificate,
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
                let _ = fs::remove_file(cert_file);
                let _ = fs::remove_file(key_file);
            }
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn folders_are_created_without_overwriting_or_escaping_parent() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().to_str().unwrap();
        let directory = mkdir(parent, "café's project").unwrap();
        assert_eq!(
            directory.path,
            root.path().join("café's project").to_str().unwrap()
        );
        fs::write(root.path().join("café's project/keep"), "unchanged").unwrap();
        for bad in [
            "",
            ".",
            "..",
            "../escape",
            "/absolute",
            "line\nbreak",
            "café's project",
        ] {
            assert!(mkdir(parent, bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            fs::read_to_string(root.path().join("café's project/keep")).unwrap(),
            "unchanged"
        );
    }

    #[test]
    fn log_tail_is_bounded_ordered_and_removes_terminal_controls() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("logs")).unwrap();
        let id = "b".repeat(32);
        let file = root.path().join("logs").join(format!("{id}.log"));
        fs::write(&file, "one\ntwo\nthree\n").unwrap();
        assert_eq!(logs_at(root.path(), &id, 2).unwrap(), "two\nthree");
        assert_eq!(logs_at(root.path(), &id, 1).unwrap(), "three");
        fs::write(
            &file,
            format!("{}\nlast\n\x1b]52;secret\x07\n", "x".repeat(200_000)),
        )
        .unwrap();
        let tail = logs_at(root.path(), &id, 2000).unwrap();
        assert_eq!(tail, "last\n]52;secret");
        fs::write(&file, "x".repeat(200_000)).unwrap();
        assert_eq!(logs_at(root.path(), &id, 1).unwrap().len(), 128 * 1024);
        assert!(logs_at(root.path(), "../escape", 1).is_err());
        assert!(logs_at(root.path(), &id, 0).is_err());
        assert!(logs_at(root.path(), &id, 2001).is_err());
    }

    #[test]
    fn rename_preserves_credentials_and_old_records_remain_readable() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_owned();
        handle_at(Request::List, root.clone(), root.clone()).unwrap();
        let id = "c".repeat(32);
        let file = root.join("conns").join(format!("{id}.json"));
        let original = serde_json::json!({"id":id,"path":"/project", "pid":std::process::id(),
            "start":identity(std::process::id()).unwrap(), "port":1234,"direct":true,
            "token":"keep-token", "certificate":"keep-cert", "log":"log", "created":17});
        fs::write(&file, original.to_string()).unwrap();
        let old: Connection = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        assert!(!old.favorite);
        assert_eq!(old.last_used, 0);
        handle_at(
            Request::Favorite {
                id: id.clone(),
                favorite: true,
            },
            root.clone(),
            root.clone(),
        )
        .unwrap();
        let visited = handle_at(
            Request::Visit { id: id.clone() },
            root.clone(),
            root.clone(),
        )
        .unwrap();
        assert!(visited[0].favorite);
        assert!(visited[0].last_used > 0);
        let mut other = old.clone();
        other.id = "d".repeat(32);
        other.path = "/aaa".into();
        let other_file = root.join("conns").join(format!("{}.json", other.id));
        fs::write(&other_file, serde_json::to_vec(&other).unwrap()).unwrap();
        assert_eq!(
            handle_at(Request::List, root.clone(), root.clone()).unwrap()[0].id,
            id
        );
        handle_at(
            Request::Favorite {
                id: id.clone(),
                favorite: false,
            },
            root.clone(),
            root.clone(),
        )
        .unwrap();
        assert_eq!(
            handle_at(Request::List, root.clone(), root.clone()).unwrap()[0].id,
            other.id
        );
        handle_at(
            Request::Favorite {
                id: id.clone(),
                favorite: true,
            },
            root.clone(),
            root.clone(),
        )
        .unwrap();
        fs::remove_file(other_file).unwrap();
        for (name, expected) in [("  café  ", Some("café")), ("", None)] {
            let rows = handle_at(
                Request::Rename {
                    id: id.clone(),
                    name: name.into(),
                },
                root.clone(),
                root.clone(),
            )
            .unwrap();
            assert_eq!(rows[0].name.as_deref(), expected);
            let saved: serde_json::Value =
                serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
            assert_eq!(saved["token"], "keep-token");
            assert_eq!(saved["certificate"], "keep-cert");
            assert_eq!(saved["created"], 17);
            assert_eq!(saved["favorite"], true);
            assert_eq!(saved["last_used"], visited[0].last_used);
            assert_eq!(
                fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        for bad in ["x".repeat(81), "bad\nname".into(), "\nname".into()] {
            assert!(
                handle_at(
                    Request::Rename {
                        id: id.clone(),
                        name: bad
                    },
                    root.clone(),
                    root.clone()
                )
                .is_err()
            );
        }
        fs::remove_file(&file).unwrap();
        assert!(
            handle_at(
                Request::Rename {
                    id,
                    name: "gone".into()
                },
                root.clone(),
                root
            )
            .is_err()
        );
        assert!(!file.exists());
    }
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
            name: None,
            favorite: false,
            last_used: 0,
            path: "/test".into(),
            pid: std::process::id(),
            start: "wrong-start-time".into(),
            port: 1234,
            direct: false,
            token: None,
            certificate: None,
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
                    direct: false,
                    server_name: None,
                    name: None,
                },
                dir.path().into(),
                dir.path().into()
            )
            .is_err()
        );
    }
}
