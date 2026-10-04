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

## Requirements and transport

- Currently **Linux on both ends, with matching CPU architecture**. The supplied build is x86-64. The helper checks architecture before uploading.
- OpenSSH locally; SSH access, `nohup`, and a recent `codex` on the remote noninteractive shell's `PATH`.
- Local Codex must support `--remote`; direct mode additionally requires app-server capability-token authentication. Tested with Codex **0.160.0**. Sign in to Codex on the remote host before model use.
- No Rust toolchain, Python, Node, Go, or shared libraries are required to run the static build remotely.

Normal mode binds the server to `127.0.0.1` and forwards a local loopback port over SSH. Existing SSH aliases, keys, agents, and ProxyJump configuration work through the system `ssh`. One SSH control connection is reused for setup, management, and forwarding. A small detached watcher removes that connection after the local Codex process exits, including abnormal exits.

`--direct` binds the remote server to **0.0.0.0**, authenticates with a random bearer token, and connects to the SSH-configured hostname. **WebSocket traffic and the token are unencrypted. Use a trusted network such as Tailscale, or use the default SSH tunnel.** You must make the selected remote port reachable yourself; rcodex does not change firewalls. A loopback-only server cannot be reconnected to with `--direct`; launch a new direct server instead. A direct server can also be accessed through the default tunnel.

## Remote state

```text
~/.cache/rcodex/<binary-hash>       cached helper (uploaded once per build)
~/.local/state/rcodex/conns/       private JSON records and direct-mode tokens
~/.local/state/rcodex/logs/        one log per app-server
```

Servers run under `nohup` in a new session with stdin disconnected. Records include the process start time and boot ID, so stale records do not identify unrelated processes after PID reuse or reboot. Inactive records are omitted from the picker; stopped records and tokens are removed, while logs are retained.

No resident rcodex daemon is required. Servers survive SSH disconnects, but are
not automatically restarted after a crash or reboot.

Set `RCODEX_STATE_DIR` in the **remote noninteractive shell environment** to use another absolute state path, e.g. `/var/lib/rcodex`. It must be writable by the SSH user. The default needs no root access.

## Build and verify

On x86-64 Linux with Nix flakes enabled:

```sh
nix run github:wokalski/rcodex -- user@host
# Or install it:
nix profile add github:wokalski/rcodex
```

The Nix package builds a static binary; Codex and OpenSSH must already be on
your PATH. To build from a checkout, run `nix build` and use `result/bin/rcodex`.

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

For a native development build, use `cargo build`. A dynamically linked build is only portable to remote hosts with the matching loader and libraries; use the musl release for distribution.
