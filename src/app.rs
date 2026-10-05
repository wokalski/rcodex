//! Foreground application flow. The picker selects intent; Host owns routing.
use crate::{
    admin,
    cli::{Operation, Target},
    history,
    host::Host,
    ssh, ui,
};
use anyhow::{Context, Result, ensure};
use std::{
    io::IsTerminal,
    os::unix::process::CommandExt,
    process::{Command, Stdio},
};

pub fn run(operation: Operation) -> Result<()> {
    let status = Command::new("codex")
        .arg("--version")
        .stdout(Stdio::null())
        .status()
        .context("install Codex locally first")?;
    ensure!(status.success(), "local codex --version failed");

    if let Operation::Admin { host, action } = operation {
        let client = ssh::Client::connect(host)?;
        client.install()?;
        return admin::run(&client, action);
    }
    let mut cache = history::History::load()?;
    match operation {
        Operation::Open {
            host,
            direct,
            target,
        } => {
            cache.add_host(&host)?;
            Host::connect(host, direct)?.open(target, &mut cache)
        }
        Operation::Ensure {
            host,
            direct,
            name,
            json,
        } => {
            cache.add_host(&host)?;
            let host = Host::connect(host, direct)?;
            admin::print_servers(&host.rename(name)?, json)
        }
        Operation::Pick { mut host, direct } => {
            if let Some(host) = &host {
                cache.add_host(host)?;
            }
            ensure!(
                std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
                "the conversation picker needs a terminal; provide a remote path or --resume ID instead"
            );
            while let Some(choice) = ui::pick(&mut cache, host.as_deref())? {
                match choice {
                    ui::Choice::Connect { host: address } => {
                        let connected = Host::connect(address.clone(), direct)?;
                        cache.merge(&address, &connected.snapshot()?)?;
                        host = Some(address);
                    }
                    ui::Choice::New { host, path } => {
                        return Host::connect(host, direct)?.open(Target::New(path), &mut cache);
                    }
                    ui::Choice::Resume { host, conversation } => {
                        return Host::connect(host, direct)?
                            .open(Target::Resume(conversation.id), &mut cache);
                    }
                    ui::Choice::Shell { host, path } => {
                        return Err(ssh::Client::connect(host)?.shell(&path).exec())
                            .context("open remote shell");
                    }
                }
            }
            Ok(())
        }
        Operation::Admin { .. } => unreachable!("handled before opening history"),
    }
}
