mod admin;
mod app;
mod backend;
mod cli;
mod daemon;
mod helper;
mod history;
mod host;
mod remote;
mod rpc;
mod sessions;
mod ssh;
mod tls;
mod ui;

use anyhow::Result;
use clap::Parser;

fn main() {
    if let Err(error) = run() {
        eprintln!("rcodex: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if helper::dispatch(&args)? {
        return Ok(());
    }
    app::run(cli::Args::parse().operation()?)
}
