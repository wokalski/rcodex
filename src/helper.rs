//! Private process entry points. Keep their wire formats independent of the CLI.
use crate::{backend, remote, rpc, ssh, tls};
use anyhow::{Context, Result};
use std::{thread, time::Duration};

pub fn reply<T: serde::Serialize>(result: Result<T>) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string(&result.map_err(|e| format!("{e:#}")))?
    );
    Ok(())
}

pub fn dispatch(args: &[String]) -> Result<bool> {
    let arg = |n| args.get(n).context("missing helper argument");
    match args.first().map(String::as_str) {
        Some("__host") => backend::run(&args[1..])?,
        Some("__git") => reply(arg(1).and_then(|p| remote::git_status(p)))?,
        Some("__browse") => reply(arg(1).and_then(|p| remote::browse(p)))?,
        Some("__mkdir") => reply((|| remote::mkdir(arg(1)?, arg(2)?))())?,
        Some("__logs") => reply((|| remote::logs(arg(1)?, arg(2)?.parse()?))())?,
        Some("__snapshot") => reply(
            remote::handle(remote::Request::List).and_then(|rows| rpc::snapshot(rows.first())),
        )?,
        Some("__thread_start" | "__thread_read") => reply((|| {
            let c = remote::select(remote::handle(remote::Request::List)?, arg(1)?)?;
            if args[0] == "__thread_start" {
                rpc::start(&c, &remote::browse(arg(2)?)?.path)
            } else {
                rpc::read(&c, arg(2)?)
            }
        })())?,
        Some("__tls") => tls::serve(arg(1)?.parse()?, arg(2)?, arg(3)?, arg(4)?)?,
        Some("__remote") => {
            let result = arg(1).and_then(|s| remote::handle(serde_json::from_str(s)?));
            let reply = match result {
                Ok(rows) => remote::Reply { rows, error: None },
                Err(e) => remote::Reply {
                    rows: vec![],
                    error: Some(format!("{e:#}")),
                },
            };
            println!("{}", serde_json::to_string(&reply)?);
        }
        Some("__watch") => {
            let pid = arg(1)?.parse()?;
            while remote::identity(pid).as_ref() == Some(arg(2)?) {
                thread::sleep(Duration::from_millis(500));
            }
            ssh::cleanup(arg(3)?, arg(4)?, arg(5)?);
        }
        _ => return Ok(false),
    }
    Ok(true)
}
