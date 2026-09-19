# TUI_PLAN.md — Cockatiel TUI Dashboard

## Status: ACTIVE

## Architecture
- **Location**: `modules/cockatiel_module-tui-rs/`
- **Parent TUI** connects to engine via WebSocket (two-phase handshake)
- **Sub-windows** connect to parent TUI via WebSocket (same proto protocol)
- **Proto** is the one source of truth for all networking

## Layout
```
┌── logo ──┬── modules (unified) ──────────┐
│           │ [ENGINE]:                     │
│           │   cockatiel: connected        │
│           │   twitch_adapter: online      │
│           │                               │
├── log ────┤ [DATABASES]:                  │
│           │   timeline: connected (12MB)  │
│           │   backup: unknown             │
│           │                               │
│           │ [PLATFORMS]:                  │
│           │   twitch    CONNECTED  42 msgs│
│           │   youtube   CONNECTED  18 msgs│
├───────────┴───────────────────────────────┤
│              chart                        │
└───────────────────────────────────────────┘
```

## Completed Features
- [x] WebSocket client — connects to engine, queries DB via proto
- [x] Log window — browser-style filter tabs (All/Logs/Errors/System/Messages)
- [x] Per-window hotkeys — each window shows its own hotkeys at bottom
- [x] Mouse capture — focus-on-click, drag-to-resize borders
- [x] Pop-out — `[⧉]` button + `w` key spawns new terminal
- [x] Merged modules window — [ENGINE]/[DATABASES]/[PLATFORMS] sections
- [x] TUI WebSocket server — accepts sub-window connections
- [x] Sub-windows connect to parent via WS for live data
- [x] Parent always renders normally (no placeholder)
- [x] Drag inversion fix

## Remaining Tasks
- [ ] Broadcast engine logs to sub-windows
- [ ] Sub-window input back to parent (e.g., chat input)
- [ ] Terminal detection for spawning new windows
- [ ] Config for default layout sizes
