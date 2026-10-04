//! Runs real Codex, but never sends a model request or needs an API credential.
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    os::unix::fs::PermissionsExt,
    process::Command,
    time::Duration,
};

fn call(root: &std::path::Path, request: Value) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_rcodex"))
        .env("RCODEX_STATE_DIR", root)
        .args(["__remote", &request.to_string()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(response["error"].is_null(), "{response}");
    response["rows"].clone()
}
fn handshake(port: u64, token: Option<&str>) -> String {
    let mut socket = TcpStream::connect(("127.0.0.1", port as u16)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    write!(socket,"GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n{auth}\r\n").unwrap();
    let mut buffer = [0; 2048];
    let n = socket.read(&mut buffer).unwrap();
    String::from_utf8_lossy(&buffer[..n]).into_owned()
}
#[test]
#[ignore = "requires codex app-server on PATH"]
fn real_codex_persists_lists_authenticates_and_stops() {
    for direct in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("state");
        let path = tmp.path().join("project ' with spaces 日本語");
        fs::create_dir(&path).unwrap();
        let rows = call(&root, json!({"action":"start","path":path,"direct":direct}));
        let c = &rows[0];
        let id = c["id"].as_str().unwrap();
        let port = c["port"].as_u64().unwrap();
        // Always stop a successfully launched server, even if an assertion fails.
        let result = std::panic::catch_unwind(|| {
            assert_eq!(
                fs::read_link(format!("/proc/{}/cwd", c["pid"])).unwrap(),
                path
            );
            assert_eq!(call(&root, json!({"action":"list"}))[0]["id"], id);
            let metadata = fs::metadata(root.join("conns").join(format!("{id}.json"))).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
            if direct {
                assert!(handshake(port, None).starts_with("HTTP/1.1 401"));
                assert!(handshake(port, Some("incorrect-token")).starts_with("HTTP/1.1 401"));
            }
            assert!(handshake(port, c["token"].as_str()).starts_with("HTTP/1.1 101"));
        });
        call(&root, json!({"action":"stop","id":id}));
        assert!(
            call(&root, json!({"action":"list"}))
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(TcpStream::connect(("127.0.0.1", port as u16)).is_err());
        assert!(!root.join("conns").join(format!("{id}.token")).exists());
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }
}
