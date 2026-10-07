# Dev info

## how work?
the engine runs at x location, and looks for the following types of modules to connect, aswell as their general flow:
- adapters ->  pre-process -> in-process -> and post-process -> adapters

## module overview

### the connection standard
> every module needs to do this

Every module is a process that connects to the engine over **WSS** (TLS; the
engine trusts only its self-signed cert, auto-pinned when `COCKATIEL_TLS_CERT`
points at `cockatiel_engine-rs/tls/cockatiel-cert.pem`, otherwise plain `ws://`).
On connect it sends a `ConnectionRequest` with a PIN (single-use, printed by the
engine on boot; `config.json` holds `engine_ip`/`engine_port`/`pin`); the engine
replies with a JWT `auth_token` + `module_instance_uuid7`. From then on every
`Container` carries `auth_token` + `module_name` + `module_instance_uuid7`.

Lifecycle requirements:
- Answer `AuthVerify` probes or the watchdog severs you (the one-file clients
  auto-answer; direct socket readers must answer manually).
- Re-register your `CommandsPayload` on every fresh connection — the engine
  forgets a session when the socket drops.
- Reconnect with backoff when the engine drops the socket (`reconnect_base_secs`
  doubling to `reconnect_max_secs`); a module that exits on disconnect leaves the
  watchdog to sever it.
- Your manifest (`cockatiel_module_info.json`) declares the pipeline stage
  (`capabilities`), launch/build commands, binary routes, per-module gates
  (`authority`/`min_rank`/`price`), and a `credentials` array describing the
  settings the TUI edits. `min_rank` is a 0-1 number (ranks are 0-1 floats;
  tier names are a display concern from the root `rank_chart.json`). The
  engine trusts the manifest `capabilities` over the requested
  `process_position` at registration time.

### adapters
the adapters are expected to be bi-directional, send and receive. 
> if you do not want your adapter to send messages, simply except the prompt and scilently drop it. cockatiel does not validate sent messages are send, only requests the module to do so.

Adapters declare `capabilities: "input"`, so they sit in the `inputs` stage, not
pre-process. They feed raw chat into the engine as `MessagePreProcess`, and can
send to a platform via `SendToPlatforms` (optionally targeting one
`channel_id`). They also push `ChannelStats` so modules can query live viewer
counts (`channel_viewers`).

### pre-process
pre-process modules are run **concurrently** on every message before the
sequential in-process chain. They can rewrite the message, hold it for audit, or
pass it through. They ack with `MessagePreProcess` (content preserved unless they
changed it). Command-owning pre-process modules (e.g. `clip`, `events`,
`fake-input`) only receive messages routed to their commands plus catch-alls.
Pre-process order is alphabetical; content-modified modules (banned-words,
score-messages) run as `inprocess` to guarantee ordering.

### in-process
in-process modules run **sequentially** in config order, each seeing the
`processed_message` produced by the previous one. They ack with
`MessageInProcess`, carrying `processed_message` (what the message became) and
`abandon_message` (drop it). The chain is ordered: `language-constrainer` first
(holds out-of-language messages for audit), then `banned-words` (censorship on
whatever the language gate let through), then the rest in config order.

### post-process
post-process modules run **concurrently** after the chain, on the finished
message, and can return anything (they are display/archival consumers —
`term-chat`, `audit-viewer`, `tts-rs`). They ack with `MessagePostProcess`.


## One-file client imports

**Any program can be a module** — the only requirement is that it can connect to the engine over **WSS** (WebSocket over TLS). The engine accepts *only* `wss://` connections, so a client must speak TLS and trust the engine's self-signed certificate. The one-file clients below do this automatically: when `COCKATIEL_TLS_CERT` points at the engine's cert (`cockatiel_engine-rs/tls/cockatiel-cert.pem`, generated on first boot) they connect via `wss://` and pin that cert; with the variable unset they fall back to plain `ws://` (useful only against a non-TLS engine).

The one-file clients live in the [`cockatiel_lib`](https://github.com/vulbyte/cockatiel_lib) submodule; each implements the same contract (single-connection PIN→JWT auth, full 23-field `Container` codec, auto `AuthVerify` liveness answer, WSS + cert pinning):

| Language | One-line import | Client file |
|---|---|---|
| JavaScript | `import { connectToEngine } from 'cockatiel-lib-js';` | `javascript/lib-cockatiel.mjs` |
| Python | `from lib_cockatiel import CockatielClient` | `vulbyte/cockatiel_client-py` |
| Rust | `use cockatiel_client::CockatielClient;` | `vulbyte/cockatiel_client-rs` |
| C# | `using Cockatiel;` + `PackageReference` | `dotnet/Cockatiel.cs` |
| C | `#include <cockatiel_lib.h>` + 1 cmake link line | `c/cockatiel_lib.h` |
| C++ | `#include <cockatiel_lib.hpp>` | `cpp11/cockatiel_lib.hpp` |
| gdScript | `preload("res://cockatiel_lib.gd")` | `gdscript/cockatiel_lib.gd` |
| Odin | `import ck "cockatiel_lib"` | `odin/cockatiel_lib/cockatiel_lib.odin` |
| Java | `import cockatiel.Cockatiel;` | `java/Cockatiel.java` |
| Lua | `local lib = require("cockatiel_lib")` | `lua/cockatiel_lib.lua` |

See `cockatiel_lib/CLIENT_CONTRACT.md` for the exact wire behavior every client implements.

## Chat commands
The engine parses commands out of raw messages, sorts them, and routes them to
the module that owns them. A module subscribes by sending a `Commands` payload
(`commands_payload`) listing its commands; an **empty** list makes it a
catch-all (it receives everything). `alert_on_unknown_command` opts it into the
apology reply when a user tries an unregistered command under one of its flags.

Syntax: `<command_flag><command> <flag:value(optional)> <args>` — e.g.
`!reprimand @user saying offensive things` or `!tts -p 2 -r 1.4 -v 88 hey!`.
Flag names ship without the `-` (the engine strips it); both `-p 2` and `-p:2`
are accepted, bare `-d` is a boolean. Flag values are validated against the
owner's registered `Flag` definitions (`ANY`/`OPTIONS`/`RANGE`).

Flow: does the message start with a registered flag? → is the command known?
→ route it **only** to the owning module + catch-alls (skips the rest of the
preprocess fanout). The parsed `Command` (with flag values) rides on
`ChatMessage.command` for downstream modules. `!help` lists every registered
command. What a module returns depends on its position: pre-process can return
anything, in-process expects a message, post-process can return anything.

Note: the engine's `!help` and invalid-command replies target the **source
channel** (the message's `channel_id` flows to `SendToPlatforms`; multi-channel
adapters route it, single-channel adapters are unaffected).

## Module manifest reference
Each module in `modules/` (or anywhere the TUI searches) declares a `cockatiel_module_info.json` telling Cockatiel how to launch it, what it does, and where it plugs into the pipeline. It is parsed with **strict JSON** (`serde_json`) — no comments. Modules that connect remotely, or are managed by another application (e.g. in-game communication), don't need a manifest.

Fields:

- `name` — **required**, no spaces, lowercased for standardization. How the engine knows and displays the module.
- `description` — human-readable summary; shown in the TUI and at the top of the credential form.
- `version` — free-form string, no formatting enforced.
- `capabilities` — where the module sits in the pipeline, one of:
  - `input` — provides chats that need processing (the platform adapters).
  - `preprocess` — runs before in-flight handling, mainly archival and moderation (e.g. banned-words, language-constrainer).
  - `inprocess` — modifies the chat in flight: censoring, swapping words, telling the engine to drop a message via a flag, etc.
  - `postprocess` — runs after the message is processed: chatbots, TTS, chat displays.
  - `output` — a display/consumer module; treated as `postprocess` in the pipeline ordering.
- `root_file` — the file the launcher needs to run.
- `launch_command` — the command to run it (`cargo run`, `python3`, `node`, …). Empty string for a prebuilt compiled binary.
- `command_flags` — extra flags appended to the launch command.
- `autostart` — whether Cockatiel should consider the module safe to launch automatically once configured. (Note: the TUI currently starts all modules disabled; this flag is honored for registry/ordering and crash-loop handling.)
- `terminal` — launch the module in its own terminal window (term-chat, audit-viewer).
- `credentials` — declared secrets/settings the operator fills in via the TUI credential prompts. Each entry has `key`, `label`, `sensitive` (masked on screen), `list`, `optional`, and `env` (the environment-variable name it maps to). Sensitive fields are stored in the module's `.env`; non-sensitive fields go in `config.json`.
- `binary` — prebuilt binary routes per OS → CPU architecture (e.g. `macos`/`aarch64`). When present and the file exists, the supervisor runs it directly instead of compiling. A legacy `os → path` form is also accepted.
- `build_command` / `build_flags` — how to build the module when no (fresh) binary exists. Falls back to `cargo build --release`.
- `unresponsive_timeout_secs` / `probe_response_secs` — optional per-module overrides for the engine's dead-air liveness probing.

**Config convention:** module settings live in `config.json`'s `module_specific`
(non-secret) or the module's `.env` (secrets, owner-only), and a module should
**create the value with its default when it's missing** — so every setting always
exists and is editable in place.

Example (strict JSON):

```json
{
  "name": "example-module",
  "description": "An example Cockatiel module.",
  "version": "0.1.0",
  "capabilities": "postprocess",
  "root_file": "./main.rs",
  "launch_command": "cargo run",
  "command_flags": ["--release"],
  "autostart": true,
  "terminal": false,
  "credentials": [
    {
      "key": "api_key",
      "label": "API Key",
      "sensitive": true,
      "list": false,
      "optional": false,
      "env": "EXAMPLE_API_KEY"
    }
  ],
  "binary": {
    "macos": {
      "aarch64": "target/release/example_module",
      "x86_64": "target/release/example_module",
      "arm": "target/release/example_module"
    },
    "linux": {
      "aarch64": "target/release/example_module",
      "x86_64": "target/release/example_module",
      "arm": "target/release/example_module"
    },
    "windows": {
      "aarch64": "target/release/example_module.exe",
      "x86_64": "target/release/example_module.exe",
      "arm": "target/release/example_module.exe"
    }
  },
  "build_command": "cargo",
  "build_flags": ["build", "--release"]
}
```

## Architecture

**Repos.** Every component is its own repository and builds **standalone** or runs together over the WS/protobuf protocol: the protocol lives in `vulbyte/cockatiel_proto` (a Rust crate), the client SDKs in `vulbyte/cockatiel_client-rs` / `-py`, and each module in its own `vulbyte/cockatiel_module-*` repo. Consumers pin the client/proto by **exact commit SHA** (`rev =`), so the wire format a module is built against never shifts underneath it. The engine, TUI, user-database, and test-runner live in their own repos too; this monorepo tracks everything as submodules.

**Pipeline.** An adapter sends a message into the engine, which inserts it into the timeline database as `queued`, then runs the 5-state chain: `queued` → pre-process fanout (concurrent) → in-process chain (sequential) → post-process fanout (concurrent) → marked `complete`. Every stage is acked; the timeline table *is* the queue, so a restart just re-queues anything still in flight. Messages flagged for audit are held (`audit` status) until a moderator approves (→ `complete`) or rejects (→ `failed`, row kept). Errors land in the timeline with `failed` status for later review. Displays (term-chat, audit-viewer) connect as post-process/output consumers and render the finished messages.

**User database.** `cockatiel_user_database-rs` (now a submodule of the engine at `cockatiel_engine-rs/modules/cockatiel_user_database-rs`) is a separate WebSocket service holding users, scores, roles, and per-user key/values. The engine is its only privileged client; mod commands (`commend`, `reprimand`, `ban`, `timeout`) map onto it. Platform roles (owner/mod/sponsor) are verified on login and merged with the user-db tier. User ranks are 0-1 floats computed server-side by the user-db (see `compute_rank`); the tier NAME displayed for a rank comes from the repo-root `rank_chart.json`, shared by the engine, the TUI and term-chat.

**TUI as supervisor.** `modules/cockatiel_module-tui_v2-rs` (the v2 TUI — a Blender-inspired
BSP layout engine) is the operator's control surface. It launches the engine +
user database, discovers and registers modules, starts/stops/rebuilds them
(prebuilt binary → rebuild → crash-recovery ladder), edits their
`.env`/`config.json` in place, answers approval/credential prompts, and streams
live logs. The v1 TUI is retired and archived under `legacy/cockatiel_tui-rs/`.

**Security model.**

- **PIN + JWT name-trust.** A first connection proves the PIN; thereafter every message carries a JWT whose `name` claim must match the container's `module_name` — a valid token can't be replayed under a trusted name (like the TUI) to reach gated capabilities. The PIN remains the master key for first connections. Blank/`unnamed_module` identities are rejected outright, and the supervisor forces each module's identity via `--name`.
- **WSS-only transport + cert pinning.** The engine generates a self-signed certificate on first boot (`tls/cockatiel-cert.pem`) and accepts **only** `wss://` — a plain `ws://` connection fails the TLS handshake and is dropped before any frame. Clients pin that exact cert (via `COCKATIEL_TLS_CERT`, which the supervisor sets on every module it launches); it is never trusted implicitly, so a man-in-the-middle with a different cert is rejected.
- **Module approval via prompts.** Connecting modules are not silently trusted: the engine raises a `Prompt`, the TUI (or term-chat) shows an approve/deny dialog, and the decision persists. Modules can also raise prompts (e.g. "enter the YouTube video ID"), answered in the TUI's prompts window.
- **Secrets vs settings.** Secrets (engine PIN/JWT secret, module tokens, OAuth keys) live in owner-only `.env` files; non-secret settings live in `config.json`. Sensitive credential fields are masked on screen. The PIN is delivered to modules via `COCKATIEL_PIN` env — never on the command line.
- **Credential isolation.** `module_list` redacts other modules' secrets (only the TUI and term-chat's OAuth-login may read them); `set_credentials` and `engine_info` (PIN) are control-surface-only — one module can't read or rewrite another's credentials.
- **Read-only data boundary.** Modules can only run `SELECT`/`WITH`/`EXPLAIN` queries against the timeline; writes go through gated engine queries. The timeline and user-db write periodic snapshots to backup paths, and the TUI warns loudly if no backup is configured.
