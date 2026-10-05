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
        vec!["--attach", "test"],
        vec!["--reconnect"],
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
        r#"#!/bin/sh
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
  *) exit 0;;
esac
"#,
    );
    let id = "a".repeat(32);
    let row = json!({"id":id,"name":"nightly","path":"/project with spaces","pid":42,
        "start":"private-start","port":1234,"direct":true,"token":"SECRET_TOKEN",
        "certificate":"SECRET_CERT","log":"/log","created":12});
    let run = |flags: &[&str]| {
        fs::write(dir.path().join("events"), "").unwrap();
        let mut row = row.clone();
        if flags.contains(&"--reconnect") {
            row["certificate"] = Value::Null;
            row["last_used"] = json!(42);
        }
        Command::new(env!("CARGO_BIN_EXE_rcodex"))
            .env("PATH", dir.path())
            .env("TEST_DIR", dir.path())
            .env(
                "REMOTE_ARCH",
                if cfg!(target_os = "macos") {
                    "x86_64"
                } else {
                    std::env::consts::ARCH
                },
            )
            .env("ROWS", json!({"rows":[row],"error":null}).to_string())
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
    assert!(!dir.path().join("codex-args").exists());
    for (flags, prefix) in [
        (vec!["--reconnect"], "resume\n--last\n"),
        (vec!["--reconnect", "--resume"], "resume\n--remote\n"),
        (
            vec!["--reconnect", "--resume", "saved-id"],
            "resume\nsaved-id\n",
        ),
    ] {
        let output = run(&flags);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let args = fs::read_to_string(dir.path().join("codex-args")).unwrap();
        assert!(args.starts_with(prefix), "{args}");
        assert!(args.contains("--cd\n/project with spaces\n"));
        let events = fs::read_to_string(dir.path().join("events")).unwrap();
        assert!(events.contains("\"action\":\"visit\""));
        assert!(events.contains(&format!("\"id\":\"{id}\"")));
        assert!(!events.contains("\"action\":\"start\""));
    }
}
