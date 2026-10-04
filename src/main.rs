mod remote;
mod ssh;
mod tls;
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
    /// Connect directly using TLS authenticated through SSH (no SSH data tunnel)
    #[arg(long)]
    direct: bool,
    /// Resume a remote conversation (omit the ID to open Codex's session picker)
    #[arg(long, num_args = 0..=1, default_missing_value = "", conflicts_with = "last")]
    resume: Option<String>,
    /// Resume the most recently updated remote conversation in the selected project
    #[arg(long)]
    last: bool,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("rcodex: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let raw: Vec<String> = env::args().collect();
    if raw.get(1).map(String::as_str) == Some("__tls") {
        anyhow::ensure!(raw.len() == 6, "invalid TLS helper arguments");
        return tls::serve(raw[2].parse()?, &raw[3], &raw[4], &raw[5]);
    }
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
    client.ensure_login()?;
    client.install()?;
    let selected = if let Some(path) = args.path {
        client
            .call(remote::Request::Start {
                path,
                direct: args.direct,
                server_name: None,
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
    let mut command = codex_command(&endpoint, &selected, args.resume.as_deref(), args.last);
    if let Some(certificate) = &selected.certificate {
        let roots = client.directory().join("roots.pem");
        tls::write_roots(&roots, certificate)?;
        command.env("SSL_CERT_FILE", roots);
    }
    Err(command.exec()).context("launch local Codex")
}

fn codex_command(
    endpoint: &str,
    selected: &remote::Connection,
    resume: Option<&str>,
    last: bool,
) -> Command {
    let mut codex = Command::new("codex");
    if resume.is_some() || last {
        codex.arg("resume");
        if let Some(id) = resume.filter(|id| !id.is_empty()) {
            codex.arg(id);
        }
        if last {
            codex.arg("--last");
        }
    }
    codex.args(["--remote", endpoint, "--cd", &selected.path]);
    if let Some(token) = &selected.token {
        codex
            .env("RCODEX_AUTH_TOKEN", token)
            .args(["--remote-auth-token-env", "RCODEX_AUTH_TOKEN"]);
    }
    codex
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_modes_preserve_remote_endpoint_directory_and_authentication() {
        let connection = remote::Connection {
            id: "connection".into(),
            path: "/remote/project with spaces".into(),
            pid: 1,
            start: "start".into(),
            port: 1234,
            direct: true,
            token: Some("test-token".into()),
            certificate: None,
            log: "log".into(),
            created: 0,
        };
        for (flags, prefix) in [
            (vec![], vec![]),
            (vec!["--resume"], vec!["resume"]),
            (vec!["--resume", "session-id"], vec!["resume", "session-id"]),
            (vec!["--last"], vec!["resume", "--last"]),
        ] {
            let args =
                Args::try_parse_from(["rcodex", "devbox", "--direct"].into_iter().chain(flags))
                    .unwrap();
            let command = codex_command(
                "ws://remote:1234",
                &connection,
                args.resume.as_deref(),
                args.last,
            );
            let mut expected = prefix;
            expected.extend([
                "--remote",
                "ws://remote:1234",
                "--cd",
                "/remote/project with spaces",
                "--remote-auth-token-env",
                "RCODEX_AUTH_TOKEN",
            ]);
            assert_eq!(command.get_args().collect::<Vec<_>>(), expected);
            assert_eq!(
                command.get_envs().collect::<Vec<_>>(),
                vec![(
                    std::ffi::OsStr::new("RCODEX_AUTH_TOKEN"),
                    Some(std::ffi::OsStr::new("test-token"))
                )]
            );
        }
        assert!(Args::try_parse_from(["rcodex", "devbox", "--resume", "--last"]).is_err());
    }
}
