use std::{fs, os::unix::fs::PermissionsExt, process::Command};

#[test]
fn authentication_precedes_remote_setup_and_failures_stop_launch() {
    for scenario in [
        "authenticated",
        "login",
        "cancelled",
        "unverified",
        "transport",
        "missing",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let executable = |name: &str, script: &str| {
            let path = dir.path().join(name);
            fs::write(&path, script).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        };
        executable("codex", "#!/bin/sh\nexit 0\n");
        executable(
            "ssh",
            r#"#!/bin/sh
for arg do last=$arg; done
case "$last" in
  *'codex login status')
    printf 'check\n' >> "$TEST_DIR/events"
    case "$SCENARIO" in
      authenticated) exit 0;;
      transport) exit 255;;
      missing) exit 127;;
    esac
    test -f "$TEST_DIR/authenticated"
    ;;
  'codex login --device-auth')
    printf 'login\n' >> "$TEST_DIR/events"
    case "$SCENARIO" in
      cancelled) exit 1;;
      unverified) exit 0;;
    esac
    : > "$TEST_DIR/authenticated"
    ;;
  'uname -sm')
    printf 'setup\n' >> "$TEST_DIR/events"
    exit 99
    ;;
  *) exit 0;;
esac
"#,
        );
        let output = Command::new(env!("CARGO_BIN_EXE_rcodex"))
            .env("PATH", dir.path())
            .env("TEST_DIR", dir.path())
            .env("SCENARIO", scenario)
            .args(["test-host", "/project"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        let events = fs::read_to_string(dir.path().join("events")).unwrap();
        let expected = match scenario {
            "authenticated" => "check\nsetup\n",
            "login" => "check\nlogin\ncheck\nsetup\n",
            "cancelled" => "check\nlogin\n",
            "unverified" => "check\nlogin\ncheck\n",
            _ => "check\n",
        };
        assert_eq!(
            events,
            expected,
            "{scenario}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
