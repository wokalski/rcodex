//! An authenticated host and its server. Every conversation uses this endpoint;
//! only its exact ID and project directory vary.
use crate::{
    cli::Target,
    history::History,
    remote::{Connection, Request},
    sessions::{Conversation, Snapshot},
    ssh, tls,
};
use anyhow::{Context, Result};
use std::{os::unix::process::CommandExt, process::Command};

pub struct Host {
    client: ssh::Client,
    server: Connection,
    direct: bool,
}

impl Host {
    pub fn connect(address: String, direct: bool) -> Result<Self> {
        let client = ssh::Client::connect(address)?;
        client.ensure_login()?;
        client.install()?;
        let server = client
            .call(Request::Ensure {
                direct,
                server_name: None,
            })?
            .into_iter()
            .next()
            .context("no host server")?;
        Ok(Self {
            client,
            server,
            direct,
        })
    }

    pub fn snapshot(&self) -> Result<Snapshot> {
        self.client.snapshot()
    }

    pub fn rename(self, name: Option<String>) -> Result<Vec<Connection>> {
        match name {
            Some(name) => self.client.call(Request::Rename {
                id: self.server.id,
                name,
            }),
            None => Ok(vec![self.server]),
        }
    }

    fn conversation(&self, target: Target, cache: &History) -> Result<Conversation> {
        match target {
            Target::Resume(id) => self.client.read_thread(&self.server.id, &id),
            Target::Reconnect => {
                let saved = cache
                    .sessions()
                    .iter()
                    .find(|r| r.host == self.client.host && r.visited > 0)
                    .context(
                        "no locally visited conversation on this host; open the picker first",
                    )?;
                self.client
                    .read_thread(&self.server.id, &saved.conversation.id)
            }
            Target::Latest(path) => {
                let path = path
                    .map(|p| self.client.browse(&p).map(|d| d.path))
                    .transpose()?;
                let snapshot = self.snapshot()?;
                snapshot.require_complete()?;
                snapshot
                    .sessions
                    .into_iter()
                    .filter(|s| path.as_ref().is_none_or(|p| p == &s.cwd))
                    .max_by_key(|s| s.updated_at)
                    .context("no conversation found")
            }
            Target::New(path) => self
                .client
                .start_thread(&self.server.id, &self.client.browse(&path)?.path),
        }
    }

    pub fn open(&self, target: Target, cache: &mut History) -> Result<()> {
        let conversation = self.conversation(target, cache)?;
        let endpoint = self.client.endpoint(&self.server, self.direct)?;
        let mut command = codex_command(&endpoint, &self.server, &conversation);
        if let Some(certificate) = &self.server.certificate {
            let roots = self.client.directory().join("roots.pem");
            tls::write_roots(&roots, certificate)?;
            command.env("SSL_CERT_FILE", roots);
        }
        cache.visit(&self.client.host, conversation)?;
        Err(command.exec()).context("launch local Codex")
    }
}

fn codex_command(endpoint: &str, server: &Connection, conversation: &Conversation) -> Command {
    let mut command = Command::new("codex");
    command.args([
        "resume",
        &conversation.id,
        "--remote",
        endpoint,
        "--cd",
        &conversation.cwd,
    ]);
    if let Some(token) = &server.token {
        command
            .env("RCODEX_AUTH_TOKEN", token)
            .args(["--remote-auth-token-env", "RCODEX_AUTH_TOKEN"]);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_conversation_and_cwd_override_server_process_directory() {
        let server: Connection = serde_json::from_value(serde_json::json!({"id":"a","path":"/server/home","pid":1,"start":"s","port":1,"token":"SECRET","log":"log","created":0})).unwrap();
        let conversation = Conversation {
            id: "exact-thread-id".into(),
            cwd: "/different/project with spaces".into(),
            ..Default::default()
        };
        let command = codex_command("ws://localhost:123", &server, &conversation);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![
                "resume",
                "exact-thread-id",
                "--remote",
                "ws://localhost:123",
                "--cd",
                "/different/project with spaces",
                "--remote-auth-token-env",
                "RCODEX_AUTH_TOKEN"
            ]
        );
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == "RCODEX_AUTH_TOKEN"
                    && value == Some(std::ffi::OsStr::new("SECRET")))
        );
        assert!(!command.get_args().any(|arg| arg == "SECRET"));
    }
}
