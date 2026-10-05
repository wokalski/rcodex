# rcodex

A Rust launcher for remote Codex conversations, built with **clap**, **Ratatui**,
**Crossterm**, and **tui-input**. One persistent app-server per SSH host/user;
each conversation has its own project directory.

```sh
rcodex                                  # cached conversations across remembered hosts
rcodex devbox                           # conversations on one host
rcodex devbox '~/projects/my-app'        # new conversation, shared host server
rcodex devbox --resume CONVERSATION_ID   # resume that exact remote conversation
rcodex devbox --reconnect                # most recently visited locally on this host
rcodex devbox --last                     # most recently updated remote conversation
rcodex devbox /srv/app --last            # latest conversation in this directory
rcodex user@host /srv/app --direct       # direct authenticated TLS
rcodex ssh://user@host:2222
```

Quote `~/...` to avoid your local shell expanding the remote path.

## Share conversations with the Codex app over SSH

Connect the Codex app to the remote host once, then use the same SSH user in
`rcodex HOST`. rcodex automatically prefers Codex's native app-server daemon:
both clients attach to the **same live server**, not just the same history files.
You can resume terminal-created conversations from the app, and app-created
conversations from rcodex, including threads still loaded in memory.

Both clients must use the same remote `CODEX_HOME` (normally `~/.codex`). rcodex
reads `codex app-server daemon version` and forwards its Unix socket to a private
local socket over SSH. The local Codex CLI attaches with `--remote unix://…`.
OpenSSH must allow Unix-socket forwarding (`AllowStreamLocalForwarding`).
This integration is tested against Codex 0.160.0's public daemon/proxy contract;
the proprietary desktop app itself is not part of the automated tests.

Background discovery never starts the daemon. A foreground connection can start
an already provisioned daemon with `codex app-server daemon start`. rcodex never
bootstraps, installs, updates, restarts, or stops it, and does not enable cloud
remote control. Codex retains its own lifecycle and updater settings. If the
daemon fails, rcodex reports the failure instead of creating a separate server.

**Omit `--direct` when sharing with the app.** The shared daemon uses SSH; rcodex
rejects direct mode rather than opening a different server. Stop/rename commands
also refuse to change the app's shared daemon. Manage it using Codex on the host.

Hosts without a provisioned Codex daemon keep rcodex's standalone-server behavior
below. Set up the app's SSH connection first for shared live work; rcodex does not
move already-running work from a standalone server into the app daemon.

## Conversations, not server bookmarks

Bare `rcodex` renders its local cache before starting SSH discovery. It then
queries remembered hosts in the background, at most four at a time, without
password or host-key prompts. Navigation and folder browsing remain responsive.
Rows show conversation title, directory, host, runtime status, and local visit
time. Locally visited conversations come first, newest visit first; conversations
discovered for the first time follow, ordered by remote activity.

Discovery uses Codex's paginated `thread/list` without a directory filter, plus
`thread/loaded/list` for live conversations. It includes normal CLI, editor,
exec, and app-server conversations; archived conversations and internal subagents
are not part of the default list. It never submits a model request. A refresh
does not count as a visit. Selection stays on the same conversation when rows
change. Failed or incomplete refreshes retain cached entries.

**Enter resumes the exact selected conversation ID**, not whichever conversation
happens to be newest in its directory. The CLI passes that conversation's `cwd`
to local Codex. New conversations are created with `thread/start` and their own
`cwd`, then opened with `codex resume ID --remote … --cd …`.

If a host has no running registered server, discovery shows that state without
starting one. Press **h**, then **Enter**, to connect and start its host
server, or **n** to browse for a new project. SSH and Codex authentication happen
in the foreground. Cached conversations can still be selected while offline;
connection errors are reported rather than silently starting another conversation.

| Key | Action |
| --- | --- |
| ↑ / ↓, j / k | Select a conversation |
| Enter | Resume selected conversation |
| / | Search titles, hosts, directories, and IDs |
| n | Browse remote folders for a new conversation |
| h | Connect a host; Tab cycles remembered hosts |
| t | Open an SSH shell in the selected conversation's directory |
| r | Retry background discovery |
| q / Esc / Ctrl+C | Exit or cancel |

In the folder browser, **Enter/→** opens a folder, **←** opens its parent, typing
filters names, **Space** starts a conversation in the current directory, and
**Ctrl+N** creates a folder. Directory symlinks are followed; existing files are
never overwritten. Esc cancels, including while a directory request is in flight.

History lives in `$XDG_STATE_HOME/rcodex/history.json`, defaulting to
`~/.local/state/rcodex/history.json`. Writes are private, locked and atomic.
Only public conversation metadata, host names, and local visit timestamps are
cached—never bearer tokens or certificates. `RCODEX_HISTORY_FILE` accepts an
absolute override. Conversations opened directly inside Codex are discovered,
but rcodex cannot reconstruct their earlier *local visit* timestamps.

## Server lifecycle

Launching another project reuses the same host server. Its process working
directory is not a project boundary: individual threads carry their own `cwd`.
Codex's sandbox and OS permissions, not server process cwd, control file access.

For standalone servers, each host has one server record. Creation is serialized
under the remote registry lock, so simultaneous clients reuse that server. Every
conversation uses the same endpoint; its ID and directory determine which project
Codex opens. There is no
automatic migration or adoption of servers from earlier rcodex versions.

Standalone servers use `nohup`, a detached process session, disconnected stdin, and
private logs. **No systemd integration yet.** They survive SSH/client disconnects,
but do not restart after a crash or reboot. Codex stores persisted conversation
history on the remote host. rcodex materializes a new thread's empty history
before handing it to Codex, without submitting a turn. On Codex 0.160 this uses
an archive/unarchive cycle on the newly allocated empty thread only; existing
conversations are never archived by rcodex. Local visits are kept even when Codex
omits an empty conversation from its default list. rcodex does not impose a work
timeout; Codex controls its own disconnected-client and approval behavior.

Server administration remains separate from conversation selection:

```sh
rcodex devbox --sessions --json          # public conversation metadata
rcodex devbox '~' --detach              # ensure host server, no new conversation
rcodex devbox --list --json             # registered running servers
rcodex devbox --logs SERVER_ID --lines 100
rcodex devbox --inspect SERVER_ID        # server details + original cwd's Git status
rcodex devbox --shell SERVER_ID          # shell in original server cwd
rcodex devbox --rename SERVER_ID --name host-server
rcodex devbox --stop SERVER_ID           # warns and asks for confirmation
rcodex devbox --stop SERVER_ID --yes     # explicit script confirmation
```

**Stopping a shared server interrupts every conversation running on it.** There
is deliberately no kill-server shortcut on a conversation row. Stop does not
delete persisted Codex history. Logs remain readable after stopping when using
the full server ID. Server selectors accept a full ID, a unique ID prefix of at
least four characters, a server name, or its process directory. Use `--resume ID`
for conversations.

## Requirements and transport

- Existing **local and remote Codex installations**. rcodex never bundles or
  installs Codex. Tested with Codex **0.160.0**, including `--remote` and the v2
  app-server API.
- OpenSSH locally; SSH access, `nohup`, and `codex` on the remote noninteractive
  shell's PATH. Unauthenticated foreground launches run `codex login --device-auth`
  before server setup. Failed/cancelled login stops the launch.
- Clients: x86-64 Linux, Apple Silicon macOS, Intel macOS. Remote: x86-64 Linux.
  macOS embeds a static Linux helper; no remote Rust, Python, Node, Go, or shared
  libraries are required by the distributed helper.

Standalone transport binds Codex to loopback and forwards a local port through SSH.
The app daemon instead uses Unix-socket forwarding, with no TCP listener.
SSH aliases, keys, agents, and ProxyJump use the system `ssh`. A detached watcher
cleans up each control connection when its client exits, including abnormal exit.
Background probes are cancellable and have a 30-second overall deadline.

`--direct` uses encrypted **wss://**, a random bearer token, and a small TLS relay.
Its public certificate is retrieved over SSH and verified before local Codex
connects. No system trust store or firewall is changed. The SSH-resolved hostname
and remote port must be reachable. Direct servers can also use the SSH tunnel.

An already running SSH-only server cannot be used directly. Reconnect without
`--direct`, or explicitly stop the server before restarting with direct TLS.
rcodex never interrupts live work to change transport.

```text
~/.cache/rcodex/<binary-hash>       uploaded helper, cached per build
~/.local/state/rcodex/server.json  private host-server record
~/.local/state/rcodex/conns/       tokens, certificates and keys
~/.local/state/rcodex/logs/        one log per app-server
```

Process start time and boot ID protect against stale PIDs. Set `RCODEX_STATE_DIR`
in the remote noninteractive environment for another writable absolute state
directory. The default needs no root access.

## Code organization

- `cli` converts flags into a single operation; `app` coordinates foreground flow.
- `host` owns authenticated access, host-server setup, conversation selection,
  and the exact-ID handoff to local Codex. `admin` handles explicit server commands.
- `ui/model` handles input and returns effects without I/O. `ui/view` renders it.
  `ui` owns the terminal and cancellable jobs; `backend` executes background SSH
  operations in isolated helper processes.
- `history` owns a local store path, locked transactions, and visit ordering.
  Its serialized cache contains only public metadata.
- `remote` owns the process registry and detached-server lifecycle. Its registry
  lock covers creation, updates, and stopping. `rpc` queries Codex's
  conversation API; `ssh` and `tls` own transport.
- `daemon` discovers and attaches to Codex's shared app daemon without taking
  ownership of its lifecycle or creating an rcodex server record.
- `helper` dispatches private process entry points, independently of user CLI
  parsing.

State-transition tests cover picker cancellation, refresh selection, search,
folder navigation, and paste. Storage tests use explicit temporary paths rather
than changing process-wide environment variables.

## Build and verify

CI builds Linux x86-64, Apple Silicon macOS, and Intel macOS separately and
publishes to the public `rcodex` Cachix cache. The flake declares its URL and
signing key, so Nix can download the matching binary and runtime dependencies
instead of compiling Rust locally.

Maintainer configuration:

1. Set the GitHub repository variable `CACHIX_CACHE` to `rcodex` and the repository
   secret `CACHIX_AUTH_TOKEN` to a write token scoped to that cache. Enter the token
   in GitHub's secrets UI, not in source code or an issue/comment.
2. Push the intended revision to `main` and wait for all three Build jobs. Only
   successful builds on pushes to `main` publish their runtime closure. Pull
   requests may read the public cache but receive no publishing token.

After the matching CI build finishes, accept the flake's cache configuration:

```sh
# --max-jobs 0 forbids a local build: a cache miss fails instead of compiling.
nix run --refresh --accept-flake-config --max-jobs 0 github:wokalski/rcodex -- user@host
```

Codex must still be installed locally and remotely. For app interoperability,
connect the Codex app to the same SSH host/user first and omit `--direct`. Create
a conversation in one client and resume its exact ID from the other. Both must
use the same remote `CODEX_HOME`.

Without a configured/populated cache, or when building a local checkout:

```sh
nix run github:wokalski/rcodex -- user@host
nix profile add github:wokalski/rcodex
# From this checkout:
nix build
result/bin/rcodex
```

Nix builds a native macOS client or static Linux binary. Codex and OpenSSH must
already be on PATH. With Rust and a musl C toolchain:

```sh
cargo test --locked
cargo clippy --all-targets -- -D warnings
rustup target add x86_64-unknown-linux-musl
cargo build --locked --release --target x86_64-unknown-linux-musl
# Real Codex lifecycle, TLS, multi-directory reuse and concurrent creation;
# uses disposable state and makes no model calls:
cargo test --test remote_lifecycle -- --ignored
# Shared daemon + official proxy interoperability (requires a running Codex
# daemon to locate its installed package; all writes use a disposable home):
cargo test --test daemon -- --ignored
```

For development use `cargo build`. A dynamically linked Linux build requires a
compatible remote loader and libraries. macOS builds embed `bin/rcodex-linux-x86_64`;
regenerate it from the final static Nix output whenever helper code changes.
