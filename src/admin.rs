//! Explicit server administration, separate from conversation navigation.
use crate::{
    cli::Admin,
    remote::{self, Connection, Request},
    ssh::Client,
};
use anyhow::{Context, Result, ensure};
use std::{
    io::{IsTerminal, Write},
    os::unix::process::CommandExt,
};

fn public_server(c: &Connection) -> serde_json::Value {
    serde_json::json!({"id":c.id,"name":c.name,"path":c.path,"pid":c.pid,"port":c.port,
        "created":c.created,"log":c.log,"socket":c.socket,
        "transport":if c.socket.is_some(){"ssh-unix"}else if c.certificate.is_some(){"tls"}else{"ssh"}})
}

pub fn print_servers(rows: &[Connection], json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows.iter().map(public_server).collect::<Vec<_>>())?
        );
    } else if rows.is_empty() {
        println!("No running servers. Use rcodex HOST to connect.");
    } else {
        println!("{:<12} {:<20} TRANSPORT", "ID", "NAME");
        for c in rows {
            println!(
                "{:<12} {:<20} {}",
                &c.id[..8.min(c.id.len())],
                c.label().escape_debug(),
                public_server(c)["transport"].as_str().unwrap()
            );
        }
    }
    Ok(())
}

pub fn run(client: &Client, action: Admin) -> Result<()> {
    let select = |selector: &str| remote::select(client.call(Request::List)?, selector);
    match action {
        Admin::List { json } => print_servers(&client.call(Request::List)?, json)?,
        Admin::Sessions { json } => {
            let snapshot = client.snapshot()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshot)?);
            } else {
                for c in snapshot.sessions {
                    println!(
                        "{}  {}  {}",
                        c.id,
                        c.cwd.escape_debug(),
                        c.title.escape_debug()
                    );
                }
                for warning in snapshot.warnings {
                    eprintln!("{warning}");
                }
            }
        }
        Admin::Logs { server, lines } => {
            // Full IDs work after stop, when no running record remains.
            let id = if server.len() == 32 && server.bytes().all(|c| c.is_ascii_hexdigit()) {
                server
            } else {
                select(&server)?.id
            };
            println!("{}", client.logs(&id, lines)?);
        }
        Admin::Inspect(server) => println!("{}", client.inspect(&select(&server)?)),
        Admin::Shell(server) => {
            return Err(client.shell(&select(&server)?.path).exec()).context("open remote shell");
        }
        Admin::Rename { server, name } => {
            let id = select(&server)?.id;
            print_servers(&client.call(Request::Rename { id, name })?, false)?;
        }
        Admin::Stop { server, confirmed } => {
            let server = select(&server)?;
            if !confirmed && !confirm_stop(&server)? {
                return Ok(());
            }
            client.call(Request::Stop {
                id: server.id.clone(),
            })?;
            println!("Stopped {}", server.id);
        }
    }
    Ok(())
}

fn confirm_stop(server: &Connection) -> Result<bool> {
    ensure!(
        std::io::stdin().is_terminal(),
        "--stop needs confirmation; pass --yes in scripts"
    );
    eprint!(
        "Stop {}? ALL its running conversations will be interrupted. [y/N] ",
        server.label().escape_debug()
    );
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_output_never_serializes_server_credentials() {
        let server = serde_json::from_value(serde_json::json!({"id":"a","path":"/home","pid":1,"start":"s","port":1,"token":"SECRET","certificate":"CERTIFICATE","log":"log","created":0})).unwrap();
        let public = public_server(&server);
        assert_eq!(public["transport"], "tls");
        assert!(public.get("token").is_none());
        assert!(public.get("certificate").is_none());
    }
}
