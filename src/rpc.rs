//! Small synchronous client for the Codex app-server JSON-RPC protocol.
//!
//! A fresh websocket is deliberately used for each operation.  Besides making
//! this module usable by the synchronous UI, that keeps a stalled app-server
//! from retaining a UI worker indefinitely.

use crate::{
    remote::Connection,
    sessions::{Conversation, Snapshot},
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use rustls::pki_types::{CertificateDer, ServerName, pem::PemObject};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    net::{SocketAddr, TcpStream},
    os::unix::net::UnixStream,
    sync::Arc,
    time::{Duration, Instant},
};
use tungstenite::{Message, WebSocket, client, http::Request};

const IO_TIMEOUT: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const PAGE_SIZE: u32 = 100;

type TlsSocket = Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>;

enum Socket {
    Plain(WebSocket<TcpStream>),
    Tls(WebSocket<TlsSocket>),
    Unix(WebSocket<UnixStream>),
}

impl Socket {
    fn send(&mut self, message: Message) -> tungstenite::Result<()> {
        match self {
            Self::Plain(ws) => ws.send(message),
            Self::Tls(ws) => ws.send(message),
            Self::Unix(ws) => ws.send(message),
        }
    }

    fn read(&mut self) -> tungstenite::Result<Message> {
        match self {
            Self::Plain(ws) => ws.read(),
            Self::Tls(ws) => ws.read(),
            Self::Unix(ws) => ws.read(),
        }
    }
}

struct Rpc {
    socket: Socket,
    next_id: u64,
}

impl Rpc {
    fn connect(connection: &Connection) -> Result<Self> {
        let socket = if let Some(path) = &connection.socket {
            let stream = UnixStream::connect(path).context("connect to Codex app daemon")?;
            stream.set_read_timeout(Some(IO_TIMEOUT))?;
            stream.set_write_timeout(Some(IO_TIMEOUT))?;
            Socket::Unix(
                client("ws://localhost/rpc", stream)
                    .context("daemon websocket handshake")?
                    .0,
            )
        } else {
            Self::connect_tcp(connection)?
        };
        let mut rpc = Self { socket, next_id: 1 };
        rpc.call(
            "initialize",
            json!({"clientInfo":{"name":"rcodex","title":"rcodex","version":env!("CARGO_PKG_VERSION")}}),
        )?;
        rpc.notify("initialized", json!({}))?;
        Ok(rpc)
    }

    fn connect_tcp(connection: &Connection) -> Result<Socket> {
        let address = SocketAddr::from(([127, 0, 0, 1], connection.port));
        let tcp = TcpStream::connect_timeout(&address, IO_TIMEOUT)
            .with_context(|| format!("connect to app-server {}", connection.id))?;
        tcp.set_read_timeout(Some(IO_TIMEOUT))?;
        tcp.set_write_timeout(Some(IO_TIMEOUT))?;

        let secure = connection.certificate.is_some();
        let scheme = if secure { "wss" } else { "ws" };
        let mut builder = Request::builder()
            .method("GET")
            .uri(format!("{scheme}://127.0.0.1:{}/", connection.port))
            .header("Host", format!("127.0.0.1:{}", connection.port))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header(
                "Sec-WebSocket-Key",
                tungstenite::handshake::client::generate_key(),
            );
        if let Some(token) = connection.token.as_deref() {
            ensure!(
                !token.bytes().any(|byte| matches!(byte, b'\r' | b'\n')),
                "invalid app-server token"
            );
            builder = builder.header("Authorization", format!("Bearer {token}"));
        }
        let request = builder.body(())?;

        let socket = if let Some(pem) = connection.certificate.as_deref() {
            let mut roots = rustls::RootCertStore::empty();
            roots.add(CertificateDer::from_pem_slice(pem.as_bytes())?)?;
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let session = rustls::ClientConnection::new(
                Arc::new(config),
                ServerName::try_from("127.0.0.1".to_owned())?,
            )?;
            let stream = Box::new(rustls::StreamOwned::new(session, tcp));
            Socket::Tls(
                client(request, stream)
                    .context("websocket TLS handshake")?
                    .0,
            )
        } else {
            Socket::Plain(client(request, tcp).context("websocket handshake")?.0)
        };
        Ok(socket)
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.socket.send(Message::Text(
            json!({"jsonrpc":"2.0","method":method,"params":params})
                .to_string()
                .into(),
        ))?;
        Ok(())
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.socket.send(Message::Text(
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
                .to_string()
                .into(),
        ))?;
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            ensure!(Instant::now() < deadline, "{method} timed out");
            match self.socket.read() {
                Ok(Message::Text(text)) => {
                    let value: Value = serde_json::from_str(&text)
                        .with_context(|| format!("invalid JSON during {method}"))?;
                    // Notifications and unrelated responses can be interleaved.
                    if value.get("id").and_then(Value::as_u64) != Some(id) {
                        continue;
                    }
                    if let Some(error) = value.get("error") {
                        let message = error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown JSON-RPC error");
                        bail!("{method}: {message}");
                    }
                    return value
                        .get("result")
                        .cloned()
                        .ok_or_else(|| anyhow!("{method}: response has no result"));
                }
                Ok(Message::Ping(data)) => self.socket.send(Message::Pong(data))?,
                Ok(Message::Close(frame)) => bail!("app-server closed connection: {frame:?}"),
                Ok(_) => {}
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) => {}
                Err(error) => return Err(error).with_context(|| format!("read {method} response")),
            }
        }
    }
}

fn conversation(thread: &Value) -> Result<Conversation> {
    let id = thread
        .get("id")
        .and_then(Value::as_str)
        .context("thread has no id")?;
    let cwd = thread
        .get("cwd")
        .and_then(Value::as_str)
        .context("thread has no cwd")?;
    let title = thread
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| thread.get("preview").and_then(Value::as_str))
        .unwrap_or("");
    let status = thread
        .get("status")
        .and_then(|s| s.get("type").and_then(Value::as_str).or_else(|| s.as_str()))
        .unwrap_or("unknown");
    Ok(Conversation {
        id: id.to_owned(),
        title: title.to_owned(),
        cwd: cwd.to_owned(),
        created_at: thread
            .get("createdAt")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        updated_at: thread
            .get("updatedAt")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        status: status.to_owned(),
    })
}

fn paged(rpc: &mut Rpc, method: &str, mut base: Value) -> Result<Vec<Value>> {
    let mut output = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen = HashSet::new();
    loop {
        let object = base
            .as_object_mut()
            .context("pagination params are not an object")?;
        object.insert("limit".into(), json!(PAGE_SIZE));
        object.insert(
            "cursor".into(),
            cursor.clone().map_or(Value::Null, Value::String),
        );
        let response = rpc.call(method, base.clone())?;
        output.extend(
            response
                .get("data")
                .and_then(Value::as_array)
                .context("page has no data")?
                .iter()
                .cloned(),
        );
        cursor = response
            .get("nextCursor")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let Some(next) = cursor.as_ref() else { break };
        ensure!(
            seen.insert(next.clone()),
            "{method} returned a repeated cursor"
        );
    }
    Ok(output)
}

fn list_threads(connection: &Connection) -> Result<Vec<Conversation>> {
    let mut rpc = Rpc::connect(connection)?;
    // Deliberately omit cwd: shared servers discover every project on the host.
    let rows = paged(
        &mut rpc,
        "thread/list",
        json!({"sourceKinds":["cli","vscode","exec","appServer"]}),
    )?;
    rows.iter().map(conversation).collect()
}

fn loaded_threads(connection: &Connection) -> Result<Vec<Conversation>> {
    let mut rpc = Rpc::connect(connection)?;
    let ids = paged(&mut rpc, "thread/loaded/list", json!({}))?;
    let mut rows = Vec::new();
    for id in ids {
        let id = id.as_str().context("loaded thread id is not a string")?;
        let value = rpc.call("thread/read", json!({"threadId":id,"includeTurns":false}))?;
        rows.push(conversation(
            value.get("thread").context("thread/read has no thread")?,
        )?);
    }
    Ok(rows)
}

/// Discover every project on the host and overlay current runtime status.
pub fn snapshot(connection: Option<&Connection>) -> Result<Snapshot> {
    let Some(connection) = connection else {
        return Ok(Snapshot::default());
    };
    let mut warnings = Vec::new();
    let mut sessions: HashMap<String, Conversation> = HashMap::new();
    let mut complete = true;
    let mut server_running = false;

    for (source, result) in [
        ("persisted", list_threads(connection)),
        ("loaded", loaded_threads(connection)),
    ] {
        match result {
            Ok(rows) => {
                server_running = true;
                for row in rows {
                    sessions.insert(row.id.clone(), row);
                }
            }
            Err(error) => {
                complete = false;
                warnings.push(format!(
                    "{}: {source} discovery failed: {error:#}",
                    connection.id
                ));
            }
        }
    }
    let mut sessions: Vec<_> = sessions.into_values().collect();
    sessions.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(Snapshot {
        server_running,
        sessions,
        complete,
        warnings,
    })
}

/// Create a thread only; no turn is submitted.
pub fn start(connection: &Connection, cwd: &str) -> Result<Conversation> {
    ensure!(!cwd.is_empty(), "cwd must not be empty");
    let mut rpc = Rpc::connect(connection)?;
    let value = rpc.call("thread/start", json!({"cwd":cwd}))?;
    let created = conversation(value.get("thread").context("thread/start has no thread")?)?;
    // Codex 0.160 stages empty threads in memory and cannot reliably resume
    // them from another client. Archive materializes their history and index;
    // immediately unarchive this newly allocated, untouched thread. Reading
    // history alone fails for fresh SQLite indexes ("list_turns unsupported").
    // This never touches an existing conversation or submits a model turn.
    rpc.call("thread/archive", json!({"threadId":created.id}))?;
    let value = rpc
        .call("thread/unarchive", json!({"threadId":created.id}))
        .with_context(|| {
            format!(
                "new conversation {} was saved but remains archived",
                created.id
            )
        })?;
    conversation(
        value
            .get("thread")
            .context("thread/unarchive has no thread")?,
    )
}

/// Read thread metadata without loading turns.
pub fn read(connection: &Connection, id: &str) -> Result<Conversation> {
    ensure!(!id.is_empty(), "thread id must not be empty");
    let mut rpc = Rpc::connect(connection)?;
    let value = rpc.call("thread/read", json!({"threadId":id,"includeTurns":false}))?;
    let thread = value.get("thread").context("thread/read has no thread")?;
    ensure!(
        thread.get("id").and_then(Value::as_str) == Some(id),
        "thread/read returned a different thread id"
    );
    conversation(thread)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::TcpListener, thread};

    fn thread_value(id: &str, cwd: &str, status: &str) -> Value {
        json!({"id":id,"preview":format!("task {id}"),"cwd":cwd,"createdAt":10,"updatedAt":20,"status":{"type":status}})
    }
    // tungstenite fixes the callback error type to an HTTP response.
    #[allow(clippy::result_large_err)]
    fn mock(
        id: &str,
        connections: usize,
        handler: impl Fn(&str, &Value) -> Value + Send + 'static,
    ) -> (Connection, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let c = serde_json::from_value(json!({"id":id,"path":"/server-cwd","pid":1,"start":"s","port":listener.local_addr().unwrap().port(),"token":"secret","log":"log","created":0})).unwrap();
        let task = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            for _ in 0..connections {
                let stream = loop {
                    match listener.accept() {
                        Ok((s, _)) => break s,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "missing RPC connection");
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(e) => panic!("{e}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut ws = tungstenite::accept_hdr(
                    stream,
                    |request: &tungstenite::handshake::server::Request, response| {
                        assert_eq!(request.headers()["authorization"], "Bearer secret");
                        Ok(response)
                    },
                )
                .unwrap();
                while let Ok(message) = ws.read() {
                    let Message::Text(text) = message else {
                        continue;
                    };
                    let request: Value = serde_json::from_str(&text).unwrap();
                    let method = request["method"].as_str().unwrap();
                    if method == "initialized" {
                        continue;
                    }
                    let result = if method == "initialize" {
                        json!({})
                    } else {
                        handler(method, &request["params"])
                    };
                    ws.send(Message::Text(
                        json!({"method":"notice","params":{}}).to_string().into(),
                    ))
                    .unwrap();
                    ws.send(Message::Text(
                        json!({"id":request["id"],"result":result})
                            .to_string()
                            .into(),
                    ))
                    .unwrap();
                }
            }
        });
        (c, task)
    }

    #[test]
    fn discovery_pages_all_directories_and_overlays_loaded_conversations() {
        let (server, task) = mock("host", 2, |method, params| match method {
            "thread/list" => {
                assert!(params.get("cwd").is_none());
                assert!(
                    params["sourceKinds"]
                        .as_array()
                        .unwrap()
                        .contains(&json!("appServer"))
                );
                if params["cursor"].is_null() {
                    json!({"data":[thread_value("one","/alpha","notLoaded")],"nextCursor":"next"})
                } else {
                    assert_eq!(params["cursor"], "next");
                    json!({"data":[thread_value("two","/beta","notLoaded")],"nextCursor":null})
                }
            }
            "thread/loaded/list" => json!({"data":["two"]}),
            "thread/read" => {
                assert_eq!(params["threadId"], "two");
                assert_eq!(params["includeTurns"], false);
                json!({"thread":thread_value("two","/beta","active")})
            }
            _ => panic!("unexpected {method}"),
        });
        let result = snapshot(Some(&server)).unwrap();
        assert!(result.complete);
        assert_eq!(result.sessions.len(), 2);
        let live = result.sessions.iter().find(|r| r.id == "two").unwrap();
        assert_eq!(live.status, "active");
        assert!(!serde_json::to_string(&result).unwrap().contains("secret"));
        task.join().unwrap();
    }

    #[test]
    fn loaded_discovery_failure_keeps_persisted_rows_without_claiming_complete() {
        let (server, task) = mock("host", 2, |method, _| match method {
            "thread/list" => json!({"data":[thread_value("saved","/other","notLoaded")]}),
            "thread/loaded/list" => json!({"invalid":"no data"}),
            _ => panic!("unexpected {method}"),
        });
        let result = snapshot(Some(&server)).unwrap();
        assert!(!result.complete);
        assert!(result.server_running);
        assert_eq!(result.sessions[0].id, "saved");
        assert!(!result.warnings.is_empty());
        task.join().unwrap();
    }

    #[test]
    fn create_uses_requested_cwd_and_read_requires_exact_id() {
        let (server, task) = mock("host", 2, |method, params| match method {
            "thread/start" => {
                assert_eq!(params["cwd"], "/new project");
                json!({"thread":thread_value("exact","/new project","idle")})
            }
            "thread/read" => {
                assert_eq!(params["threadId"], "requested");
                json!({"thread":thread_value("different","/wrong","idle")})
            }
            "thread/archive" => {
                assert_eq!(params["threadId"], "exact");
                json!({})
            }
            "thread/unarchive" => {
                assert_eq!(params["threadId"], "exact");
                json!({"thread":thread_value("exact","/new project","notLoaded")})
            }
            _ => panic!("unexpected {method}"),
        });
        assert_eq!(start(&server, "/new project").unwrap().id, "exact");
        assert!(
            read(&server, "requested")
                .unwrap_err()
                .to_string()
                .contains("different thread id")
        );
        task.join().unwrap();
    }
}
