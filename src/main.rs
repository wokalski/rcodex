mod remote;
mod ssh;
mod ui;

use anyhow::{Context, Result, bail};
use clap::Parser;
use std::{
    env,
    io::IsTerminal,
    os::unix::process::CommandExt,
    process::{Command, Stdio},
    thread,
    time::Duration,
};

#[derive(Parser, Debug)]
#[command(version, about = "Persistent remote Codex workspaces over SSH")]
struct Args {
    /// SSH alias, user@host, or ssh://user@host:port
    ssh_address: String,
    /// Existing remote project directory (omit to open the workspace picker)
    path: Option<String>,
    /// Connect with authenticated, unencrypted WebSockets; trusted networks only
    #[arg(long)]
    direct: bool,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("rcodex: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let raw: Vec<String> = env::args().collect();
    if raw.get(1).map(String::as_str) == Some("__browse") {
        let result = raw
            .get(2)
            .context("missing directory")
            .and_then(|path| remote::browse(path))
            .map_err(|error| format!("{error:#}"));
        println!("{}", serde_json::to_string(&result)?);
        return Ok(());
    }
    if raw.get(1).map(String::as_str) == Some("__remote") {
        let result = raw
            .get(2)
            .context("missing request")
            .and_then(|s| remote::handle(serde_json::from_str(s)?));
        let reply = match result {
            Ok(rows) => remote::Reply { rows, error: None },
            Err(e) => remote::Reply {
                rows: vec![],
                error: Some(format!("{e:#}")),
            },
        };
        println!("{}", serde_json::to_string(&reply)?);
        return Ok(());
    }
    if raw.get(1).map(String::as_str) == Some("__watch") {
        anyhow::ensure!(raw.len() == 7, "invalid watcher arguments");
        let pid = raw[2].parse()?;
        while remote::identity(pid).as_deref() == Some(&raw[3]) {
            thread::sleep(Duration::from_millis(500));
        }
        ssh::cleanup(&raw[4], &raw[5], &raw[6]);
        return Ok(());
    }
    let args = Args::parse();
    if args.ssh_address.starts_with('-') {
        bail!("invalid SSH address");
    }
    if args.path.is_none() && (!std::io::stdin().is_terminal() || !std::io::stdout().is_terminal())
    {
        bail!("the workspace picker needs a terminal; provide a remote path instead");
    }
    // Check before creating any persistent remote state.
    let status = Command::new("codex")
        .arg("--version")
        .stdout(Stdio::null())
        .status()
        .context("install Codex locally first")?;
    anyhow::ensure!(status.success(), "local codex --version failed");
    if args.direct {
        eprintln!("Direct ws:// is unencrypted. Use only on a trusted network such as Tailscale.");
    }
    let client = ssh::Client::connect(args.ssh_address)?;
    let pid = std::process::id();
    let start = remote::identity(pid).context("cannot identify local process")?;
    // Covers interrupted setup as well as Codex exit, surviving the final exec.
    let mut watcher = Command::new(env::current_exe()?);
    watcher
        .args([
            "__watch",
            &pid.to_string(),
            &start,
            &client.host,
            &client.control().to_string_lossy(),
            &client.directory().to_string_lossy(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        watcher.pre_exec(|| {
            nix::unistd::setsid().map_err(std::io::Error::from)?;
            Ok(())
        });
    }
    watcher.spawn().context("start tunnel cleanup watcher")?;
    client.install()?;
    let selected = if let Some(path) = args.path {
        client
            .call(remote::Request::Start {
                path,
                direct: args.direct,
            })?
            .into_iter()
            .next()
    } else {
        ui::pick(client.clone(), args.direct)?
    };
    let Some(selected) = selected else {
        return Ok(());
    };
    let endpoint = client.endpoint(&selected, args.direct)?;
    let mut codex = Command::new("codex");
    codex.args(["--remote", &endpoint, "--cd", &selected.path]);
    if let Some(token) = &selected.token {
        codex
            .env("RCODEX_AUTH_TOKEN", token)
            .args(["--remote-auth-token-env", "RCODEX_AUTH_TOKEN"]);
    }
    Err(codex.exec()).context("launch local Codex")
}
