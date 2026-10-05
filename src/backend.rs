//! Background operations use a cancellable child so blocking SSH and RPC work
//! never blocks the UI. SSH authentication prompts are reserved for foreground.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{env, process::Stdio, time::Duration};

#[derive(Serialize, Deserialize)]
enum Request {
    Snapshot,
    Browse { path: String },
    Mkdir { parent: String, name: String },
}

async fn query<T: serde::de::DeserializeOwned>(host: String, request: Request) -> Result<T> {
    let mut command = tokio::process::Command::new(env::current_exe()?);
    command
        .arg("__host")
        .arg(host)
        .arg(serde_json::to_string(&request)?)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .context("SSH discovery timed out; press r to retry or h to connect interactively")??;
    anyhow::ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reply: Result<T, String> =
        serde_json::from_slice(&output.stdout).context("decode host reply")?;
    reply.map_err(anyhow::Error::msg)
}

pub async fn refresh(host: String) -> Result<crate::sessions::Snapshot> {
    query(host, Request::Snapshot).await
}
pub async fn browse(host: String, path: String) -> Result<crate::remote::Directory> {
    query(host, Request::Browse { path }).await
}
pub async fn mkdir(host: String, parent: String, name: String) -> Result<crate::remote::Directory> {
    query(host, Request::Mkdir { parent, name }).await
}

pub fn run(args: &[String]) -> Result<()> {
    crate::helper::reply((|| -> Result<serde_json::Value> {
        let request: Request =
            serde_json::from_str(args.get(1).context("missing host operation")?)?;
        let client =
            crate::ssh::Client::connect_background(args.first().context("missing host")?.clone())?;
        client.install()?;
        Ok(match request {
            Request::Snapshot => serde_json::to_value(client.snapshot()?)?,
            Request::Browse { path } => serde_json::to_value(client.browse(&path)?)?,
            Request::Mkdir { parent, name } => serde_json::to_value(client.mkdir(&parent, &name)?)?,
        })
    })())
}
