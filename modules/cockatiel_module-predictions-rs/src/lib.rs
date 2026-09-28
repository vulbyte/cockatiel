//! Cockatiel prediction module — zero-sum chat predictions.
//!
//! Two binaries share this crate:
//!   * `prediction_brain` — the chat-command module that owns prediction state,
//!     handles `!pred start/stop/bet`, applies parimutuel payouts and broadcasts
//!     `PredictionUpdate` bars to every connected module.
//!   * `prediction_display` — a minimal terminal window that renders those bars.
//!
//! The pure, socket-free prediction logic lives in [`logic`] so the payout math,
//! bet validation, role gating and command parsing are all unit-testable without
//! an engine. The brain's async read loop only calls into these functions.

pub mod logic;