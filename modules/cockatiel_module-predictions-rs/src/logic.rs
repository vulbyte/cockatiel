//! Pure prediction logic — no sockets, no engine types, fully unit-testable.
//!
//! Everything the brain needs that can be expressed as a function over plain
//! data lives here: command/subcommand parsing, label defaults, the role gate,
//! bet validation, parimutuel payout math and refunds. The brain's async read
//! loop is a thin shell over these.

use std::collections::HashMap;

/// A side of a prediction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// Lifecycle of a prediction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Open,
    Resolved,
    Cancelled,
}

/// A user's accumulated bets. One entry per user; a user may hold amounts on
/// both sides (a bet accumulates into whichever side it was placed on).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bet {
    pub left: i64,
    pub right: i64,
}

impl Bet {
    pub fn side_amount(&self, side: Side) -> i64 {
        match side {
            Side::Left => self.left,
            Side::Right => self.right,
        }
    }

    pub fn total(&self) -> i64 {
        self.left + self.right
    }
}

/// The in-memory prediction owned by the brain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prediction {
    pub id: String,
    pub prompt: String,
    pub left_label: String,
    pub right_label: String,
    pub bets: HashMap<String, Bet>,
    pub status: Status,
    pub winner: Option<Side>,
    pub created_at: String,
}

/// One payout credit for a winning bettor (parimutuel, integer floor).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payout {
    pub user: String,
    pub amount: i64,
}

/// One full refund for a cancelled prediction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refund {
    pub user: String,
    pub amount: i64,
}

/// Why a bet was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BetError {
    /// amount <= 0, or below the configured bet_min.
    InvalidAmount,
    /// amount > the actor's current score.
    InsufficientScore,
    /// amount > the configured bet_max (bet_max 0 = unlimited).
    MaxBetExceeded,
}

impl Prediction {
    pub fn new(
        id: String,
        prompt: String,
        left_label: String,
        right_label: String,
        created_at: String,
    ) -> Self {
        Prediction {
            id,
            prompt,
            left_label,
            right_label,
            bets: HashMap::new(),
            status: Status::Open,
            winner: None,
            created_at,
        }
    }

    pub fn side_total(&self, side: Side) -> i64 {
        self.bets
            .values()
            .map(|b| b.side_amount(side))
            .sum()
    }

    pub fn pot(&self) -> i64 {
        self.bets.values().map(Bet::total).sum()
    }

    /// Record a bet for `user` on `side`, validating the amount against the
    /// actor's `score` and the configured bet_min/bet_max. Returns the recorded
    /// amount on success. Pure: mutates only this prediction's state.
    pub fn place_bet(
        &mut self,
        user: &str,
        side: Side,
        amount: i64,
        score: i64,
        bet_min: i64,
        bet_max: i64,
    ) -> Result<i64, BetError> {
        if amount <= 0 || amount < bet_min {
            return Err(BetError::InvalidAmount);
        }
        if amount > score {
            return Err(BetError::InsufficientScore);
        }
        if bet_max > 0 && amount > bet_max {
            return Err(BetError::MaxBetExceeded);
        }
        let entry = self.bets.entry(user.to_string()).or_default();
        match side {
            Side::Left => entry.left += amount,
            Side::Right => entry.right += amount,
        }
        Ok(amount)
    }

    /// Resolve to `winner` and compute parimutuel payouts (integer floor):
    /// `payout(user) = bet(user) × pot / winning_side_total`. The pot is fully
    /// redistributed among winners — nothing is created; integer flooring can
    /// leave a sub-point remainder uncredited, never a credit above the pot.
    /// Marks the prediction Resolved and returns the payouts to apply.
    pub fn resolve(&mut self, winner: Side) -> Vec<Payout> {
        let pot = self.pot();
        let win_total = self.side_total(winner);
        let mut out = Vec::new();
        if win_total > 0 {
            for (user, bet) in &self.bets {
                let amt = bet.side_amount(winner);
                if amt > 0 {
                    let payout = amt * pot / win_total;
                    if payout > 0 {
                        out.push(Payout {
                            user: user.clone(),
                            amount: payout,
                        });
                    }
                }
            }
        }
        self.status = Status::Resolved;
        self.winner = Some(winner);
        out
    }

    /// Cancel the prediction: refund every bet in full. Marks it Cancelled and
    /// returns the refunds to apply.
    pub fn cancel(&mut self) -> Vec<Refund> {
        let mut out = Vec::new();
        for (user, bet) in &self.bets {
            if bet.total() > 0 {
                out.push(Refund {
                    user: user.clone(),
                    amount: bet.total(),
                });
            }
        }
        self.status = Status::Cancelled;
        self.winner = None;
        out
    }
}

/// Lifecycle of a poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollStatus {
    Open,
    Closed,
}

/// Why a vote was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollError {
    /// option_index is not a valid option (0..options.len()).
    InvalidOption,
    /// this user already voted — the FIRST vote is kept.
    AlreadyVoted,
}

/// A free-vote poll: no score moves, one vote per user (first wins), up to
/// six options. `votes` is parallel to `options`; `voters` maps user_uuid7 →
/// the option index they voted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Poll {
    pub id: String,
    pub prompt: String,
    pub options: Vec<String>,
    pub votes: Vec<i64>,
    pub voters: HashMap<String, usize>,
    pub status: PollStatus,
    pub winner_index: i32,
    pub hide_counts: bool,
}

impl Poll {
    pub fn new(id: String, prompt: String, options: Vec<String>, hide_counts: bool) -> Self {
        let votes = vec![0; options.len()];
        Poll {
            id,
            prompt,
            options,
            votes,
            voters: HashMap::new(),
            status: PollStatus::Open,
            winner_index: -1,
            hide_counts,
        }
    }

    /// Record one vote for `user` on `option_index`. A user's first vote wins;
    /// repeats are rejected. Pure: mutates only this poll's state.
    pub fn cast_vote(&mut self, user: &str, option_index: usize) -> Result<usize, PollError> {
        if option_index >= self.options.len() {
            return Err(PollError::InvalidOption);
        }
        if self.voters.contains_key(user) {
            return Err(PollError::AlreadyVoted);
        }
        self.voters.insert(user.to_string(), option_index);
        self.votes[option_index] += 1;
        Ok(option_index)
    }

    pub fn total(&self) -> i64 {
        self.votes.iter().sum()
    }

    /// Close the poll and compute the winner: the option with the highest
    /// count, only when it holds the strict maximum. A tie for the top (or an
    /// all-zero poll) yields no winner. Sets status Closed and winner_index
    /// (0-based, or -1 when there is no winner).
    pub fn close(&mut self) -> Option<usize> {
        let winner = (0..self.options.len())
            .max_by_key(|&i| self.votes[i])
            .filter(|&i| {
                self.votes[i] > 0
                    && !self
                        .votes
                        .iter()
                        .enumerate()
                        .any(|(j, &c)| j != i && c == self.votes[i])
            });
        self.status = PollStatus::Closed;
        self.winner_index = winner.map(|i| i as i32).unwrap_or(-1);
        winner
    }
}

/// The user's platform role, from coarsest to most privileged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    User,
    Sponsor,
    Moderator,
    Admin,
    Owner,
}

/// Minimal projection of `UserData` so the role gate stays proto-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UserInfo {
    pub is_sponsor: bool,
    pub is_moderator: bool,
    pub is_admin: bool,
    pub is_owner: bool,
}

impl UserInfo {
    pub fn role(&self) -> Role {
        if self.is_owner {
            Role::Owner
        } else if self.is_admin {
            Role::Admin
        } else if self.is_moderator {
            Role::Moderator
        } else if self.is_sponsor {
            Role::Sponsor
        } else {
            Role::User
        }
    }
}

/// Map a configured role name onto its [`Role`] level. Unknown requirements
/// default to `Moderator` so a typo'd `creator_role` never opens the door to
/// unprivileged users.
pub fn required_role_level(required: &str) -> Role {
    match required.trim().to_lowercase().as_str() {
        "owner" => Role::Owner,
        "admin" => Role::Admin,
        "mod" | "moderator" => Role::Moderator,
        "sponsor" => Role::Sponsor,
        "user" | "" => Role::User,
        _ => Role::Moderator,
    }
}

/// Role gate for start/stop: the actor (if present) must meet `required_role`.
/// A missing user always fails closed.
pub fn role_gate(user: Option<&UserInfo>, required: &str) -> bool {
    match user {
        Some(u) => u.role() >= required_role_level(required),
        None => false,
    }
}

/// Resolve the left/right labels for a `start` command, defaulting to Yes/No.
pub fn start_labels(left: Option<&str>, right: Option<&str>) -> (String, String) {
    (
        left.unwrap_or("Yes").to_string(),
        right.unwrap_or("No").to_string(),
    )
}

/// A fully-parsed `!pred` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedCommand {
    Start {
        prompt: String,
        left_label: Option<String>,
        right_label: Option<String>,
    },
    Bet {
        side: Side,
        amount: i64,
    },
    Stop {
        winner: Option<Side>,
    },
    /// Not a recognizable pred invocation (empty, unknown subcommand, no flags).
    None,
}

/// Pull a parsed flag value out of the engine's `command_flags` list
/// (each `Flag` is (flag_name, value); the engine strips the leading `-`).
fn flag_value<'a>(flags: &'a [(String, String)], name: &str) -> Option<&'a str> {
    flags
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

/// Normalize a `-w` value to a side, accepting l/r/left/right.
fn parse_winner(v: &str) -> Option<Side> {
    match v.trim().to_lowercase().as_str() {
        "l" | "left" => Some(Side::Left),
        "r" | "right" => Some(Side::Right),
        _ => None,
    }
}

/// Remove flag tokens (`-name` and their value) from a token slice, mirroring
/// the engine's flag parser (`-name:value`, `-name value`, or bare `-name`).
fn strip_flag_tokens(tokens: &[&str]) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        if t.starts_with('-') && t.len() > 1 {
            let body = &t[1..];
            if !body.contains(':') && i + 1 < tokens.len() && !tokens[i + 1].starts_with('-') {
                i += 1; // consume the flag's value token
            }
        } else {
            out.push(t);
        }
        i += 1;
    }
    out.join(" ")
}

/// The poll PROMPT is the free text after the `-p` flag, because the engine's
/// flag parser only assigns one token to a flag. `-p Will we hit 1k?` reaches
/// the module as flags=[("p","Will")] plus the raw tokens. Take everything from
/// the `-p` token onward, dropping any option flags (`-N`) and their values and
/// a trailing bare `-h`, so the prompt survives multi-word.
fn poll_prompt_from_tokens(tokens: &[&str], flag_value: Option<&str>) -> String {
    // Find the `-p` token position.
    let mut start = None;
    for (i, t) in tokens.iter().enumerate() {
        if *t == "-p" || t.starts_with("-p:") {
            start = Some(i + 1);
            break;
        }
    }
    let mut words: Vec<&str> = Vec::new();
    let mut i = start.unwrap_or(0);
    while i < tokens.len() {
        let t = tokens[i];
        if t.starts_with('-') && t.len() > 1 {
            let body = &t[1..];
            // A bare `-h` / `-N` flag: skip it (and, for `-N`, its value token).
            if body == "h" {
                i += 1;
                continue;
            }
            if body.chars().all(|c| c.is_ascii_digit()) && !body.contains(':') {
                if i + 1 < tokens.len() && !tokens[i + 1].starts_with('-') {
                    i += 1;
                }
                i += 1;
                continue;
            }
            // A colon-form flag carries its own value inline; skip it too.
            i += 1;
            continue;
        }
        words.push(t);
        i += 1;
    }
    let joined = words.join(" ").trim().to_string();
    if joined.is_empty() {
        flag_value.unwrap_or("").trim().to_string()
    } else {
        joined
    }
}

/// Parse a routed `!pred` invocation from the raw message text (the subcommand
/// token lives in the text; the `-l/-r/-w` values come from the engine's parsed
/// flags). Modeled on commend's `strip_command`.
pub fn parse_command(
    raw: &str,
    flag: &str,
    name: &str,
    flags: &[(String, String)],
) -> ParsedCommand {
    let mut rest = raw.trim();
    if let Some(idx) = rest.find(flag) {
        rest = rest[idx + flag.len()..].trim_start();
    }
    if rest.starts_with(name) {
        rest = rest[name.len()..].trim_start();
    }
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    match tokens.first().copied() {
        Some("start") => {
            let prompt = strip_flag_tokens(&tokens[1..]);
            let left_label = flag_value(flags, "l").map(String::from);
            let right_label = flag_value(flags, "r").map(String::from);
            ParsedCommand::Start {
                prompt,
                left_label,
                right_label,
            }
        }
        Some("stop") => {
            let winner = flag_value(flags, "w").and_then(parse_winner);
            ParsedCommand::Stop { winner }
        }
        Some(_) => {
            // Bet: `-l <amount>` / `-r <amount>`. Disambiguated from start by
            // the absence of the `start` subcommand token.
            if let Some(v) = flag_value(flags, "l") {
                if let Ok(amount) = v.trim().parse::<i64>() {
                    return ParsedCommand::Bet {
                        side: Side::Left,
                        amount,
                    };
                }
            }
            if let Some(v) = flag_value(flags, "r") {
                if let Ok(amount) = v.trim().parse::<i64>() {
                    return ParsedCommand::Bet {
                        side: Side::Right,
                        amount,
                    };
                }
            }
            ParsedCommand::None
        }
        None => ParsedCommand::None,
    }
}

/// A fully-parsed `!poll` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollCommand {
    Create {
        prompt: String,
        options: Vec<String>,
        hide_counts: bool,
    },
    Vote {
        option_index: usize,
    },
    Stop,
    /// Not a recognizable poll invocation (empty, unknown subcommand, no flags).
    None,
}

/// Parse a routed `!poll` invocation from the raw message text. A `-p` flag
/// means Create (collect the `-1`..`-6` option labels in order; a poll needs
/// at least 2 options to be meaningful, so fewer → None; `-h` hides counts).
/// A bare `-1`..`-6` with no `-p` is a Vote for that option; the `stop`
/// subcommand closes the poll.
pub fn parse_poll_command(
    raw: &str,
    flag: &str,
    name: &str,
    flags: &[(String, String)],
) -> PollCommand {
    let mut rest = raw.trim();
    if let Some(idx) = rest.find(flag) {
        rest = rest[idx + flag.len()..].trim_start();
    }
    if rest.starts_with(name) {
        rest = rest[name.len()..].trim_start();
    }
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    if tokens.first().copied() == Some("stop") {
        return PollCommand::Stop;
    }
    // Create: a `-p` flag present (the prompt is its value).
    if flag_value(flags, "p").is_some() {
        let mut options = Vec::new();
        for n in 1..=6 {
            if let Some(v) = flag_value(flags, &n.to_string()) {
                if !v.trim().is_empty() {
                    options.push(v.to_string());
                }
            }
        }
        if options.len() < 2 {
            return PollCommand::None;
        }
        // The engine's flag parser gives a flag a SINGLE token value, so
        // `-p Will we hit 1k?` would lose everything after "Will". Recover the
        // full prompt from the raw text instead: take everything after the `-p`
        // token, dropping option flags (`-N`) and `-h`. Same spirit as the
        // `pred` start parser's raw-text recovery.
        let prompt = poll_prompt_from_tokens(&tokens[1..], flag_value(flags, "p"));
        if prompt.is_empty() {
            return PollCommand::None;
        }
        return PollCommand::Create {
            prompt,
            options,
            hide_counts: flag_value(flags, "h").is_some(),
        };
    }
    // Vote: a bare `-N` (1-6) with no `-p`. Out-of-range `-N` is None.
    for n in 1..=6 {
        if flag_value(flags, &n.to_string()).is_some() {
            return PollCommand::Vote {
                option_index: n - 1,
            };
        }
    }
    PollCommand::None
}

/// Width of each side's bar in the terminal render.
pub const BAR_WIDTH: usize = 40;

/// Left-side bar: fills left-to-right, proportional to its share of the pot.
fn bar_left(share: i64, pot: i64) -> String {
    if pot <= 0 {
        return "░".repeat(BAR_WIDTH);
    }
    let filled = ((share as f64) / (pot as f64) * BAR_WIDTH as f64)
        .round()
        .min(BAR_WIDTH as f64) as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(BAR_WIDTH - filled))
}

/// Right-side bar: fills right-to-left, proportional to its share of the pot.
fn bar_right(share: i64, pot: i64) -> String {
    if pot <= 0 {
        return "░".repeat(BAR_WIDTH);
    }
    let filled = ((share as f64) / (pot as f64) * BAR_WIDTH as f64)
        .round()
        .min(BAR_WIDTH as f64) as usize;
    format!("{}{}", "░".repeat(BAR_WIDTH - filled), "█".repeat(filled))
}

/// Render the full display screen (ANSI clear + prompt + two side bars +
/// totals + pot + result line) from plain fields, so the display window and the
/// tests share one pure renderer with no wire types.
pub fn render_screen(
    prompt: &str,
    left_label: &str,
    left_total: i64,
    right_label: &str,
    right_total: i64,
    status: Status,
    winner: Option<Side>,
) -> String {
    let pot = left_total + right_total;
    let mut s = String::new();
    s.push_str("\x1b[2J\x1b[H");
    s.push_str(&format!("Prediction: {}\n", prompt));
    s.push_str(&format!(
        "Left  [{}] {} {}\n",
        left_label,
        bar_left(left_total, pot),
        left_total
    ));
    s.push_str(&format!(
        "Right [{}] {} {}\n",
        right_label,
        bar_right(right_total, pot),
        right_total
    ));
    s.push_str(&format!("Pot: {}\n", pot));
    match status {
        Status::Resolved => {
            let (wside, wlabel) = match winner {
                Some(Side::Left) => ("Left", left_label),
                Some(Side::Right) => ("Right", right_label),
                None => ("?", "?"),
            };
            s.push_str(&format!(
                ">>> {} [{}] WINS — {} points split <<<\n",
                wside, wlabel, pot
            ));
        }
        Status::Cancelled => {
            s.push_str(">>> prediction cancelled — refunded <<<\n");
        }
        Status::Open => {}
    }
    s
}

/// The display's idle screen: no active prediction. ANSI clear + a plain line,
/// so the hold-then-clear in the display window shares the pure renderer.
pub fn render_idle() -> String {
    "\x1b[2J\x1b[HNo active prediction.\n".to_string()
}

/// Render a poll display screen (ANSI clear + title + one line per option +
/// total + winner line) from plain fields, so the display window and the tests
/// share one pure renderer with no wire types. While a poll is OPEN and
/// `hide_counts`, counts render as `?` to prevent bandwagon bias; closing
/// always reveals them. A CLOSED poll ends with the winner line (or a
/// "no winner (tie)" line when winner_index < 0).
pub fn render_poll_screen(
    prompt: &str,
    options: &[String],
    counts: &[i64],
    total: i64,
    status: PollStatus,
    winner_index: i32,
    hide_counts: bool,
) -> String {
    let mut s = String::new();
    s.push_str("\x1b[2J\x1b[H");
    s.push_str(&format!("Poll: {}\n", prompt));
    for (i, opt) in options.iter().enumerate() {
        let count = match status {
            PollStatus::Open if hide_counts => "?".to_string(),
            _ => counts.get(i).copied().unwrap_or(0).to_string(),
        };
        s.push_str(&format!("{}. {} : {}\n", i + 1, opt, count));
    }
    s.push_str(&format!("Total votes: {}\n", total));
    if status == PollStatus::Closed {
        let i = winner_index as usize;
        if winner_index >= 0 && i < options.len() {
            s.push_str(&format!(">>> winner: {} <<<\n", options[i]));
        } else {
            s.push_str(">>> no winner (tie) <<<\n");
        }
    }
    s
}

/// The display's idle screen for polls: no active poll. Kept separate from
/// [`render_idle`] so the prediction and poll windows clear to their own line.
pub fn render_poll_idle() -> String {
    "\x1b[2J\x1b[HNo active poll.\n".to_string()
}

/// Minimal UUIDv7 generator (no uuid dependency): 48-bit unix-ms timestamp,
/// version 7, RFC 4122 variant, 62 random bits seeded from the platform RNG.
/// Not security-sensitive — the id only needs to be unique per prediction.
pub fn new_uuid7() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    // Two independent hashers → 128 bits of system-seeded randomness.
    let r1 = RandomState::new().build_hasher().finish();
    let r2 = RandomState::new().build_hasher().finish();

    let mut b = [0u8; 16];
    b[0] = (ms >> 40) as u8;
    b[1] = (ms >> 32) as u8;
    b[2] = (ms >> 24) as u8;
    b[3] = (ms >> 16) as u8;
    b[4] = (ms >> 8) as u8;
    b[5] = ms as u8;
    b[6] = ((r1 >> 56) as u8 & 0x0F) | 0x70; // version 7
    b[7] = (r1 >> 48) as u8;
    b[8] = ((r1 >> 40) as u8 & 0x3F) | 0x80; // variant 10
    b[9] = (r1 >> 32) as u8;
    b[10] = (r1 >> 24) as u8;
    b[11] = (r1 >> 16) as u8;
    b[12] = (r1 >> 8) as u8;
    b[13] = r1 as u8;
    b[14] = (r2 >> 8) as u8;
    b[15] = r2 as u8;

    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13],
        b[14], b[15]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(_start: &str) -> Prediction {
        Prediction::new(
            "pred-1".to_string(),
            "Will we hit 1k?".to_string(),
            "Yes".to_string(),
            "No".to_string(),
            "0".to_string(),
        )
    }

    fn user(owner: bool, admin: bool, mod_: bool, sponsor: bool) -> UserInfo {
        UserInfo {
            is_owner: owner,
            is_admin: admin,
            is_moderator: mod_,
            is_sponsor: sponsor,
        }
    }

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(n, val)| (n.to_string(), val.to_string()))
            .collect()
    }

    // ── 1. Flag/subcommand parsing ────────────────────────────────────

    #[test]
    fn start_with_prompt() {
        assert_eq!(
            parse_command("!pred start Will we hit 1k?", "!", "pred", &[]),
            ParsedCommand::Start {
                prompt: "Will we hit 1k?".to_string(),
                left_label: None,
                right_label: None,
            }
        );
    }

    #[test]
    fn bet_left() {
        assert_eq!(
            parse_command("!pred -l 5000", "!", "pred", &pairs(&[("l", "5000")])),
            ParsedCommand::Bet {
                side: Side::Left,
                amount: 5000,
            }
        );
    }

    #[test]
    fn bet_right() {
        assert_eq!(
            parse_command("!pred -r 100", "!", "pred", &pairs(&[("r", "100")])),
            ParsedCommand::Bet {
                side: Side::Right,
                amount: 100,
            }
        );
    }

    #[test]
    fn stop_no_winner() {
        assert_eq!(
            parse_command("!pred stop", "!", "pred", &[]),
            ParsedCommand::Stop { winner: None }
        );
    }

    #[test]
    fn stop_winner_right() {
        assert_eq!(
            parse_command("!pred stop -w r", "!", "pred", &pairs(&[("w", "r")])),
            ParsedCommand::Stop {
                winner: Some(Side::Right),
            }
        );
    }

    #[test]
    fn stop_winner_left_spelled_out() {
        assert_eq!(
            parse_command("!pred stop -w left", "!", "pred", &pairs(&[("w", "left")])),
            ParsedCommand::Stop {
                winner: Some(Side::Left),
            }
        );
    }

    #[test]
    fn not_a_pred_invocation() {
        assert_eq!(parse_command("!pred", "!", "pred", &[]), ParsedCommand::None);
        assert_eq!(
            parse_command("hello there", "!", "pred", &[]),
            ParsedCommand::None
        );
    }

    // ── 2. Label defaults + overrides ─────────────────────────────────

    #[test]
    fn default_labels() {
        assert_eq!(
            start_labels(None, None),
            ("Yes".to_string(), "No".to_string())
        );
    }

    #[test]
    fn overridden_labels() {
        assert_eq!(
            start_labels(Some("A"), Some("B")),
            ("A".to_string(), "B".to_string())
        );
    }

    #[test]
    fn partial_label_override() {
        assert_eq!(
            start_labels(Some("A"), None),
            ("A".to_string(), "No".to_string())
        );
    }

    #[test]
    fn start_labels_from_flags() {
        let parsed = parse_command(
            "!pred start -l A -r B Will we hit 1k?",
            "!",
            "pred",
            &pairs(&[("l", "A"), ("r", "B")]),
        );
        assert_eq!(
            parsed,
            ParsedCommand::Start {
                prompt: "Will we hit 1k?".to_string(),
                left_label: Some("A".to_string()),
                right_label: Some("B".to_string()),
            }
        );
    }

    // ── 3. Role gate ──────────────────────────────────────────────────

    #[test]
    fn mod_requirement_passes_privileged() {
        assert!(role_gate(Some(&user(true, false, false, false)), "mod"));
        assert!(role_gate(Some(&user(false, true, false, false)), "mod"));
        assert!(role_gate(Some(&user(false, false, true, false)), "mod"));
    }

    #[test]
    fn mod_requirement_fails_unprivileged() {
        assert!(!role_gate(Some(&user(false, false, false, true)), "mod"));
        assert!(!role_gate(Some(&user(false, false, false, false)), "mod"));
        assert!(!role_gate(None, "mod"));
    }

    #[test]
    fn stricter_requirements() {
        assert!(role_gate(Some(&user(false, true, false, false)), "admin"));
        assert!(!role_gate(Some(&user(false, false, true, false)), "admin"));
        assert!(role_gate(Some(&user(true, false, false, false)), "owner"));
        assert!(!role_gate(Some(&user(false, true, false, false)), "owner"));
    }

    #[test]
    fn sponsor_requirement() {
        assert!(role_gate(Some(&user(false, false, false, true)), "sponsor"));
        assert!(!role_gate(Some(&user(false, false, false, false)), "sponsor"));
    }

    // ── 4. Bet validation ─────────────────────────────────────────────

    #[test]
    fn rejects_invalid_amounts() {
        let mut pred = p("open");
        assert_eq!(
            pred.place_bet("u1", Side::Left, 0, 5000, 1, 0),
            Err(BetError::InvalidAmount)
        );
        assert_eq!(
            pred.place_bet("u1", Side::Left, -5, 5000, 1, 0),
            Err(BetError::InvalidAmount)
        );
        assert_eq!(
            pred.place_bet("u1", Side::Left, 0, 5000, 1, 0),
            Err(BetError::InvalidAmount)
        );
    }

    #[test]
    fn rejects_below_bet_min() {
        let mut pred = p("open");
        assert_eq!(
            pred.place_bet("u1", Side::Left, 1, 5000, 10, 0),
            Err(BetError::InvalidAmount)
        );
        assert_eq!(pred.place_bet("u1", Side::Left, 10, 5000, 10, 0), Ok(10));
    }

    #[test]
    fn rejects_over_score() {
        let mut pred = p("open");
        assert_eq!(
            pred.place_bet("u1", Side::Left, 5001, 5000, 1, 0),
            Err(BetError::InsufficientScore)
        );
    }

    #[test]
    fn rejects_over_bet_max() {
        let mut pred = p("open");
        assert_eq!(
            pred.place_bet("u1", Side::Left, 101, 5000, 1, 100),
            Err(BetError::MaxBetExceeded)
        );
        assert_eq!(pred.place_bet("u1", Side::Left, 100, 5000, 1, 100), Ok(100));
    }

    #[test]
    fn accepts_within_score() {
        let mut pred = p("open");
        assert_eq!(pred.place_bet("u1", Side::Left, 5000, 5000, 1, 0), Ok(5000));
        assert_eq!(pred.side_total(Side::Left), 5000);
    }

    // ── 5. Payout math (the critical tests) ───────────────────────────

    #[test]
    fn parimutuel_2_to_1_split_resolve_left() {
        // A (left) total 10000 split 5000/5000 across two bettors, B (right)
        // total 5000 from one bettor. Pot 15000. Resolve left:
        //   each A bettor: floor(5000 * 15000 / 10000) = 7500 (1.5×).
        let mut pred = p("open");
        pred.place_bet("u1", Side::Left, 5000, 5000, 1, 0).unwrap();
        pred.place_bet("u2", Side::Left, 5000, 5000, 1, 0).unwrap();
        pred.place_bet("u3", Side::Right, 5000, 5000, 1, 0).unwrap();
        assert_eq!(pred.pot(), 15000);

        let payouts = pred.resolve(Side::Left);
        assert_eq!(pred.status, Status::Resolved);
        assert_eq!(pred.winner, Some(Side::Left));

        let mut by_user: HashMap<&str, i64> =
            payouts.iter().map(|p| (p.user.as_str(), p.amount)).collect();
        assert_eq!(by_user.remove("u1"), Some(7500));
        assert_eq!(by_user.remove("u2"), Some(7500));
        assert_eq!(by_user.remove("u3"), None);
        assert!(by_user.is_empty());

        // Zero-sum invariant: sum(bets) == sum(payouts).
        let payout_sum: i64 = payouts.iter().map(|p| p.amount).sum();
        assert_eq!(payout_sum, 15000);
        assert_eq!(pred.pot(), payout_sum);
    }

    #[test]
    fn parimutuel_2_to_1_split_resolve_minority_right() {
        // Same 15000 pot, but resolve the minority side (right, total 5000):
        //   the B bettor gets floor(5000 * 15000 / 5000) = 15000 (3×).
        let mut pred = p("open");
        pred.place_bet("u1", Side::Left, 5000, 5000, 1, 0).unwrap();
        pred.place_bet("u2", Side::Left, 5000, 5000, 1, 0).unwrap();
        pred.place_bet("u3", Side::Right, 5000, 5000, 1, 0).unwrap();

        let payouts = pred.resolve(Side::Right);
        assert_eq!(pred.winner, Some(Side::Right));
        assert_eq!(payouts.len(), 1);
        assert_eq!(payouts[0].user, "u3");
        assert_eq!(payouts[0].amount, 15000);

        let payout_sum: i64 = payouts.iter().map(|p| p.amount).sum();
        assert_eq!(payout_sum, 15000);
        assert_eq!(pred.pot(), payout_sum);
    }

    #[test]
    fn parimutuel_floor_case_odd_pot_exact() {
        // Odd pot 7 (left 3, right 4). Resolve left (single bettor of 3):
        //   floor(3 * 7 / 3) = 7 — the whole pot. Zero-sum holds exactly.
        let mut pred = p("open");
        pred.place_bet("u1", Side::Left, 3, 5000, 1, 0).unwrap();
        pred.place_bet("u2", Side::Right, 4, 5000, 1, 0).unwrap();
        assert_eq!(pred.pot(), 7);

        let payouts = pred.resolve(Side::Left);
        assert_eq!(payouts[0].amount, 7);
        let payout_sum: i64 = payouts.iter().map(|p| p.amount).sum();
        assert_eq!(payout_sum, 7);
        assert_eq!(pred.pot(), payout_sum);
    }

    #[test]
    fn parimutuel_floor_truncates_never_creates() {
        // Pot 5, winning side (left) total 4 split 2/2, losing side 1.
        //   each winner: floor(2 * 5 / 4) = 2. Sum = 4 < 5: the 1-point
        //   remainder is uncredited by integer flooring — nothing is created.
        let mut pred = p("open");
        pred.place_bet("u1", Side::Left, 2, 5000, 1, 0).unwrap();
        pred.place_bet("u2", Side::Left, 2, 5000, 1, 0).unwrap();
        pred.place_bet("u3", Side::Right, 1, 5000, 1, 0).unwrap();

        let payouts = pred.resolve(Side::Left);
        assert!(payouts.iter().all(|p| p.amount == 2));
        let payout_sum: i64 = payouts.iter().map(|p| p.amount).sum();
        assert_eq!(payout_sum, 4);
        // Never above the pot (nothing created).
        assert!(payout_sum <= pred.pot());
    }

    #[test]
    fn resolve_to_empty_side_pays_nothing() {
        let mut pred = p("open");
        pred.place_bet("u1", Side::Left, 500, 5000, 1, 0).unwrap();
        let payouts = pred.resolve(Side::Right); // nobody bet right
        assert!(payouts.is_empty());
        assert_eq!(pred.status, Status::Resolved);
        assert_eq!(pred.winner, Some(Side::Right));
    }

    // ── 6. Refund on cancel ───────────────────────────────────────────

    #[test]
    fn cancel_refunds_every_bet_in_full() {
        let mut pred = p("open");
        pred.place_bet("u1", Side::Left, 1000, 5000, 1, 0).unwrap();
        pred.place_bet("u1", Side::Right, 2000, 5000, 1, 0).unwrap();
        pred.place_bet("u2", Side::Left, 3000, 5000, 1, 0).unwrap();
        assert_eq!(pred.pot(), 6000);

        let refunds = pred.cancel();
        assert_eq!(pred.status, Status::Cancelled);
        assert_eq!(pred.winner, None);

        let mut by_user: HashMap<&str, i64> =
            refunds.iter().map(|r| (r.user.as_str(), r.amount)).collect();
        assert_eq!(by_user.remove("u1"), Some(3000)); // 1000 + 2000
        assert_eq!(by_user.remove("u2"), Some(3000));
        assert!(by_user.is_empty());

        let refund_sum: i64 = refunds.iter().map(|r| r.amount).sum();
        assert_eq!(refund_sum, 6000);
        assert_eq!(refund_sum, pred.pot());
    }

    #[test]
    fn cancel_with_no_bets_refunds_nothing() {
        let mut pred = p("open");
        let refunds = pred.cancel();
        assert!(refunds.is_empty());
        assert_eq!(pred.status, Status::Cancelled);
    }

    // ── uuid7 generator ───────────────────────────────────────────────

    #[test]
    fn uuid7_is_well_formed() {
        let id = new_uuid7();
        assert_eq!(id.len(), 36);
        // "xxxxxxxx-xxxx-Mxxx-Nxxx-xxxxxxxxxxxx": version nibble at index 14,
        // variant nibble at index 19 (RFC 4122).
        assert_eq!(&id[14..15], "7");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
        assert!(new_uuid7() != new_uuid7());
    }

    // ── Display renderer ──────────────────────────────────────────────

    #[test]
    fn render_open_screen() {
        let s = render_screen(
            "Will we hit 1k?",
            "Yes",
            12000,
            "No",
            8000,
            Status::Open,
            None,
        );
        assert!(s.contains("Prediction: Will we hit 1k?"));
        assert!(s.contains("Left  [Yes]"));
        assert!(s.contains("12000"));
        assert!(s.contains("Right [No]"));
        assert!(s.contains("8000"));
        assert!(s.contains("Pot: 20000"));
        assert!(!s.contains("WINS"));
        // The majority side (left, 12000 of 20000 pot) fills more than the
        // minority. The two bars have equal segment counts, so compare the
        // filled █ count of the left bar against the right bar's.
        let left_line = s.lines().find(|l| l.starts_with("Left  [Yes]")).unwrap();
        let right_line = s.lines().find(|l| l.starts_with("Right [No]")).unwrap();
        assert!(left_line.matches('█').count() > right_line.matches('█').count());
        assert_eq!(left_line.matches('█').count() + right_line.matches('█').count(), 40);
    }

    #[test]
    fn render_resolved_highlights_winner() {
        let s = render_screen(
            "Will we hit 1k?",
            "Yes",
            12000,
            "No",
            8000,
            Status::Resolved,
            Some(Side::Left),
        );
        assert!(s.contains(">>> Left [Yes] WINS — 20000 points split <<<"));
    }

    #[test]
    fn render_cancelled() {
        let s = render_screen("x", "Yes", 100, "No", 50, Status::Cancelled, None);
        assert!(s.contains(">>> prediction cancelled — refunded <<<"));
    }

    #[test]
    fn render_idle_screen() {
        let s = render_idle();
        assert!(s.contains("No active prediction."));
    }

    // ── 7. Poll command parsing ────────────────────────────────────────

    fn poll(opt: &[&str]) -> Poll {
        Poll::new(
            "poll-1".to_string(),
            "Best game?".to_string(),
            opt.iter().map(|s| s.to_string()).collect(),
            false,
        )
    }

    #[test]
    fn poll_create_parse() {
        assert_eq!(
            parse_poll_command(
                "!poll -p Best game? -1 A -2 B",
                "!",
                "poll",
                &pairs(&[("p", "Best game?"), ("1", "A"), ("2", "B")]),
            ),
            PollCommand::Create {
                prompt: "Best game?".to_string(),
                options: vec!["A".to_string(), "B".to_string()],
                hide_counts: false,
            }
        );
    }

    #[test]
    fn poll_create_recovers_multi_word_prompt_from_raw_text() {
        // The engine's flag parser assigns a flag ONE token, so a real message
        // `!poll -p Will we hit 1k? -1 Yes -2 No` arrives with flags = [("p",
        // "Will"), ("1", "Yes"), ("2", "No")] and the raw text intact. The
        // prompt must recover "Will we hit 1k?" from the raw tokens, not just
        // the single flag value "Will".
        assert_eq!(
            parse_poll_command(
                "!poll -p Will we hit 1k? -1 Yes -2 No",
                "!",
                "poll",
                &pairs(&[("p", "Will"), ("1", "Yes"), ("2", "No")]),
            ),
            PollCommand::Create {
                prompt: "Will we hit 1k?".to_string(),
                options: vec!["Yes".to_string(), "No".to_string()],
                hide_counts: false,
            }
        );
    }

    #[test]
    fn poll_create_prompt_ignores_option_and_hide_flags() {
        // A prompt with the hide flag and numeric option flags interspersed
        // still recovers the full free text and drops the flags.
        assert_eq!(
            parse_poll_command(
                "!poll -p Which map -h -1 Dust2 -2 Mirage -3 Inferno",
                "!",
                "poll",
                &pairs(&[("p", "Which"), ("1", "Dust2"), ("2", "Mirage"), ("3", "Inferno"), ("h", "")]),
            ),
            PollCommand::Create {
                prompt: "Which map".to_string(),
                options: vec!["Dust2".to_string(), "Mirage".to_string(), "Inferno".to_string()],
                hide_counts: true,
            }
        );
    }

    #[test]
    fn poll_create_up_to_six_options_in_order() {
        let labels = ["A", "B", "C", "D", "E", "F"];
        let flags = pairs(&[
            ("p", "Q"),
            ("1", "A"),
            ("2", "B"),
            ("3", "C"),
            ("4", "D"),
            ("5", "E"),
            ("6", "F"),
        ]);
        assert_eq!(
            parse_poll_command(
                "!poll -p Q -1 A -2 B -3 C -4 D -5 E -6 F",
                "!",
                "poll",
                &flags,
            ),
            PollCommand::Create {
                prompt: "Q".to_string(),
                options: labels.iter().map(|s| s.to_string()).collect(),
                hide_counts: false,
            }
        );
    }

    #[test]
    fn poll_create_fewer_than_two_options_is_none() {
        assert_eq!(
            parse_poll_command(
                "!poll -p Q -1 A",
                "!",
                "poll",
                &pairs(&[("p", "Q"), ("1", "A")]),
            ),
            PollCommand::None
        );
    }

    #[test]
    fn poll_create_hide_counts() {
        assert_eq!(
            parse_poll_command(
                "!poll -p Q -1 A -2 B -h",
                "!",
                "poll",
                &pairs(&[("p", "Q"), ("1", "A"), ("2", "B"), ("h", "")]),
            ),
            PollCommand::Create {
                prompt: "Q".to_string(),
                options: vec!["A".to_string(), "B".to_string()],
                hide_counts: true,
            }
        );
    }

    #[test]
    fn poll_vote_parse() {
        assert_eq!(
            parse_poll_command("!poll -1", "!", "poll", &pairs(&[("1", "")])),
            PollCommand::Vote { option_index: 0 }
        );
        assert_eq!(
            parse_poll_command("!poll -6", "!", "poll", &pairs(&[("6", "")])),
            PollCommand::Vote { option_index: 5 }
        );
        assert_eq!(
            parse_poll_command("!poll -7", "!", "poll", &pairs(&[("7", "")])),
            PollCommand::None
        );
    }

    #[test]
    fn poll_stop_parse() {
        assert_eq!(
            parse_poll_command("!poll stop", "!", "poll", &[]),
            PollCommand::Stop
        );
    }

    #[test]
    fn poll_bare_invocation_is_none() {
        assert_eq!(parse_poll_command("!poll", "!", "poll", &[]), PollCommand::None);
        assert_eq!(
            parse_poll_command("hello there", "!", "poll", &[]),
            PollCommand::None
        );
    }

    // ── 8. Poll voting ─────────────────────────────────────────────────

    #[test]
    fn poll_cast_vote_records_and_increments() {
        let mut p = poll(&["A", "B"]);
        assert_eq!(p.cast_vote("u1", 0), Ok(0));
        assert_eq!(p.cast_vote("u2", 1), Ok(1));
        assert_eq!(p.votes, vec![1, 1]);
        assert_eq!(p.total(), 2);
        assert_eq!(p.voters["u1"], 0);
        assert_eq!(p.voters["u2"], 1);
    }

    #[test]
    fn poll_cast_vote_out_of_range_is_invalid_option() {
        let mut p = poll(&["A", "B"]);
        assert_eq!(p.cast_vote("u1", 2), Err(PollError::InvalidOption));
        assert_eq!(p.cast_vote("u1", 99), Err(PollError::InvalidOption));
    }

    #[test]
    fn poll_cast_vote_repeat_keeps_first() {
        let mut p = poll(&["A", "B"]);
        assert_eq!(p.cast_vote("u1", 0), Ok(0));
        assert_eq!(p.cast_vote("u1", 1), Err(PollError::AlreadyVoted));
        assert_eq!(p.votes, vec![1, 0]);
        assert_eq!(p.total(), 1);
        assert_eq!(p.voters["u1"], 0); // first vote kept
    }

    // ── 9. Poll close / winner ─────────────────────────────────────────

    #[test]
    fn poll_close_picks_strict_winner() {
        let mut p = poll(&["A", "B", "C"]);
        for (u, opt) in [("u1", 0), ("u2", 0), ("u3", 0), ("u4", 0), ("u5", 0)] {
            p.cast_vote(u, opt).unwrap();
        }
        p.cast_vote("u6", 1).unwrap();
        p.cast_vote("u7", 1).unwrap();
        p.cast_vote("u8", 2).unwrap();
        assert_eq!(p.close(), Some(0));
        assert_eq!(p.status, PollStatus::Closed);
        assert_eq!(p.winner_index, 0);
    }

    #[test]
    fn poll_close_top_tie_has_no_winner() {
        let mut p = poll(&["A", "B", "C"]);
        for (u, opt) in [("u1", 0), ("u2", 0), ("u3", 0)] {
            p.cast_vote(u, opt).unwrap();
        }
        for (u, opt) in [("u4", 1), ("u5", 1), ("u6", 1)] {
            p.cast_vote(u, opt).unwrap();
        }
        p.cast_vote("u7", 2).unwrap();
        assert_eq!(p.close(), None);
        assert_eq!(p.status, PollStatus::Closed);
        assert_eq!(p.winner_index, -1);
    }

    #[test]
    fn poll_close_all_zero_has_no_winner() {
        let mut p = poll(&["A", "B", "C"]);
        assert_eq!(p.close(), None);
        assert_eq!(p.status, PollStatus::Closed);
        assert_eq!(p.winner_index, -1);
    }

    // ── 10. Poll renderer ──────────────────────────────────────────────

    #[test]
    fn poll_render_open_hidden_shows_question_marks() {
        let s = render_poll_screen(
            "Best game?",
            &["A".to_string(), "B".to_string()],
            &[5, 3],
            8,
            PollStatus::Open,
            -1,
            true,
        );
        assert!(s.contains("Poll: Best game?"));
        assert!(s.contains("1. A : ?"));
        assert!(s.contains("2. B : ?"));
        assert!(!s.contains(": 5"));
        assert!(!s.contains(">>>"));
    }

    #[test]
    fn poll_render_open_shown_shows_counts() {
        let s = render_poll_screen(
            "Best game?",
            &["A".to_string(), "B".to_string()],
            &[5, 3],
            8,
            PollStatus::Open,
            -1,
            false,
        );
        assert!(s.contains("1. A : 5"));
        assert!(s.contains("2. B : 3"));
        assert!(s.contains("Total votes: 8"));
        assert!(!s.contains(">>>"));
    }

    #[test]
    fn poll_render_closed_shows_counts_and_winner() {
        let s = render_poll_screen(
            "Best game?",
            &["A".to_string(), "B".to_string()],
            &[5, 3],
            8,
            PollStatus::Closed,
            0,
            false,
        );
        assert!(s.contains("1. A : 5"));
        assert!(s.contains("2. B : 3"));
        assert!(s.contains(">>> winner: A <<<"));
    }

    #[test]
    fn poll_render_closed_tie_shows_no_winner() {
        let s = render_poll_screen(
            "Best game?",
            &["A".to_string(), "B".to_string()],
            &[3, 3],
            6,
            PollStatus::Closed,
            -1,
            false,
        );
        assert!(s.contains(">>> no winner (tie) <<<"));
    }

    #[test]
    fn poll_render_idle_screen() {
        let s = render_poll_idle();
        assert!(s.contains("No active poll."));
    }
}