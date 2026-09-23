# Cockatiel

Chat automation and moderation engine for streamers.

Cockatiel is a platform-agnostic chat engine. Platform adapters (Twitch, Kick, YouTube, Discord) feed every message into a 5-state engine pipeline — queued → pre-process → in-process → post-process → complete — where your modules censor, score, constrain, translate, synthesize speech, or archive whatever they want. All messages live in a timeline database that doubles as the queue, so nothing is lost and everything can be audited. You supervise the whole thing from a terminal TUI, and watch the chat in term-chat.

## Why make this?

There isn't a good open standard for chat interactions, and the tools that exist are painful to use or extend — so instead of stitching disconnected services together, this project is one cohesive engine. Most alternatives favor a specific platform, so Cockatiel is deliberately platform-agnostic: anyone on any platform can stream and give viewers the best experience possible. The engine is built on queues and signals (protobuf messages over WebSocket), which is far easier to extend and customize than the alternatives.

The general loop:

- init and verify the config to make sure there are no issues
- check/listen for an update
- add the data to an unprocessed queue (capture as fast as possible, allow deferred processing)
- process the message into Cockatiel's platform-neutral format
- add messages to the message queue on success

If there is an error at any point, the error goes to an errored queue that can be reviewed later.

## Quickstart

Requirements: Rust toolchain. The `tts-service` module also needs Python 3.

```sh
cd cockatiel_tui-rs
./run.sh          # = cargo run --release
```

The TUI is the supervisor: it launches the engine and the user-database service, discovers every module that ships a `cockatiel_module_info.json` (recursively from the `modules/` folder), and registers them in the pipeline. Modules start **disabled** by default — start them from the modules window (`s`). The engine itself is a passive router: it owns no processes, it only orders the pipeline and routes messages between connected modules.

## One-file client imports

Any program can be a module — adapters, chatbots, TTS, in-game communication. The one-file clients live in the [`cockatiel_lib`](https://github.com/vulbyte/cockatiel_lib) submodule; each implements the same contract (single-connection PIN→JWT auth, full 23-field `Container` codec, auto `AuthVerify` liveness answer):

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

Note: the engine's `!help` and invalid-command replies go out through
`SendToPlatforms`, which today broadcasts to **all** channels of the target
platform (a per-channel target is a planned follow-up).

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

**User database.** `cockatiel_user_database-rs` is a separate WebSocket service holding users, scores, roles, and per-user key/values. The engine is its only privileged client; mod commands (`commend`, `reprimand`, `ban`, `timeout`) map onto it. Platform roles (owner/mod/sponsor) are verified on login and merged with the user-db tier.

**TUI as supervisor.** `cockatiel_tui-rs` is the operator's control surface. It launches the engine + user database, discovers and registers modules, starts/stops/rebuilds them (prebuilt binary → rebuild → crash-recovery ladder), edits their `.env`/`config.json` in place, answers approval/credential prompts, and streams live logs.

**Security model.**

- **PIN + JWT name-trust.** A first connection proves the PIN; thereafter every message carries a JWT whose `name` claim must match the container's `module_name` — a valid token can't be replayed under a trusted name (like the TUI) to reach gated capabilities. The PIN remains the master key for first connections. Blank/`unnamed_module` identities are rejected outright, and the supervisor forces each module's identity via `--name`.
- **Module approval via prompts.** Connecting modules are not silently trusted: the engine raises a `Prompt`, the TUI (or term-chat) shows an approve/deny dialog, and the decision persists. Modules can also raise prompts (e.g. "enter the YouTube video ID"), answered in the TUI's prompts window.
- **Secrets vs settings.** Secrets (engine PIN/JWT secret, module tokens, OAuth keys) live in owner-only `.env` files; non-secret settings live in `config.json`. Sensitive credential fields are masked on screen. The PIN is delivered to modules via `COCKATIEL_PIN` env — never on the command line.
- **Credential isolation.** `module_list` redacts other modules' secrets (only the TUI and term-chat's OAuth-login may read them); `set_credentials` and `engine_info` (PIN) are control-surface-only — one module can't read or rewrite another's credentials.
- **Read-only data boundary.** Modules can only run `SELECT`/`WITH`/`EXPLAIN` queries against the timeline; writes go through gated engine queries. The timeline and user-db write periodic snapshots to backup paths, and the TUI warns loudly if no backup is configured.

## FAQ

> Why is global syncing paid?

Servers cost money, and sustainability has to be decoupled from user rights — otherwise you end up with the Discord/Palantir kind of platform. This keeps Cockatiel sustainable without violating users' rights.

> Why is there no blacklist of bad words by default?

I have no faith my account won't be insta-banned for shipping a list of slurs (AI moderation), and I'm not here to police your community. Set your own standards with your community.

> Why are the tests private?

AI companies train on data, and tests are a major data source. I refuse to feed companies I consider largely immoral (see my ethics on AI).

> Why Rust for the backend?

I personally prefer C/C++/Odin, but Cockatiel is frontend-facing and handles input from many sources at once — Rust gives the best balance of safety and performance for that.

> Why GPLv2?

I don't want this project hijacked or obfuscated by a larger company. This is meant to be owned by the community and something everyone benefits from, not just me.