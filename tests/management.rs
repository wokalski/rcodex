use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, process::Command};

fn executable(dir: &std::path::Path, name: &str, script: &str) {
    let path = dir.join(name);
    fs::write(&path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn every_operation_requires_local_codex_before_ssh() {
    let dir = tempfile::tempdir().unwrap();
    executable(
        dir.path(),
        "ssh",
        "#!/bin/sh\nprintf called > \"$TEST_DIR/ssh-called\"\n",
    );
    for flags in [
        vec!["--list"],
        vec!["/project", "--detach"],
        vec!["--resume", "test"],
        vec!["--reconnect"],
        vec!["--inspect", "test"],
        vec!["--shell", "test"],
        vec!["--logs", "abcd"],
        vec!["--stop", "abcd", "--yes"],
        vec!["--rename", "abcd", "--name", "new"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_rcodex"))
            .env("PATH", dir.path())
            .env("TEST_DIR", dir.path())
            .arg("test-host")
            .args(flags)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("install Codex locally first"));
        assert!(!dir.path().join("ssh-called").exists());
    }
}

#[test]
fn management_uses_exact_ids_keeps_json_private_and_requires_stop_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    executable(
        dir.path(),
        "codex",
        "#!/bin/sh\ntest \"$1\" = --version && exit 0\nprintf '%s\\n' \"$@\" > \"$TEST_DIR/codex-args\"\n",
    );
    executable(
        dir.path(),
        "ssh",
        r###"#!/bin/sh
for arg do last=$arg; done
case "$last" in
  *'codex login status') printf 'login\n' >> "$TEST_DIR/events"; exit 0;;
  'uname -sm') printf 'Linux %s\n' "$REMOTE_ARCH";;
  *' __remote '*)
    printf '%s\n' "$last" >> "$TEST_DIR/events"
    case "$last" in
      *'"action":"stop"'*) printf '{"rows":[],"error":null}\n';;
      *) printf '%s\n' "$ROWS";;
    esac;;
  *' __logs '*) printf '%s\n' "$last" >> "$TEST_DIR/events"; printf '%s\n' '{"Ok":"last two\nlines"}';;
  *' __git '*) printf '%s\n' "$last" >> "$TEST_DIR/events"; printf '%s\n' '{"Ok":"## topic\n M file.rs"}';;
  *' __snapshot '*) printf '%s\n' "$last" >> "$TEST_DIR/events"; printf '%s\n' "$SNAPSHOT";;
  *' __thread_read '*) printf '%s\n' "$last" >> "$TEST_DIR/events"; printf '%s\n' "$CONVERSATION";;
  'cd -- '*) printf '%s\n' "$last" >> "$TEST_DIR/events";;
  *) exit 0;;
esac
"###,
    );
    let id = "a".repeat(32);
    let row = json!({"id":id,"name":"nightly","path":"/project with spaces","pid":42,
        "start":"private-start","port":1234,"token":"SECRET_TOKEN",
        "certificate":"SECRET_CERT","log":"/log","created":12});
    let conversation = json!({"id":"saved-id","title":"older activity, newer visit","cwd":"/conversation/project","created_at":1,"updated_at":2,"status":"idle"});
    let mut newer = conversation.clone();
    newer["id"] = json!("newer-id");
    newer["updated_at"] = json!(9000);
    let run = |flags: &[&str]| {
        fs::write(dir.path().join("events"), "").unwrap();
        let mut row = row.clone();
        if flags.contains(&"--reconnect")
            || flags.contains(&"--resume")
            || flags.contains(&"--last")
        {
            row["certificate"] = Value::Null;
        }
        Command::new(env!("CARGO_BIN_EXE_rcodex"))
            .env("PATH", dir.path())
            .env("TEST_DIR", dir.path())
            .env("RCODEX_HISTORY_FILE", dir.path().join("history.json"))
            .env(
                "REMOTE_ARCH",
                if cfg!(target_os = "macos") {
                    "x86_64"
                } else {
                    std::env::consts::ARCH
                },
            )
            .env("ROWS", json!({"rows":[row],"error":null}).to_string())
            .env("CONVERSATION", json!({"Ok":conversation}).to_string())
            .env("SNAPSHOT", json!({"Ok":{"sessions":[conversation,newer],"server_running":true,"complete":true,"warnings":[]}}).to_string())
            .arg("test-host")
            .args(flags)
            .output()
            .unwrap()
    };
    for flags in [
        vec!["--list", "--json"],
        vec!["/project", "--detach", "--name", "nightly", "--json"],
    ] {
        let output = run(&flags);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value[0]["id"], id);
        assert!(value[0].get("token").is_none());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("SECRET"));
        let events = fs::read_to_string(dir.path().join("events")).unwrap();
        assert_eq!(events.contains("login"), flags.contains(&"--detach"));
    }
    let output = run(&["--stop", "nightly"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("pass --yes"));
    assert!(
        !fs::read_to_string(dir.path().join("events"))
            .unwrap()
            .contains("\"action\":\"stop\"")
    );
    assert!(run(&["--stop", "aaaa", "--yes"]).status.success());
    let events = fs::read_to_string(dir.path().join("events")).unwrap();
    assert!(events.contains(&format!("\"id\":\"{id}\"")));
    assert!(!events.contains("login"));
    assert!(
        run(&["--rename", "nightly", "--name", "Morning"])
            .status
            .success()
    );
    assert!(
        fs::read_to_string(dir.path().join("events"))
            .unwrap()
            .contains("\"name\":\"Morning\"")
    );
    let output = run(&["--logs", &id, "--lines", "2"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "last two\nlines\n"
    );
    assert!(
        !fs::read_to_string(dir.path().join("events"))
            .unwrap()
            .contains("\"action\":\"list\"")
    );
    let output = run(&["--inspect", "nightly"]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("## topic\n M file.rs"));
    assert!(!text.contains("SECRET"));
    assert!(
        !fs::read_to_string(dir.path().join("events"))
            .unwrap()
            .contains("login")
    );
    assert!(run(&["--shell", "nightly"]).status.success());
    let events = fs::read_to_string(dir.path().join("events")).unwrap();
    assert!(events.contains("cd -- '/project with spaces' && exec"));
    assert!(!events.contains("login"));
    assert!(!events.contains("\"action\":\"start\""));
    assert!(!dir.path().join("codex-args").exists());
    fs::write(dir.path().join("history.json"), json!({"hosts":["test-host"],"sessions":[{"host":"test-host","conversation":conversation,"visited":50}]}).to_string()).unwrap();
    for (flags, prefix) in [
        (vec!["--reconnect"], "resume\nsaved-id\n"),
        (vec!["--resume", "saved-id"], "resume\nsaved-id\n"),
        (vec!["--last"], "resume\nnewer-id\n"),
    ] {
        let output = run(&flags);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let args = fs::read_to_string(dir.path().join("codex-args")).unwrap();
        assert!(args.starts_with(prefix), "{args}");
        assert!(args.contains("--cd\n/conversation/project\n"));
        let events = fs::read_to_string(dir.path().join("events")).unwrap();
        assert!(events.contains("\"action\":\"ensure\""));
        assert!(!events.contains("\"action\":\"start\""));
        if !flags.contains(&"--last") {
            assert!(events.contains("__thread_read"));
            assert!(!events.contains("__snapshot"));
        }
    }
    let history: Value =
        serde_json::from_slice(&fs::read(dir.path().join("history.json")).unwrap()).unwrap();
    assert_eq!(history["sessions"].as_array().unwrap().len(), 2);
    assert_eq!(history["sessions"][0]["conversation"]["id"], "newer-id");
    assert_eq!(history["sessions"][0]["host"], "test-host");
    assert!(!history.to_string().contains("SECRET"));
}
