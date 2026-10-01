# Cockatiel — Project Conventions

## 32-bit max for ints/floats

All integer and float types are capped at 32-bit (`i32`/`u32`/`f32`; proto
`int32`/`uint32`/`float`, wire type fixed32). 32-bit systems are a target, and
the program does not need 64-bit precision.

- Use `f32`, not `f64`. Use `i32`/`u32`, not `i64`/`u64`.
- Proto fields are `int32`/`uint32`/`float`, never `int64`/`uint64`/`double`.
- **Exception (*):** values with NO smaller alternative stay 64-bit:
  epoch-millisecond timestamps, uuid7, JWT epoch-seconds (`iat`/`exp` — a 32-bit
  int wraps in 2038), monotonic/hash-derived counters, disk-byte sizes, and
  external-protocol 64-bit values (e.g. Discord gateway sequence numbers).
- The user-db `RankConfig` and all rank math use `f32`.

## Rank system: numbers for logic, names for display

- Ranks are **0-1 floats** (`User.rank`, proto `float`). All gating/config
  compares **numbers**, never tier names.
- Tier **names** (the mineral ladder Coal → Opal) exist ONLY in the repo-root
  `rank_chart.json`. The engine, the TUI and term-chat each read it; tier order
  in the file is arbitrary and consumers sort by `min` ascending (a rank maps
  to the highest tier whose `min <= rank`).
- `rank_chart.json` is the streamer-facing config: rename / add / remove tiers
  freely. Missing/parse-failed charts fall back to the built-in mineral
  template.
- The TUI injects the chart path as `COCKATIEL_RANK_CHART` into the engine and
  module processes so every consumer reads the same names.

## Other

- Run `cargo test --features probe --all-targets` in each crate you touch, and
  `cargo clippy` — a change that doesn't compile isn't done.