# Cockatiel Roadmap (v1)

The v1 path to a complete, extensible chat engine. Done/pending reflects the live ledger in `PLANNING.md`.

## Checkpoint 1 — Core & Protocol

### Done
- [x] Protocol stability first — the Protobuf schema is locked before new features.
- [x] One-file imports: **JavaScript**, **Python**, **Rust** (ez import, README tutorial, `connect()`, `Send()`, `ReceiveAny()`, `Receive.protobuffType()`; Rust uses `Send(<protobuffType>, <target>, <message>)`).
- [x] Compliance & benchmark suite (`cockatiel_test_runner-rs`):
  - [x] Chain verification vs the real engine (fake messages → timeline)
  - [x] Per-module benchmark via a fake engine (accepts any auth)
  - [x] Throughput (req/s → projected msgs/min) + WS round-trip + crash/error detection
  - [x] Timeline archival of every test (batch `test-<uuid>`)
  - [x] TUI `t` triggers suites; `--suite/--module/--iterations` CLI
- [x] Message `Container` proto — `version`, `auth_token`, `module_name`, `module_instance_uuid7`, plus the payload oneof (ConnectionRequest/Return, AuthVerify, AuthNew, Command(s), MessagePre/In/PostProcess, TimelineEvent, UserData, Shutdown, Log, Err, SendToPlatforms, MessageAck, DatabaseQuery/Result, ModuleControl/Result, Prompt, PromptResponse, AuditFlag).

- [x] One-file import: **C** (`#import <cockatiel_lib.h/c>` or cmake) — `cockatiel_lib/c/`, nanopb codegen + libwebsockets, chain-tested live.
- [x] One-file import: **C#** (dotnet package) — `cockatiel_lib/dotnet/Cockatiel.cs` (single .cs + `Google.Protobuf` PackageReference), chain-tested live.
- [x] One-file import: **gdScript** (paste-into-project folder; `preload("res://...")`) — `cockatiel_lib/gdscript/cockatiel_lib.gd`, codec self-test + live chain test under `godot --headless`.
- [x] Test input module — the C/C#/gdScript clients each pass a live chain-dataflow test (ingest `MessagePreProcess` → timeline row verified), covering adapter→pre→in→post dataflow.
- [x] **Engine command system** — modules register `<flag><command>` subscriptions via `commands_payload` (empty list = catch-all, receives everything; `alert_on_unknown_command` opts into the apology reply). The engine parses `<flag><command> -flag:value … args` (strips the `-`, accepts `-p 2` and `-p:2`, bare flags as booleans), attaches the parsed `Command` (with flag values) to `ChatMessage.command`, and routes a known command ONLY to its owner + catch-alls. Built-in `!help` lists registered commands; an invalid command on an alerting flag replies "sorry \<user\>, that command isn't valid…".
- [x] **Chat ratings** — `chat_commend` / `chat_reprimand` engine queries (any verified user, gated to the two modules) backed by the user-db `rating_history` table (24h reprimand cooldown per giver→recipient, unit-tested). Modules: `cockatiel_module-commend-rs` (unlimited) + `cockatiel_module-reprimand-rs` (24h cooldown).

### Pending
- [x] **Adapter command migration** — twitch/kick/discord/youtube `!ban`/`!timeout` register via `commands_payload` and consume the engine-parsed command (routed with the parsed `Command` on `ChatMessage.command`); ingest-side raw parsing removed.
- [x] **Targeted command replies** — `ChatMessage.channel_id` + `SendToPlatforms.channel_id`: `!help`/invalid-command replies target the source channel (discord routes to it; single-channel adapters unaffected).
- [x] **Standalone command invocation** — engine handles `Payload::CommandPayload`: a module/UI can invoke a registered command outside any chat message.

## Checkpoint 2 — Engine modules

### Done
- [x] `timeline_database` — config check + create, add event, get event. The timeline table *is* the message queue.
- [x] `user_database` (`cockatiel_user_database-rs`, separate WS service) — config check/create/valid, init table, user control (read/write/delete), user value control (read/write/delete via uuid7), subprocess listening for: commend, reprimand, ban, timeout. Roles, scores, channels, and per-user key/values live here; the engine is its only privileged client.

## Checkpoint 3 — Platform modules

### Done
- [x] `discord_adapter` — config check/create, cockatiel_lib proto, receive from a specific channel or an entire server (guild-scoped), pass messages as pre-process to the engine, send as a user (`cockatiel: message`), ban with `-r --reason`, timeout with `-d`/`-r`, multi-server, image extraction, prompt-driven server selection.
- [x] `kick_adapter` — config, proto, read chat, pass as pre-process, send messages, ban (`-d`/`-r`), timeout (`-d`/`-r`).
- [x] `twitch_adapter` — config, proto, read chat, pass as pre-process, send messages, ban (`-d`/`-r`), timeout (`-d`/`-r`).
- [x] `youtube_adapter` — config, proto, read chat, pass as pre-process, send messages (Google OAuth `youtube.force-ssl`; auto browser flow or pasted refresh token), ban (`-d`/`-r`), timeout (`-d`/`-r`).

### Pending
- [x] discord: receive a `Send` and post as an **embed** (discordjs-style) — `embed_sends` toggle in `config.json` (default off).
- [x] discord: ban with `-d --duration` (discord bans are permanent; timeouts currently use `-d`) — `!ban @user -d <secs>` routes to `mod_timeout`.

## Checkpoint 4 — Processing modules

### Done
- [x] `banned_words` — config check/create, proto, parse for banned words (no spaces, leet, extra spaces), flag for review, soft/mid/hard censor, replace word, replace sentence, censor sentence, `sentence_mode`.
- [x] `score_messages` — config check/create, proto, contextual rewards/punishments with toggles + user-settable values: punctuation, questions, length, emoji frequency, bad punctuation, spam, no spacing, wordless (trigram validation).
- [x] `tts_service` (`tts_service-py`, post-process) — proto, take in a PostProcessMessage string, render via the desired model/method, play rendered TTS, configurable volume. Audio is returned on the message and persisted to the timeline for displays.
- [x] `language_constrainer` — see Checkpoint 7.

### Pending
- [ ] banned_words: optional lightweight LLM review — thorough check with flags via [llama-guard-3-1b](https://huggingface.co/meta-llama/Llama-Guard-3-1B); quick 0–1 probability check via [deberta-v3-small](https://huggingface.co/microsoft/deberta-v3-small).
- [x] tts: allow users to choose a custom model from a local file — `config.json` `model_source` (HF id or local dir) passed to the worker's `load(model=…)`.
- [x] tts: add a fallback when rendering fails — tries every worker in order, then replies with empty audio so the stage acks/completes.

## Checkpoint 5 — Minimal UI (term-chat)

### Done
- [x] `term_chat` (merged with mod-chat — see Checkpoint 6) — config check/create.
- [x] Disable custom colors (global).
- [x] Show user (with color) — toggles for username and color.
- [x] Show platform — toggles for username and color.
- [x] Show user status (sponsor/mod/admin/owner + commendation/reprimand) — toggle for user priv.
- [x] Show user rank (opal/trash/other derived from score) — toggle for display.
- [x] Message easing — smooth bursts (target messages/minute, toggle; instant when idle).
- [x] Emoji map (`emoji_map.json`, `:emoji_from_platform:` → emoji) — toggle.
- [x] Display images/gifs as ascii art — toggle; adaptive sizing (longest dimension ~80% of the terminal, re-measured on resize); built-in converter (`image` crate, prefers `ascii-image-converter` when installed); `image_map.json` string→URL map; `image_min_rank` rank/score gating with `<image>` placeholder + logged reasons.

### Pending
- [x] Disappear after x seconds — implemented as `message_fade_secs` (0 = off) + `message_fade_mode` (`remove`|`dim`) in `chat_config.json`.
- [x] Show user status: toggle for reprimand; toggle for colors — `show_reprimand` (compact red `R`, reads the enriched `reprimands` counter) + `show_status_color` (role badge letters).
- [x] Show user rank: toggle for colors — `show_rank_color` (rank text).

## Checkpoint 6 — Mod tools

### Done
- [x] `mod_chat` merged into `term_chat`; mod tools unlock after the operator logs in via a platform and is verified.
- [x] Login via platform — Twitch browser OAuth, Kick OAuth+PKCE, YouTube Google OAuth — to verify the operator's perms (platform tier first, then the user DB).
- [x] Engine double-checks every privileged command (`mod_*`, `SendToPlatforms`) against the user database before executing — no more viewer `!ban` hole.
- [x] Show user status (sponsor/mod/admin/owner + commendation/reprimand) — toggle.
- [x] Show user rank (opal/trash/other) — toggle.
- [x] Disable custom colors — toggle.
- [x] Message easing — toggle + "smooth amount" slider (target messages/sec).
- [x] Send message (appears as `cockatiel`; engine logs who sent it into the timeline).
- [x] Mod actions from chat: ban / timeout / commend / reprimand (engine verifies the actor).
- [x] Scrolling freezes the chat so a mod can act.
- [x] Display images/gifs — toggle (`none` | `all`).
- [x] Emoji map (`:emoji_from_platform:` map).
- [x] `timeline_web_ui` (first half): the terminal audit viewer (`cockatiel-audit-viewer`) — live messages/errors/audit/logs + module health from the timeline; the pattern to mirror in the web viewer.

### Pending
- [x] Disappear after x seconds — term-chat `message_fade_secs`/`message_fade_mode` (see Checkpoint 5).
- [ ] `timeline_web_ui` (web/HTML+JS viewer): config check/create, get messages from user, user notes (get/display/adjust on type), get errors, get logs, get users by property (bans, commendations, reprimands, totalscore, etc.), display user properties.

## Checkpoint 7 — Extend modules and features

### Done
- [x] `language_constrainer` — config check/create, proto, UTF-8 valid-character codes for the target language (toggles for emojis and expressive characters like `ඞ` or `(๑ > ᴗ < ๑)`), per-language map file + construction, out-of-language messages held for audit (a moderator approves/rejects via the prompt bus).

### Pending
- [x] One-file import: **C++** (`#import <cockatiel_lib.h/c>` or cmake) — `cockatiel_lib/cpp11/cockatiel_lib.hpp`, C++11 RAII wrapper over the C client, smoke-tested live.
- [x] One-file import: **odin** (`import "<path/to/file/cockatiel_lib>"`) — `cockatiel_lib/odin/`, native Odin (hand-rolled codec + WS over `core:net`), chain-tested live.
- [x] One-file import: **java** (??? — done as a single `Cockatiel.java` + vendored `protobuf-java` jar) — `cockatiel_lib/java/`, chain-tested live.
- [x] One-file import: **lua** (`local my_lib = require("mymodule")`) — `cockatiel_lib/lua/cockatiel_lib.lua`, LuaJIT FFI, chain-tested live.
- [ ] `web_chat_renderer` — config check/create, take in PostProcessMessage with user data, display in a CSS-based chat window; options: disappear after x seconds, animations on/off, disable custom colors.

---

## Cross-cutting gaps (from PLANNING.md)

- [ ] Real remote sync (`sync_to_remote()`) — currently a local-only stub.
- [ ] TUI user-db UI — results currently surface in the log window only.
- [x] Local DB size warning logic (5 MB floor, 95% target warning) — `timeline_database_target_mb` + one-shot broadcast + `db_status` fields.
- [ ] YouTube adapter needs a real Data API key (config holds a placeholder the API rejects).

---

## Side research for possible future modules

- bilibili: `scraping?` — [scraping option](https://github.com/mistgc/bili-live-chat)
- facebook: curl — [docs](https://developers.facebook.com/docs/live-video-api/)
- instagram: no idea — [i think this is not it but worth a shot](https://developers.facebook.com/docs/messenger-platform/instagram/)
- kick: curl — [docs](https://docs.kick.com/apis/livestreams)
- picarto: curl-http — [docs](https://api.picarto.tv/)
- tiktok: `scraping?` — [scraping option for now](https://github.com/jpw142/rscraperTikTokLive)
- twitch: curl-http — [docs](https://dev.twitch.tv/docs/chat/send-receive-messages/)
- twitter: curl-http/webhook — [docs i think](https://docs.x.com/x-api/introduction?search=livestream+messages)
- vimeo: no idea — [i believe these are the docs](https://help.vimeo.com/hc/en-us/articles/12427783601937-How-to-use-the-Vimeo-Live-API)