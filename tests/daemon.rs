use serde_json::{Value, json};
use std::{
    fs,
    os::unix::{
        fs::{PermissionsExt, symlink},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

fn helper(home: &Path, args: &[&str], path: Option<&Path>) -> Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rcodex"));
    command
        .env("CODEX_HOME", home)
        .env("RCODEX_STATE_DIR", home.join("rcodex-state"))
        .args(args);
    if let Some(path) = path {
        command.env("PATH", path);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn request(home: &Path, value: Value, path: Option<&Path>) -> Value {
    helper(home, &["__remote", &value.to_string()], path)
}

#[test]
fn daemon_discovery_is_passive_and_failures_never_start_a_separate_server() {
    // macOS/Nix TMPDIR can exceed sockaddr_un's pathname limit.
    let temp = tempfile::Builder::new()
        .prefix("rcd-")
        .tempdir_in("/tmp")
        .unwrap();
    let home = temp.path();
    let bin = home.join("bin");
    fs::create_dir(&bin).unwrap();
    let executable = bin.join("codex");
    fs::write(&executable, r#"#!/bin/sh
printf '%s\n' "$*" >> "$CODEX_HOME/commands"
case "$3" in
  version) printf '{"status":"running","socketPath":"%s/app-server-control/app-server-control.sock"}\n' "$CODEX_HOME";;
  start) printf 'cannot start daemon\n' >&2; exit 1;;
  *) exit 99;;
esac
"#).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    // A configured but stopped daemon must not trigger standalone fallback.
    let package = home.join("packages/app-server-daemon/current/bin");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("codex"), "fixture").unwrap();
    let rows = request(home, json!({"action":"list"}), Some(&bin));
    assert_eq!(rows["rows"], json!([]));
    assert!(!home.join("commands").exists());
    let failed = request(home, json!({"action":"ensure","direct":false}), Some(&bin));
    assert!(
        failed["error"]
            .as_str()
            .unwrap()
            .contains("cannot start daemon")
    );
    assert!(!home.join("rcodex-state").exists());
    assert_eq!(
        fs::read_to_string(home.join("commands")).unwrap(),
        "app-server daemon start\n"
    );
    fs::remove_file(home.join("commands")).unwrap();
    let direct = request(home, json!({"action":"ensure","direct":true}), Some(&bin));
    assert!(
        direct["error"]
            .as_str()
            .unwrap()
            .contains("without --direct")
    );
    assert!(!home.join("commands").exists());

    fs::create_dir(home.join("app-server-control")).unwrap();
    let listener =
        UnixListener::bind(home.join("app-server-control/app-server-control.sock")).unwrap();
    let accept = std::thread::spawn(move || {
        for _ in 0..5 {
            drop(listener.accept().unwrap());
        }
    });
    let server = request(home, json!({"action":"ensure","direct":false}), Some(&bin));
    assert!(server["error"].is_null(), "{server}");
    assert!(
        server["rows"][0]["socket"]
            .as_str()
            .unwrap()
            .ends_with(".sock")
    );
    for action in [
        json!({"action":"stop","id":server["rows"][0]["id"]}),
        json!({"action":"rename","id":server["rows"][0]["id"],"name":"other"}),
    ] {
        let refused = request(home, action, Some(&bin));
        assert!(
            refused["error"]
                .as_str()
                .unwrap()
                .contains("will not stop or rename")
        );
    }
    fs::write(
        &executable,
        "#!/bin/sh\nprintf 'broken daemon' >&2; exit 1\n",
    )
    .unwrap();
    let failed = request(home, json!({"action":"ensure","direct":false}), Some(&bin));
    assert!(failed["error"].as_str().unwrap().contains("broken daemon"));
    fs::write(&executable, "#!/bin/sh\nprintf 'not json'\n").unwrap();
    let failed = request(home, json!({"action":"ensure","direct":false}), Some(&bin));
    assert!(
        failed["error"]
            .as_str()
            .unwrap()
            .contains("decode Codex daemon status")
    );
    accept.join().unwrap();
    assert!(!home.join("rcodex-state").exists());
    assert_eq!(
        fs::read_to_string(home.join("commands")).unwrap(),
        "app-server daemon version\n".repeat(3)
    );
}

struct Daemon(PathBuf);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = Command::new("codex")
            .env("CODEX_HOME", &self.0)
            .args(["app-server", "daemon", "stop"])
            .output();
    }
}
struct Proxy(std::process::Child);
impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn rpc(ws: &mut tungstenite::WebSocket<UnixStream>, id: u64, method: &str, params: Value) -> Value {
    ws.send(tungstenite::Message::Text(
        json!({"id":id,"method":method,"params":params})
            .to_string()
            .into(),
    ))
    .unwrap();
    loop {
        if let tungstenite::Message::Text(text) = ws.read().unwrap() {
            let response: Value = serde_json::from_str(&text).unwrap();
            if response["id"] == id {
                assert!(response["error"].is_null(), "{response}");
                return response["result"].clone();
            }
        }
    }
}

#[test]
#[ignore = "requires an existing Codex daemon package; uses a disposable CODEX_HOME, no model calls"]
fn official_proxy_and_rcodex_share_one_live_daemon_and_conversations() {
    // Only read the real installation location; never manage the user's daemon.
    let output = Command::new("codex")
        .args(["app-server", "daemon", "version"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    let package = Path::new(status["managedCodexPath"].as_str().unwrap())
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let install = home.join("packages/app-server-daemon");
    fs::create_dir_all(&install).unwrap();
    symlink(package, install.join("current")).unwrap();
    fs::create_dir(home.join("app-server-daemon")).unwrap();
    fs::write(
        home.join("app-server-daemon/settings.json"),
        r#"{"remoteControlEnabled":false,"updater":{"autoUpdateEnabled":false}}"#,
    )
    .unwrap();
    let _cleanup = Daemon(home.into());
    let ensure = json!({"action":"ensure","direct":false});
    let server = request(home, ensure.clone(), None);
    assert!(server["error"].is_null(), "{server}");
    let id = server["rows"][0]["id"].as_str().unwrap();
    assert!(server["rows"][0]["socket"].is_string());
    assert!(!home.join("rcodex-state/server.json").exists());
    let again = request(home, ensure, None);
    assert_eq!(again["rows"][0]["id"], id);

    // Official SSH clients speak WebSocket through `app-server proxy` stdio.
    let (stream, child_stream) = UnixStream::pair().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let _proxy = Proxy(
        Command::new("codex")
            .env("CODEX_HOME", home)
            .args(["app-server", "proxy"])
            .stdin(Stdio::from(std::os::fd::OwnedFd::from(
                child_stream.try_clone().unwrap(),
            )))
            .stdout(Stdio::from(std::os::fd::OwnedFd::from(child_stream)))
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let (mut ws, _) = tungstenite::client("ws://localhost/rpc", stream).unwrap();
    rpc(
        &mut ws,
        1,
        "initialize",
        json!({"clientInfo":{"name":"app-ssh-test","version":"1"}}),
    );
    ws.send(tungstenite::Message::Text(
        json!({"method":"initialized"}).to_string().into(),
    ))
    .unwrap();
    let project = home.join("terminal project");
    fs::create_dir(&project).unwrap();
    let created = helper(
        home,
        &["__thread_start", id, project.to_str().unwrap()],
        None,
    );
    let thread = created["Ok"]["id"].as_str().unwrap();
    let resumed = rpc(&mut ws, 2, "thread/resume", json!({"threadId":thread}));
    assert_eq!(resumed["thread"]["id"], thread);
    assert_eq!(resumed["thread"]["cwd"], project.to_str().unwrap());
    // An empty, still-loaded app thread distinguishes shared runtime from merely
    // reading another server's persisted session files.
    let other = home.join("app project");
    fs::create_dir(&other).unwrap();
    let app_thread = rpc(&mut ws, 3, "thread/start", json!({"cwd":other}));
    let app_id = app_thread["thread"]["id"].as_str().unwrap();
    let snapshot = helper(home, &["__snapshot"], None);
    assert_eq!(snapshot["Ok"]["complete"], true, "{snapshot}");
    let rows = snapshot["Ok"]["sessions"].as_array().unwrap();
    assert!(
        rows.iter()
            .any(|r| r["id"] == thread && r["cwd"] == project.to_str().unwrap())
    );
    assert!(
        rows.iter()
            .any(|r| r["id"] == app_id && r["cwd"] == other.to_str().unwrap())
    );
    let read = helper(home, &["__thread_read", id, app_id], None);
    assert_eq!(read["Ok"]["id"], app_id);
    let loaded = rpc(&mut ws, 4, "thread/loaded/list", json!({}));
    assert!(
        loaded["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r == app_id)
    );
}
