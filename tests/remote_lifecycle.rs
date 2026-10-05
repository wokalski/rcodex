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
    let codex_home = root.parent().unwrap().join("codex");
    fs::create_dir_all(&codex_home).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rcodex"))
        .env("RCODEX_STATE_DIR", root)
        .env("CODEX_HOME", codex_home)
        .args(["__remote", &request.to_string()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(response["error"].is_null(), "{response}");
    response["rows"].clone()
}

fn helper(root: &std::path::Path, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_rcodex"))
        .env("RCODEX_STATE_DIR", root)
        .env("CODEX_HOME", root.parent().unwrap().join("codex"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(response.get("Ok").is_some(), "{response}");
    response["Ok"].clone()
}

trait Socket: Read + Write {}
impl<T: Read + Write> Socket for T {}

fn resume(server: &Value, id: &str) -> tungstenite::WebSocket<Box<dyn Socket>> {
    use rustls::pki_types::{CertificateDer, ServerName, pem::PemObject};
    use tungstenite::client::IntoClientRequest;
    let port = server["port"].as_u64().unwrap() as u16;
    let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut request = format!("ws://127.0.0.1:{port}")
        .into_client_request()
        .unwrap();
    if let Some(token) = server["token"].as_str() {
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    }
    let stream: Box<dyn Socket> = if let Some(cert) = server["certificate"].as_str() {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_slice(cert.as_bytes()).unwrap())
            .unwrap();
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Box::new(rustls::StreamOwned::new(
            rustls::ClientConnection::new(
                std::sync::Arc::new(config),
                ServerName::try_from("localhost").unwrap(),
            )
            .unwrap(),
            tcp,
        ))
    } else {
        Box::new(tcp)
    };
    let (mut ws, _) = tungstenite::client(request, stream).unwrap();
    for (number, method, params) in [
        (
            1,
            "initialize",
            json!({"clientInfo":{"name":"rcodex-test","version":"1"}}),
        ),
        (2, "thread/resume", json!({"threadId":id})),
    ] {
        ws.send(tungstenite::Message::Text(
            json!({"id":number,"method":method,"params":params})
                .to_string()
                .into(),
        ))
        .unwrap();
        loop {
            let message = ws.read().unwrap();
            if let tungstenite::Message::Text(text) = message {
                let response: Value = serde_json::from_str(&text).unwrap();
                if response["id"] == number {
                    assert!(response["error"].is_null(), "{response}");
                    if number == 2 {
                        assert_eq!(response["result"]["thread"]["id"], id);
                    }
                    break;
                }
            }
        }
    }
    ws
}

struct Servers(std::path::PathBuf);
impl Drop for Servers {
    fn drop(&mut self) {
        for c in call(&self.0, json!({"action":"list"})).as_array().unwrap() {
            call(&self.0, json!({"action":"stop","id":c["id"]}));
        }
    }
}

#[test]
#[ignore = "requires codex app-server on PATH"]
fn one_host_server_serves_multiple_directories_and_concurrent_clients() {
    for direct in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("state");
        let _cleanup = Servers(root.clone());
        let joins: Vec<_> = (0..4)
            .map(|_| {
                let root = root.clone();
                std::thread::spawn(move || {
                    call(
                        &root,
                        json!({"action":"ensure","direct":direct,"server_name":"localhost"}),
                    )
                })
            })
            .collect();
        let servers: Vec<_> = joins.into_iter().map(|j| j.join().unwrap()).collect();
        let id = servers[0][0]["id"].as_str().unwrap();
        assert!(servers.iter().all(|s| s[0]["id"] == id));
        assert_eq!(
            call(&root, json!({"action":"list"}))
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let mut threads = Vec::new();
        for name in ["alpha", "beta project"] {
            let path = tmp.path().join(name);
            fs::create_dir(&path).unwrap();
            let c = helper(&root, &["__thread_start", id, path.to_str().unwrap()]);
            assert_eq!(c["cwd"], path.to_str().unwrap());
            threads.push(c);
        }
        assert_ne!(threads[0]["id"], threads[1]["id"]);
        let _attached: Vec<_> = threads
            .iter()
            .map(|c| resume(&servers[0][0], c["id"].as_str().unwrap()))
            .collect();
        let snapshot = helper(&root, &["__snapshot"]);
        assert_eq!(snapshot["complete"], true);
        for expected in &threads {
            let row = snapshot["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["id"] == expected["id"])
                .unwrap();
            assert_eq!(row["cwd"], expected["cwd"]);
            assert!(row.get("server_id").is_none());
            assert_eq!(
                helper(
                    &root,
                    &["__thread_read", id, expected["id"].as_str().unwrap()]
                )["id"],
                expected["id"]
            );
        }
        // A fresh thread must survive beyond in-memory discovery, before its
        // first turn. This catches the lazy-rollout resume failure in Codex.
        call(&root, json!({"action":"stop","id":id}));
        let replacement = call(
            &root,
            json!({"action":"ensure","direct":direct,"server_name":"localhost"}),
        );
        let replacement_id = replacement[0]["id"].as_str().unwrap();
        assert_ne!(id, replacement_id);
        for expected in threads {
            let saved = helper(
                &root,
                &[
                    "__thread_read",
                    replacement_id,
                    expected["id"].as_str().unwrap(),
                ],
            );
            assert_eq!(saved["id"], expected["id"]);
            assert_eq!(saved["cwd"], expected["cwd"]);
        }
    }
}

fn handshake(
    port: u64,
    token: Option<&str>,
    certificate: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    use rustls::pki_types::{CertificateDer, ServerName, pem::PemObject};
    trait Socket: Read + Write {}
    impl<T: Read + Write> Socket for T {}
    let socket = TcpStream::connect(("127.0.0.1", port as u16))?;
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut socket: Box<dyn Socket> = if let Some(certificate) = certificate {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from_pem_slice(certificate.as_bytes())?)?;
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Box::new(rustls::StreamOwned::new(
            rustls::ClientConnection::new(
                std::sync::Arc::new(config),
                ServerName::try_from("localhost")?,
            )?,
            socket,
        ))
    } else {
        Box::new(socket)
    };
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    write!(
        socket,
        "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n{auth}\r\n"
    )?;
    let mut buffer = [0; 2048];
    let n = socket.read(&mut buffer)?;
    Ok(String::from_utf8_lossy(&buffer[..n]).into_owned())
}
#[test]
#[ignore = "requires codex app-server on PATH"]
fn real_codex_persists_lists_authenticates_and_stops() {
    for direct in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("state");
        let rows = call(
            &root,
            json!({"action":"ensure","direct":direct,"server_name":"localhost"}),
        );
        let c = &rows[0];
        let id = c["id"].as_str().unwrap();
        let port = c["port"].as_u64().unwrap();
        // Always stop a successfully launched server, even if an assertion fails.
        let result = std::panic::catch_unwind(|| {
            assert_eq!(
                fs::read_link(format!("/proc/{}/cwd", c["pid"])).unwrap(),
                std::path::PathBuf::from(std::env::var("HOME").unwrap())
            );
            assert_eq!(call(&root, json!({"action":"list"}))[0]["id"], id);
            call(
                &root,
                json!({"action":"rename","id":id,"name":"workstation"}),
            );
            let saved = call(&root, json!({"action":"list"}));
            assert_eq!(saved[0]["name"], "workstation");
            assert_eq!(saved[0]["token"], c["token"]);
            assert_eq!(saved[0]["certificate"], c["certificate"]);
            let metadata = fs::metadata(root.join("server.json")).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
            let certificate = c["certificate"].as_str();
            if direct {
                assert!(certificate.is_some());
                assert!(
                    handshake(port, None, certificate)
                        .unwrap()
                        .starts_with("HTTP/1.1 401")
                );
                assert!(
                    handshake(port, Some("incorrect-token"), certificate)
                        .unwrap()
                        .starts_with("HTTP/1.1 401")
                );
                let wrong = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
                assert!(handshake(port, c["token"].as_str(), Some(&wrong.cert.pem())).is_err());
                assert!(
                    !handshake(port, c["token"].as_str(), None)
                        .unwrap_or_default()
                        .starts_with("HTTP/1.1 101")
                );
                assert_eq!(
                    fs::metadata(root.join("conns").join(format!("{id}.key")))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o600
                );
            }
            assert!(
                handshake(port, c["token"].as_str(), certificate)
                    .unwrap()
                    .starts_with("HTTP/1.1 101")
            );
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
        assert!(!root.join("conns").join(format!("{id}.key")).exists());
        assert!(!root.join("conns").join(format!("{id}.pem")).exists());
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }
}
