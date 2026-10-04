# rcodex

A small Rust launcher for persistent remote Codex workspaces. Built with **clap**, **Ratatui**, **Crossterm**, and **tui-input**.

```sh
rcodex devbox ~/projects/my-app    # start in an existing remote directory
rcodex devbox                     # pick a running server or launch a new project
rcodex user@host /srv/app --direct # authenticated WebSocket, no SSH tunnel
rcodex ssh://user@host:2222
```

Quote `~/...` if you want the remote user's home rather than your shell's local expansion:

```sh
rcodex devbox '~/projects/my-app'
```

## Controls

| Key | Action |
| --- | --- |
| ↑ / ↓, j / k | Select a workspace |
| Enter | Connect, or launch from the new-project entry |
| n | Browse remote folders for a new project |
| Shift+X / Shift+Q | Stop selected server, with confirmation |
| r | Refresh active servers |
| q / Esc | Exit or cancel |
| Ctrl+C | Exit |

The folder browser starts in the remote user's home directory. Use **↑/↓** to select,
**Enter/→** to open a folder, **←** for its parent, and **Home** to return home.
Type to filter folder names; Backspace edits the filter. **Space launches in the
current directory** and Esc returns to the server list. Directory symlinks are
followed; hidden folders appear after ordinary folders.

The directory must already exist. Each launch starts a separate server; reconnecting through the picker reuses that server. Closing Codex leaves the server running. Stopping a server disconnects **all** its clients.

## Resume a conversation

```sh
rcodex devbox --resume             # select a server, then a remote conversation
rcodex devbox --resume SESSION_ID  # select a server, then resume this conversation
rcodex devbox --last               # select a server, then resume its project's latest conversation
rcodex devbox --direct --resume    # also works with direct WebSockets
```

Choose the existing server from the workspace list to rejoin a live conversation.
Resume uses Codex's remote session picker and remote history, not your laptop's
session files. A conversation active in another app-server may be read-only:
connect to its owning server to continue it. Supplying a project path or choosing
"Launch a new project" starts a new server; use that for saved conversations whose
old server is no longer running. Codex's `--last` starts a fresh conversation if
there is no matching history in the selected project.

## Requirements and transport

- Clients: **macOS (Apple Silicon or Intel)** and **x86-64 Linux**. Remote servers: **x86-64 Linux**. The macOS binary embeds a static Linux helper; no remote build tools or Rosetta are needed. Other remote architectures are not yet supported by the macOS client.
- OpenSSH locally; SSH access, `nohup`, and a recent `codex` on the remote noninteractive shell's `PATH`.
- Local Codex must support `--remote`; direct mode additionally requires app-server capability-token authentication. Tested with Codex **0.160.0**. Before opening the picker or starting a server, rcodex checks remote login and runs `codex login --device-auth` over SSH if needed. Open the displayed URL in your browser and approve the device code. Failed or cancelled login stops the launch. Device-code login must be enabled in your ChatGPT account settings.
- No Rust toolchain, Python, Node, Go, or shared libraries are required to run the static build remotely.

Normal mode binds the server to `127.0.0.1` and forwards a local loopback port over SSH. Existing SSH aliases, keys, agents, and ProxyJump configuration work through the system `ssh`. One SSH control connection is reused for setup, management, and forwarding. A small detached watcher removes that connection after the local Codex process exits, including abnormal exits.

`--direct` uses **encrypted `wss://`** with a random bearer token. A small per-server
TLS relay binds to **0.0.0.0**, while Codex itself listens on loopback. The server
generates a private certificate; rcodex retrieves its public certificate over SSH,
verifies the direct endpoint, and supplies a temporary CA bundle to the local
Codex process. No system trust store, reverse proxy, or Tailscale configuration is
changed. Native/configured CA roots are retained in that bundle.

The SSH-configured hostname and selected remote port must be reachable; rcodex
does not change firewalls. Failed reachability checks leave the server running
and explain how to reconnect through SSH. A loopback-only server cannot be used
with `--direct`; launch a new direct server instead. Direct TLS servers can also
be accessed through the default SSH tunnel.

Servers created by older rcodex builds used plaintext direct WebSockets, which
Codex rejects with a bearer token. Reconnect to those without `--direct` to keep
working, or launch a new server for TLS. Running servers are never automatically
restarted during an upgrade.

## Remote state

```text
~/.cache/rcodex/<binary-hash>       cached helper (uploaded once per build)
~/.local/state/rcodex/conns/       private records, tokens, TLS certificates and keys
~/.local/state/rcodex/logs/        one log per app-server
```

Servers run under `nohup` in a new session with stdin disconnected. Records include the process start time and boot ID, so stale records do not identify unrelated processes after PID reuse or reboot. Inactive records are omitted from the picker; stopped records and tokens are removed, while logs are retained.

No shared rcodex daemon is required; direct mode runs one TLS relay per server.
Servers survive SSH disconnects, but are
not automatically restarted after a crash or reboot.

Set `RCODEX_STATE_DIR` in the **remote noninteractive shell environment** to use another absolute state path, e.g. `/var/lib/rcodex`. It must be writable by the SSH user. The default needs no root access.

## Build and verify

On macOS or x86-64 Linux with Nix flakes enabled:

```sh
nix run github:wokalski/rcodex -- user@host
# Or install it:
nix profile add github:wokalski/rcodex
```

The Nix package builds a native macOS client or static Linux binary; Codex and
OpenSSH must already be on your PATH. To build from a checkout, run `nix build`
and use `result/bin/rcodex`.

With Rust and a musl C toolchain (e.g. `musl-tools` on Debian/Ubuntu):

```sh
git clone https://github.com/wokalski/rcodex.git
cd rcodex
rustup target add x86_64-unknown-linux-musl
cargo build --locked --release --target x86_64-unknown-linux-musl
install -Dm755 target/x86_64-unknown-linux-musl/release/rcodex ~/.local/bin/rcodex

cargo test --locked
cargo clippy --all-targets -- -D warnings
# Optional integration test: starts/stops real Codex servers, makes no model calls.
cargo test --test remote_lifecycle -- --ignored
```

For a native development build, use `cargo build`. On macOS this embeds the
checked-in Linux helper. On Linux a dynamically linked build is only portable
to remote hosts with the matching loader and libraries; use musl for distribution.
