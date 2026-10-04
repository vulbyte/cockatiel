# Cockatiel
Chat automation and moderation engine for streamers.

> note: if you're looking for dev impl details, refere to ./DEV.md

Cockatiel is a platform-agnostic chat engine. Platform adapters (Twitch, Kick, YouTube, Discord) feed every message into a 5-state engine pipeline: queued → pre-process → in-process → post-process → complete. where your modules censor, score, constrain, translate, synthesize speech, or archive whatever they want. All messages live in a timeline database that doubles as the queue, so nothing is lost and everything can be audited. You supervise the whole thing from a terminal TUI, and watch the chat in term-chat.

---

## Getting started

This guide gets Cockatiel running on your machine from scratch. The whole stack
is Rust; there is no Python, no interpreter, and no web server to set up for the
core path.

### 1. Install the prerequisites

- **[Rust](https://rustup.rs/)** — the engine uses Rust 2024 edition, so install
  a recent stable toolchain (1.85+):
  ```sh
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  rustup update stable
  ```
- **[Git](https://git-scm.com/downloads)** — required to clone the repo and its
  submodules.

That's it for the core stack. Everything else (TLS, audio, protobuf) is a Rust
crate and builds itself. Optional extras:

- **[sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx)** — for the default
  `tts-rs` module, voice bundles download automatically on first start from the
  [sherpa-onnx `tts-models` release](https://github.com/k2-fsa/sherpa-onnx/releases/tag/tts-models).
  No install needed — the crate links ONNX Runtime statically.
- **[fake-input](https://github.com/vulbyte/cockatiel_module-fake_input-rs)**
  controller bridge — Linux needs `evdev`/`uinput` (kernel modules, usually
  present); Windows and macOS are built-in. Used only if you want the controller
  module.

### 2. Clone the repo (with submodules)

The project is split across many small repos tied together with git submodules.
You must clone **recursively**, and later update submodules whenever you pull:

```sh
git clone --recurse-submodules https://github.com/vulbyte/cockatiel
cd cockatiel
```

If you already cloned without `--recurse-submodules`:

```sh
git submodule update --init --recursive
```

### 3. Build

The **TUI** (`cockatiel_tui_v2-rs`) is your control surface. Build it in release
mode:

```sh
cargo build --release --manifest-path cockatiel_tui_v2-rs/Cargo.toml
```

On first build Cargo compiles ~20 dependent crates (engine, user-database,
protocol, all modules) — expect several minutes and a few GB of disk. The cargo
cache is shared, so subsequent builds are fast.

### 4. Launch

Run the TUI:

```sh
cargo run --release --manifest-path cockatiel_tui_v2-rs/Cargo.toml
```

On startup the TUI **automatically launches the engine and the user database**
(you can disable this later under the TUI config, `launch_engine`). It generates
and wires the credentials (PIN, tokens) between them for you — no manual setup.

### 5. Connect a platform

From the **modules window** in the TUI (a pane in the default layout — press
`Tab` to move focus between panes), find your platform adapter —
`twitch-adapter`, `youtube-adapter`, `kick-adapter`, or `discord-adapter` — and:

1. **Configure it** (press `e` to edit): set your channel/server, and paste your
   platform credentials when prompted (Twitch OAuth, YouTube API key, etc.).
   The TUI walks you through the auth flow.
2. **Start it** (press `s`). The adapter connects to the platform and begins
   feeding chat messages into the pipeline.

> Each platform adapter needs its own credentials. See the
> [platform modules](https://github.com/vulbyte/cockatiel/tree/main/modules) for
> per-platform setup details.

### 6. Add processing modules

Modules sit in the pipeline stages (pre → in → post). The usual starter set:

| Module | Stage | What it does |
|--------|-------|--------------|
| `banned-words` | in-process | censor / flag messages |
| `language-constrainer` | in-process | hold out-of-language messages for audit |
| `score-messages` | pre-process | score every message (feeds user ranks) |
| `events` | pre-process | predictions & polls |
| `commend` / `reprimand` | pre-process | chat ratings |
| `clip` | pre-process | `!clip` timestamps the stream |
| `tts-rs` | post-process | speak chat aloud (Piper voices, auto-download) |
| `term-chat` | — | a terminal chat reader with images, ranks & mod tools |

Start them the same way: select in the modules window, press `e` to configure,
then `s` to start. Modules that aren't built yet **build on demand** (the TUI
runs `cargo build --release` in the module directory first) — the cargo cache is
shared, so it's quick.

### 7. Watch the chat

Run `term-chat` to see messages, ranks, emoji, and embedded images in a terminal
window, or watch the timeline in the audit-viewer. You can supervise everything
from the TUI.

### First-run checklist

- [ ] Rust stable installed (`rustup update stable`)
- [ ] Repo cloned with submodules (`--recurse-submodules`)
- [ ] TUI built (`cargo build --release --manifest-path cockatiel_tui_v2-rs/Cargo.toml`)
- [ ] Platform adapter configured + started
- [ ] At least one processing module started (e.g. `score-messages`)

### Troubleshooting

- **`error: failed to run custom build command` during build** — usually a
  missing Rust build dependency. Ensure Rust is up to date (`rustup update
  stable`). A `Could not find protoc` error is NOT a missing dependency: the
  protocol crates now ship a vendored `protoc` binary, so no system
  `protobuf-compiler` install is required — update the repo (`git pull` +
  `git submodule update --init --recursive`) to pull in the fix.
- **TUI starts but no modules appear** — the modules window lists discovered
  modules; if it's empty, check that the repo was cloned with `--recurse-
  submodules` and run `git submodule update --init --recursive`.
- **Module won't connect / auth errors** — the engine and user-db generate
  credentials automatically when launched by the TUI. If you ran the engine
  manually, it needs `USER_DB_TOKEN` etc. from the user-db's `.env`; run
  everything from the TUI instead.
- **TTS is silent** — the first `!tts` or message triggers a voice-bundle
  download (default ~67 MB) into `voices/`. Give it a moment; check the module
  log in the TUI.

For deeper internals, see [DEV.md](./DEV.md).

---

## Why make this?
1. There isn't a good open-standard for chat interactions, and the tools that exist are painful to use or extend — so instead of stitching disconnected services together, this project is one cohesive engine. 
2. Most alternatives favor a specific platform, so Cockatiel is deliberately platform-agnostic: anyone on any platform can stream and give viewers the best experience possible. 
3. something you can run locally, remote, or in a hybrid fashion. this platform allows everything to run together, or seperately, it's your choice.






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

## Modules

Cockatiel's modules extend the pipeline (censor, score, translate, synthesize speech, archive, …). There are two tiers:

- **[Supported modules (Rust)](https://github.com/vulbyte/cockatiel/tree/main/modules)** — the default, stable set. Native Rust binaries, no interpreter, no environment setup; the supervisor runs them directly. These are what the majority of users should use.
- **[Experimental collection (Python)](https://github.com/vulbyte/cockatiel_module-tts_experimental_py)** — an opt-in escape hatch for model families the Rust set does not cover (e.g. `tts-experimental-py` for XTTS/Kokoro/F5 and other TTS models sherpa-onnx doesn't support). These need a working Python environment and are started manually, for users who want to go deeper.

The default TTS module is **`tts-rs`** (Rust, zero-setup); `tts-experimental-py` is the opt-in alternative.
