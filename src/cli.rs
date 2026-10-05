//! Convert the public CLI into one operation before doing any I/O.
use anyhow::{Result, ensure};
use clap::Parser;

pub enum Operation {
    Pick {
        host: Option<String>,
        direct: bool,
    },
    Open {
        host: String,
        direct: bool,
        target: Target,
    },
    Ensure {
        host: String,
        direct: bool,
        name: Option<String>,
        json: bool,
    },
    Admin {
        host: String,
        action: Admin,
    },
}

pub enum Target {
    New(String),
    Resume(String),
    Reconnect,
    Latest(Option<String>),
}

pub enum Admin {
    List { json: bool },
    Sessions { json: bool },
    Logs { server: String, lines: usize },
    Inspect(String),
    Shell(String),
    Rename { server: String, name: String },
    Stop { server: String, confirmed: bool },
}

#[derive(Parser, Debug)]
#[command(version, about = "Remote Codex conversations, one app-server per host")]
pub struct Args {
    /// SSH alias or user@host (omit for conversations across remembered hosts)
    ssh_address: Option<String>,
    /// Start a conversation in this remote directory; omit for the picker
    #[arg(group = "target")]
    path: Option<String>,
    /// Use SSH-authenticated direct TLS instead of an SSH data tunnel
    #[arg(long)]
    direct: bool,
    /// Resume an exact conversation ID; omit the ID to open the rcodex picker
    #[arg(long, num_args = 0..=1, default_missing_value = "", conflicts_with = "last")]
    resume: Option<String>,
    /// Resume the most recently updated conversation (optionally restricted by PATH)
    #[arg(long)]
    last: bool,
    /// List registered app servers (administration, not conversations)
    #[arg(long, group = "target")]
    list: bool,
    /// List conversations across all directories without opening the TUI
    #[arg(long, group = "target")]
    sessions: bool,
    /// Resume the conversation most recently visited locally on this host
    #[arg(long, group = "target")]
    reconnect: bool,
    /// Show a server and its original directory's Git status
    #[arg(long, group = "target", value_name = "SERVER")]
    inspect: Option<String>,
    /// Open a shell in a server's original directory
    #[arg(long, group = "target", value_name = "SERVER")]
    shell: Option<String>,
    /// Read a server's log
    #[arg(long, group = "target", value_name = "SERVER")]
    logs: Option<String>,
    #[arg(long, default_value_t = 200, requires = "logs", value_parser = clap::value_parser!(u16).range(1..=2000))]
    lines: u16,
    /// Stop a server, interrupting ALL conversations running in it
    #[arg(long, group = "target", value_name = "SERVER")]
    stop: Option<String>,
    #[arg(long, requires = "stop")]
    yes: bool,
    #[arg(long, group = "target", requires = "name", value_name = "SERVER")]
    rename: Option<String>,
    /// Friendly server name for --rename or --detach
    #[arg(long)]
    name: Option<String>,
    /// Ensure the host server is running; exit without creating a conversation
    #[arg(long, requires = "path", conflicts_with_all = ["resume", "last"])]
    detach: bool,
    /// Machine-readable output for --list, --sessions or --detach
    #[arg(long)]
    json: bool,
}

impl Args {
    pub fn operation(self) -> Result<Operation> {
        let admin = self.list
            || self.sessions
            || self.logs.is_some()
            || self.stop.is_some()
            || self.rename.is_some()
            || self.inspect.is_some()
            || self.shell.is_some();
        ensure!(
            self.ssh_address.is_some()
                || (!admin && !self.reconnect && self.resume.is_none() && !self.last),
            "this operation needs an SSH address"
        );
        ensure!(
            !self.json || self.list || self.sessions || self.detach,
            "--json requires --list, --sessions or --detach"
        );
        ensure!(
            self.name.is_none() || self.detach || self.rename.is_some(),
            "--name requires --detach or --rename"
        );
        ensure!(
            !admin || (!self.direct && self.resume.is_none() && !self.last),
            "--direct, --resume and --last apply only when opening conversations"
        );

        // The parser's target group makes these mutually exclusive. Convert once;
        // no execution path needs to rediscover intent from unrelated booleans.
        let action = if self.list {
            Some(Admin::List { json: self.json })
        } else if self.sessions {
            Some(Admin::Sessions { json: self.json })
        } else if let Some(server) = self.logs {
            Some(Admin::Logs {
                server,
                lines: self.lines.into(),
            })
        } else if let Some(server) = self.inspect {
            Some(Admin::Inspect(server))
        } else if let Some(server) = self.shell {
            Some(Admin::Shell(server))
        } else if let Some(server) = self.rename {
            Some(Admin::Rename {
                server,
                name: self.name.clone().unwrap_or_default(),
            })
        } else {
            self.stop.map(|server| Admin::Stop {
                server,
                confirmed: self.yes,
            })
        };
        if let Some(action) = action {
            return Ok(Operation::Admin {
                host: self.ssh_address.unwrap(),
                action,
            });
        }
        if self.detach {
            return Ok(Operation::Ensure {
                host: self.ssh_address.unwrap(),
                direct: self.direct,
                name: self.name,
                json: self.json,
            });
        }
        let target = if let Some(id) = self.resume.filter(|s| !s.is_empty()) {
            Some(Target::Resume(id))
        } else if self.reconnect {
            Some(Target::Reconnect)
        } else if self.last {
            Some(Target::Latest(self.path))
        } else {
            self.path.map(Target::New)
        };
        Ok(match target {
            Some(target) => Operation::Open {
                host: self.ssh_address.unwrap(),
                direct: self.direct,
                target,
            },
            None => Operation::Pick {
                host: self.ssh_address,
                direct: self.direct,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Operation {
        Args::try_parse_from(std::iter::once("rcodex").chain(args.iter().copied()))
            .unwrap()
            .operation()
            .unwrap()
    }

    #[test]
    fn conversation_intent_is_resolved_before_execution() {
        assert!(matches!(parse(&[]), Operation::Pick { host: None, .. }));
        assert!(matches!(
            parse(&["dev", "--resume"]),
            Operation::Pick { host: Some(_), .. }
        ));
        assert!(matches!(parse(&["dev", "--resume", "exact"]),
            Operation::Open { target: Target::Resume(id), .. } if id == "exact"));
        assert!(matches!(parse(&["dev", "/alpha", "--last"]),
            Operation::Open { target: Target::Latest(Some(path)), .. } if path == "/alpha"));
        assert!(matches!(parse(&["dev", "/alpha", "--direct"]),
            Operation::Open { direct: true, target: Target::New(path), .. } if path == "/alpha"));
        assert!(matches!(
            parse(&["dev", "~", "--detach", "--json"]),
            Operation::Ensure { json: true, .. }
        ));
        assert!(matches!(
            parse(&["dev", "--stop", "abcd", "--yes"]),
            Operation::Admin {
                action: Admin::Stop {
                    confirmed: true,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn invalid_combinations_never_reach_network_code() {
        for args in [
            vec!["--list"],
            vec!["dev", "--list", "--direct"],
            vec!["dev", "--json"],
            vec!["dev", "--attach", "old-server"],
            vec!["dev", "--name", "orphan"],
            vec!["dev", "--stop", "abcd", "--sessions"],
        ] {
            let parsed = Args::try_parse_from(std::iter::once("rcodex").chain(args));
            assert!(
                parsed
                    .map_err(anyhow::Error::from)
                    .and_then(Args::operation)
                    .is_err()
            );
        }
    }
}
