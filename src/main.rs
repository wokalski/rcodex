mod remote;
mod ssh;
mod tls;
mod ui;

use anyhow::{Context, Result, bail};
use clap::Parser;
use std::{
    env,
    io::{IsTerminal, Write},
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
    #[arg(group = "target")]
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
    /// List running servers without opening Codex
    #[arg(long, group = "target")]
    list: bool,
    /// Attach by server name, exact path, or unique ID prefix (at least 4 characters)
    #[arg(long, group = "target", value_name = "SERVER")]
    attach: Option<String>,
    /// Return to the last opened running server and continue its latest conversation
    #[arg(long, group = "target")]
    reconnect: bool,
    /// Show the tail of a server's log
    #[arg(long, group = "target", value_name = "SERVER")]
    logs: Option<String>,
    /// Number of log lines (maximum 2000; log reads are capped at 128 KiB)
    #[arg(long, default_value_t = 200, requires = "logs", value_parser = clap::value_parser!(u16).range(1..=2000))]
    lines: u16,
    /// Stop one server; prompts unless --yes is supplied
    #[arg(long, group = "target", value_name = "SERVER")]
    stop: Option<String>,
    /// Confirm --stop without prompting
    #[arg(long, requires = "stop")]
    yes: bool,
    /// Rename a server using --name (an empty name restores the folder label)
    #[arg(long, group = "target", requires = "name", value_name = "SERVER")]
    rename: Option<String>,
    /// Friendly name for a new server or --rename
    #[arg(long)]
    name: Option<String>,
    /// Start a server in PATH and exit without opening Codex
    #[arg(long, requires = "path", conflicts_with_all = ["resume", "last"])]
    detach: bool,
    /// Print --list or --detach output as JSON, without credentials
    #[arg(long)]
    json: bool,
}

impl Args {
    fn management(&self) -> bool {
        self.list || self.logs.is_some() || self.stop.is_some() || self.rename.is_some()
    }
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.json || self.list || self.detach,
            "--json requires --list or --detach"
        );
        anyhow::ensure!(
            self.name.is_none() || self.path.is_some() || self.rename.is_some(),
            "--name requires a project path or --rename"
        );
        anyhow::ensure!(
            !self.management() || (!self.direct && self.resume.is_none() && !self.last),
            "--direct, --resume and --last apply only when opening a workspace"
        );
        Ok(())
    }
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
    if matches!(raw.get(1).map(String::as_str), Some("__browse" | "__mkdir")) {
        let result = raw
            .get(2)
            .context("missing directory")
            .and_then(|path| {
                if raw[1] == "__mkdir" {
                    remote::mkdir(path, raw.get(3).context("missing folder name")?)
                } else {
                    remote::browse(path)
                }
            })
            .map_err(|error| format!("{error:#}"));
        println!("{}", serde_json::to_string(&result)?);
        return Ok(());
    }
    if raw.get(1).map(String::as_str) == Some("__logs") {
        let result = (|| -> Result<String> {
            remote::logs(
                raw.get(2).context("missing server ID")?,
                raw.get(3).context("missing line count")?.parse()?,
            )
        })()
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
    args.validate()?;
    if args.ssh_address.starts_with('-') {
        bail!("invalid SSH address");
    }
    if !args.management()
        && args.path.is_none()
        && args.attach.is_none()
        && !args.reconnect
        && (!std::io::stdin().is_terminal() || !std::io::stdout().is_terminal())
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
    let client = ssh::Client::connect(args.ssh_address.clone())?;
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
    if !args.management() {
        client.ensure_login()?;
    }
    client.install()?;
    if args.management() {
        return manage(&client, &args);
    }
    let selected = if let Some(path) = args.path {
        client
            .call(remote::Request::Start {
                path,
                direct: args.direct,
                server_name: None,
                name: args.name,
            })?
            .into_iter()
            .next()
            .map(|c| (c, ui::Open::New))
    } else if args.reconnect {
        Some((recent(client.call(remote::Request::List)?)?, ui::Open::Last))
    } else if let Some(selector) = args.attach {
        Some((
            select(client.call(remote::Request::List)?, &selector)?,
            ui::Open::New,
        ))
    } else {
        ui::pick(client.clone(), args.direct)?
    };
    let Some((selected, open)) = selected else {
        return Ok(());
    };
    if args.detach {
        print_servers(&[selected], args.json)?;
        return Ok(());
    }
    let endpoint = client.endpoint(&selected, args.direct)?;
    client.call(remote::Request::Visit {
        id: selected.id.clone(),
    })?;
    let resume = args
        .resume
        .as_deref()
        .or_else(|| matches!(open, ui::Open::Resume).then_some(""));
    let last = args.last || (args.resume.is_none() && matches!(open, ui::Open::Last));
    let mut command = codex_command(&endpoint, &selected, resume, last);
    if let Some(certificate) = &selected.certificate {
        let roots = client.directory().join("roots.pem");
        tls::write_roots(&roots, certificate)?;
        command.env("SSL_CERT_FILE", roots);
    }
    Err(command.exec()).context("launch local Codex")
}

fn recent(rows: Vec<remote::Connection>) -> Result<remote::Connection> {
    rows.into_iter()
        .filter(|c| c.last_used > 0)
        .max_by(|a, b| a.last_used.cmp(&b.last_used).then(a.id.cmp(&b.id)))
        .context("no previously opened server is still running; open the workspace picker first")
}

fn select(rows: Vec<remote::Connection>, selector: &str) -> Result<remote::Connection> {
    if let Some(row) = rows.iter().find(|r| r.id == selector) {
        return Ok(row.clone());
    }
    let mut matches = rows.into_iter().filter(|r| {
        r.name.as_deref() == Some(selector)
            || r.path == selector
            || (selector.len() >= 4 && r.id.starts_with(selector))
    });
    let row = matches
        .next()
        .with_context(|| format!("no running server matches {selector:?}; use --list"))?;
    anyhow::ensure!(
        matches.next().is_none(),
        "ambiguous server {selector:?}; use a longer ID from --list"
    );
    Ok(row)
}

fn public_server(c: &remote::Connection) -> serde_json::Value {
    // Deliberate allowlist: never serialize the remote protocol record to stdout.
    serde_json::json!({"id": c.id, "name": c.name, "path": c.path, "pid": c.pid,
        "port": c.port, "created": c.created, "log": c.log,
        "favorite": c.favorite, "last_used": c.last_used,
        "transport": if c.certificate.is_some() { "tls" } else if c.direct { "legacy-direct" } else { "ssh" }})
}

fn print_servers(rows: &[remote::Connection], json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows.iter().map(public_server).collect::<Vec<_>>())?
        );
    } else if rows.is_empty() {
        println!("No running servers. Start one with: rcodex HOST PATH --detach");
    } else {
        println!("{:<12} {:<20} {:<9} PATH", "ID", "NAME", "TRANSPORT");
        for c in rows {
            let value = public_server(c);
            println!(
                "{:<12} {:<20} {:<9} {}",
                &c.id[..8.min(c.id.len())],
                c.label().escape_debug().to_string(),
                value["transport"].as_str().unwrap(),
                c.path.escape_debug()
            );
        }
    }
    Ok(())
}

fn manage(client: &ssh::Client, args: &Args) -> Result<()> {
    // Logs outlive servers. A full ID can read a stopped/crashed server's log.
    if let Some(id) = &args.logs
        && id.len() == 32
        && id.bytes().all(|c| c.is_ascii_hexdigit())
    {
        println!("{}", client.logs(id, args.lines as usize)?);
        return Ok(());
    }
    let rows = client.call(remote::Request::List)?;
    if args.list {
        return print_servers(&rows, args.json);
    }
    let selector = args
        .logs
        .as_ref()
        .or(args.stop.as_ref())
        .or(args.rename.as_ref())
        .context("missing server")?;
    let c = select(rows, selector)?;
    if args.logs.is_some() {
        println!("{}", client.logs(&c.id, args.lines as usize)?);
    } else if args.rename.is_some() {
        let rows = client.call(remote::Request::Rename {
            id: c.id,
            name: args.name.clone().unwrap_or_default(),
        })?;
        print_servers(&rows, false)?;
    } else {
        if !args.yes {
            anyhow::ensure!(
                std::io::stdin().is_terminal(),
                "--stop needs confirmation; pass --yes in scripts"
            );
            eprint!(
                "Stop {} ({})? All clients will disconnect. [y/N] ",
                c.label().escape_debug(),
                c.id
            );
            std::io::stderr().flush()?;
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            if !matches!(answer.trim(), "y" | "Y" | "yes") {
                println!("Cancelled; server left running.");
                return Ok(());
            }
        }
        client.call(remote::Request::Stop { id: c.id.clone() })?;
        println!("Stopped {}", c.id);
    }
    Ok(())
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

    fn connection(id: &str, name: &str, path: &str) -> remote::Connection {
        remote::Connection {
            id: id.into(),
            name: Some(name.into()),
            favorite: false,
            last_used: 0,
            path: path.into(),
            pid: 1,
            start: "private-start".into(),
            port: 1234,
            direct: true,
            token: Some("SECRET_TOKEN".into()),
            certificate: Some("SECRET_CERT".into()),
            log: "/logs/server.log".into(),
            created: 100,
        }
    }

    #[test]
    fn reconnect_uses_visit_time_not_creation_or_favorites() {
        let mut a = connection("a", "old", "/a");
        let mut b = connection("b", "new", "/b");
        assert!(recent(vec![a.clone(), b.clone()]).is_err());
        a.last_used = 20;
        b.last_used = 10;
        b.created = 900;
        b.favorite = true;
        assert_eq!(recent(vec![a.clone(), b.clone()]).unwrap().id, "a");
        assert_eq!(recent(vec![b, a]).unwrap().id, "a");
        assert!(recent(vec![]).is_err());
        assert!(Args::try_parse_from(["rcodex", "host", "--reconnect", "/project"]).is_err());
    }

    #[test]
    fn selectors_refuse_ambiguity_and_output_never_contains_credentials() {
        let a = connection("abcd1111", "nightly", "/first");
        let b = connection("abcd2222", "nightly", "/second");
        for selector in ["abcd", "nightly", "abc", "missing"] {
            assert!(select(vec![a.clone(), b.clone()], selector).is_err());
        }
        assert_eq!(
            select(vec![a.clone(), b.clone()], "abcd2").unwrap().path,
            "/second"
        );
        assert_eq!(
            select(vec![a.clone(), b.clone()], "/first").unwrap().id,
            "abcd1111"
        );
        assert_eq!(
            select(vec![a.clone(), b], "abcd1111").unwrap().path,
            "/first"
        );
        let output = public_server(&a);
        assert_eq!(output["name"], "nightly");
        assert_eq!(output["transport"], "tls");
        assert!(output.get("token").is_none());
        assert!(output.get("certificate").is_none());
        assert!(output.get("start").is_none());
        assert!(!output.to_string().contains("SECRET"));
    }

    #[test]
    fn management_flag_combinations_are_unambiguous() {
        for flags in [
            vec!["--list", "--json"],
            vec!["--attach", "nightly", "--last"],
            vec!["/project", "--detach", "--name", "nightly", "--json"],
            vec!["--logs", "abcd", "--lines", "50"],
            vec!["--rename", "abcd", "--name", "new"],
            vec!["--stop", "abcd", "--yes"],
        ] {
            Args::try_parse_from(["rcodex", "devbox"].into_iter().chain(flags))
                .unwrap()
                .validate()
                .unwrap();
        }
        for flags in [
            vec!["--list", "--stop", "abcd"],
            vec!["/path", "--attach", "abcd"],
            vec!["--detach"],
            vec!["--json"],
            vec!["--name", "orphan"],
            vec!["--lines", "1"],
            vec!["--logs", "abcd", "--lines", "2001"],
            vec!["--list", "--direct"],
            vec!["--rename", "abcd"],
            vec!["--yes"],
        ] {
            let result =
                Args::try_parse_from(["rcodex", "devbox"].into_iter().chain(flags.clone()))
                    .map_err(anyhow::Error::from)
                    .and_then(|a| a.validate());
            assert!(result.is_err(), "accepted {flags:?}");
        }
    }

    #[test]
    fn resume_modes_preserve_remote_endpoint_directory_and_authentication() {
        let connection = remote::Connection {
            id: "connection".into(),
            name: None,
            favorite: false,
            last_used: 0,
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
