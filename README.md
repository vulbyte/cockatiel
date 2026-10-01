# Cockatiel
Chat automation and moderation engine for streamers.

> note: if you're looking for dev impl details, refere to ./DEV.md

Cockatiel is a platform-agnostic chat engine. Platform adapters (Twitch, Kick, YouTube, Discord) feed every message into a 5-state engine pipeline: queued → pre-process → in-process → post-process → complete. where your modules censor, score, constrain, translate, synthesize speech, or archive whatever they want. All messages live in a timeline database that doubles as the queue, so nothing is lost and everything can be audited. You supervise the whole thing from a terminal TUI, and watch the chat in term-chat.

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
