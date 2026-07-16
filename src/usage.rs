// Fetching and modeling Claude Code subscription usage.
//
// Talks to the (undocumented) `GET https://api.anthropic.com/api/oauth/usage`
// endpoint using the OAuth access token Claude Code itself caches at
// `%USERPROFILE%\.claude\.credentials.json`. This is the same endpoint used
// by community tools such as `claude-code-statusline`; there is no public
// token-refresh flow, so on any failure (expired token, offline, endpoint
// shape change, ...) we fall back to a neutral "unavailable" state instead
// of crashing, and simply try again on the next timer tick.

use chrono::{DateTime, Duration, Local, Utc};
use serde::Deserialize;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const ANTHROPIC_BETA: &str = "oauth-2025-04-20";

/// The weekly ("seven_day") limit window is exactly 7 days long. The usage
/// endpoint only tells us when it *resets* (the end of the window), so the
/// window start -- needed to work out how far through it we currently are --
/// is derived as `resets_at - 7 days`.
const WEEKLY_WINDOW_SECS: i64 = 7 * 24 * 60 * 60;

/// Floor applied to "seconds elapsed into the weekly window" before it's used
/// as the denominator of the run-rate extrapolation. In the first minutes of
/// a fresh window, dividing by a near-zero elapsed fraction turns any tiny
/// bit of usage into an absurd projection (thousands of percent) that flaps
/// wildly on every poll. Flooring the elapsed time at one hour caps that
/// early multiplier at a sane 168x, so the projected value is always present
/// and never nonsensical; once an hour has actually passed it's the exact
/// linear extrapolation with no fudging.
const MIN_ELAPSED_SECS: i64 = 60 * 60;

/// Hard budget, in UTF-16 code units, for the whole tray tooltip.
///
/// Windows' `NOTIFYICONDATAW::szTip` field is 128 units wide, but the shell
/// only *honors* 128 for callers that opt into the Shell v5 (Windows 2000+)
/// behaviour, by sending a correct `cbSize` and/or `NIM_SETVERSION` with
/// `NOTIFYICON_VERSION_4`. `tray-icon` (0.24.x) does neither -- it builds the
/// struct with `..std::mem::zeroed()`, leaving `cbSize` at 0, and never calls
/// `NIM_SETVERSION` -- so we get the *legacy* limit of 64 units including the
/// terminating NUL, i.e. 63 usable characters.
///
/// This was not theoretical: 0.5.0's tooltip was 101 characters, so the shell
/// chopped it at exactly 63 and the new "Projected" line rendered as the
/// single word fragment `Pr`. Nothing warned about it -- the crate happily
/// copies up to 128 units and `Shell_NotifyIconW` returns success; the
/// truncation happens silently inside the shell.
///
/// Since the limit is imposed by a dependency's struct initialisation, we
/// can't lift it from here; we live inside it instead, and
/// `tooltip_worst_case_fits_the_shell_limit` fails the build if we stop.
const TOOLTIP_MAX_CHARS: usize = 63;

/// Last-resort guard so a tooltip can never again be silently mangled by the
/// shell: anything over budget is truncated *here*, visibly and on a
/// character boundary, rather than being cut at an arbitrary UTF-16 offset by
/// Windows. Under normal operation this is a no-op -- the formats in
/// `tooltip` are sized to fit, and the tests assert that -- so if this ever
/// actually trims something, that's a bug in the wording, not in this
/// function.
fn fit_tooltip(text: String) -> String {
    if text.encode_utf16().count() <= TOOLTIP_MAX_CHARS {
        return text;
    }

    eprintln!(
        "[claude-usage-widget] tooltip over the {TOOLTIP_MAX_CHARS}-char shell limit, truncating: {text:?}"
    );

    let mut out = String::new();
    let mut units = 0usize;
    for ch in text.chars() {
        let ch_units = ch.len_utf16();
        if units + ch_units > TOOLTIP_MAX_CHARS {
            break;
        }
        out.push(ch);
        units += ch_units;
    }
    out
}

/// Why a fetch attempt failed, used to show an accurate message instead of a
/// single generic "unavailable" string. This matters in practice: the
/// original generic message ("sign-in token expired... run `claude` to
/// refresh") is actively misleading when the real cause is rate-limiting --
/// a user seeing that message during a 429 backoff has no reason to think
/// their session is fine and it'll clear on its own, so they go re-authenticate
/// for no reason (confirmed as a real point of confusion, not hypothetical).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnavailableReason {
    /// The credentials file is missing/unparseable/empty, or the server
    /// itself told us the token is bad (401/403). Actually worth telling the
    /// user to re-run `claude`.
    TokenProblem,
    /// HTTP 429 from the usage endpoint. Transient by nature -- the worker
    /// loop in `main.rs` is already backing off and will recover on its own.
    RateLimited,
    /// The request itself couldn't be sent/completed (DNS, offline, timeout).
    NetworkError,
    /// Anything else (unexpected HTTP status, unparseable response body/
    /// timestamp). Also expected to be transient.
    Other,
}

impl UnavailableReason {
    /// Tooltip text for this reason, worded to fit inside
    /// [`TOOLTIP_MAX_CHARS`] once the "Claude usage: " prefix is added.
    ///
    /// The wording is deliberately terse: the previous, chattier version of
    /// this text ("sign-in token expired or unavailable\nRun `claude` once to
    /// refresh, then this will update.") blew straight past the shell's
    /// tooltip limit and got truncated mid-word, silently eating the only
    /// part the user actually needed -- the instruction to run `claude`.
    /// Every variant other than `TokenProblem` still says it's retrying, so
    /// it's clear no user action is needed.
    fn tooltip_label(self) -> &'static str {
        match self {
            UnavailableReason::TokenProblem => "sign-in needed\nRun `claude` once to refresh.",
            UnavailableReason::RateLimited => "rate-limited\nClears on its own -- retrying.",
            UnavailableReason::NetworkError => "network error\nRetrying automatically.",
            UnavailableReason::Other => "unavailable\nRetrying automatically.",
        }
    }

    /// Very short (fits a single menu line) version of `label`.
    fn short_label(self) -> &'static str {
        match self {
            UnavailableReason::TokenProblem => "sign-in needed",
            UnavailableReason::RateLimited => "rate-limited, retrying",
            UnavailableReason::NetworkError => "network error, retrying",
            UnavailableReason::Other => "unavailable, retrying",
        }
    }
}

#[derive(Deserialize)]
struct CredentialsFile {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: ClaudeAiOauth,
}

#[derive(Deserialize)]
struct ClaudeAiOauth {
    #[serde(rename = "accessToken")]
    access_token: String,
}

#[derive(Deserialize)]
struct UsageResponse {
    five_hour: LimitWindow,
    seven_day: LimitWindow,
    // Deserialized as a loose `Value` (rather than `Option<ExtraUsage>`
    // directly) so that *any* shape change or removal of this field by
    // Anthropic -- not just its outright absence -- can never break parsing
    // of `five_hour`/`seven_day` above. See `parse_extra_usage`.
    #[serde(default)]
    extra_usage: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct LimitWindow {
    // The live endpoint sends this as a JSON float (e.g. `3.0`, `54.0`)
    // even though it's conceptually an integer percentage, so we parse it
    // as f64 and round for display rather than risk a hard deserialize
    // failure on a non-integer value.
    utilization: f64,
    resets_at: String,
}

/// The optional extra-usage/credit-balance block. `monthly_limit` and
/// `used_credits` are conceptually integer minor units (e.g. cents); divide
/// by `10^decimal_places` to get a display value. Like `LimitWindow::
/// utilization` elsewhere in this file, the live endpoint sends these as
/// JSON floats (e.g. `0.0`) even for whole numbers, so they're parsed as
/// f64 rather than risking a deserialize failure on a non-integer value.
///
/// Every field defaults so that a partially-shaped (rather than fully
/// missing) `extra_usage` object still parses instead of failing outright.
#[derive(Deserialize, Clone)]
struct ExtraUsage {
    #[serde(default)]
    is_enabled: bool,
    #[serde(default)]
    monthly_limit: f64,
    #[serde(default)]
    used_credits: f64,
    #[serde(default)]
    currency: String,
    #[serde(default)]
    decimal_places: u32,
}

/// Display-ready extra usage/credit balance info. Only produced when the
/// account has extra usage enabled and the response was parseable.
#[derive(Clone)]
pub struct ExtraUsageInfo {
    pub pct: u32,
    pub used: f64,
    pub limit: f64,
    pub currency: String,
}

/// One data point (percentage + human-readable reset label) for a single
/// usage window, used by the threshold-notification logic in `main.rs`.
pub struct WindowUsage {
    pub pct: u32,
    pub resets_label: String,
}

/// Extrapolated end-of-week weekly utilization: if you keep burning tokens at
/// the average rate seen so far this weekly window, this is roughly where
/// utilization lands when the window resets. `projected_pct` can (and, when
/// you're on pace to blow the limit, will) exceed 100 -- that's the whole
/// point of showing it, so it is deliberately NOT clamped to 100 the way the
/// live utilization percentages are.
#[derive(Clone, Copy)]
pub struct WeeklyProjection {
    /// Projected utilization at reset, in percent. May exceed 100 (capped at
    /// a sane 999 only to avoid an absurd label in the first hour).
    pub projected_pct: u32,
    /// True when on pace to exceed the weekly limit (`projected_pct > 100`).
    pub over_limit: bool,
}

impl WeeklyProjection {
    /// "under" / "over" tag for the projection, matching the user's mental
    /// model of "83% -> underuse, room to spare" vs "123% -> overuse, pace
    /// down". Kept to a single short word so it fits the tray tooltip line,
    /// the menu line and the drawn panel bar caption identically.
    pub fn tag(self) -> &'static str {
        if self.over_limit { "over" } else { "under" }
    }
}

/// Linearly extrapolates end-of-window weekly utilization from the current
/// utilization and how far through the 7-day window we are. Factored out as a
/// pure function of its three inputs so the run-rate math can be unit-tested
/// without a live endpoint or a real clock.
fn project_weekly(
    current_pct: u32,
    seven_day_resets: DateTime<Utc>,
    now: DateTime<Utc>,
) -> WeeklyProjection {
    let window_start = seven_day_resets - Duration::seconds(WEEKLY_WINDOW_SECS);
    // Clamp into `[MIN_ELAPSED_SECS, WEEKLY_WINDOW_SECS]`: the lower bound
    // tames first-hour noise (see `MIN_ELAPSED_SECS`) and also absorbs the
    // pathological "now is before the window even started" clock-skew case;
    // the upper bound means once the window has fully elapsed the projection
    // just equals the current utilization.
    let elapsed = (now - window_start)
        .num_seconds()
        .clamp(MIN_ELAPSED_SECS, WEEKLY_WINDOW_SECS);
    let fraction = elapsed as f64 / WEEKLY_WINDOW_SECS as f64;
    let projected = (current_pct as f64 / fraction).round().clamp(0.0, 999.0) as u32;
    WeeklyProjection {
        projected_pct: projected,
        over_limit: projected > 100,
    }
}

/// Leniently converts the raw `extra_usage` JSON (if any) into display-ready
/// info. Never propagates a parse error to the caller -- an unparseable or
/// disabled block just means "don't show this line", not "usage data is
/// broken".
fn parse_extra_usage(value: &Option<serde_json::Value>) -> Option<ExtraUsageInfo> {
    let value = value.clone()?;
    let extra: ExtraUsage = match serde_json::from_value(value) {
        Ok(extra) => extra,
        Err(e) => {
            eprintln!("[claude-usage-widget] ignoring unparseable extra_usage field: {e}");
            return None;
        }
    };

    if !extra.is_enabled {
        return None;
    }

    // Defensive clamp: a garbage decimal_places value shouldn't be able to
    // blow up the power-of-ten scale below.
    let decimal_places = extra.decimal_places.min(10);
    let scale = 10f64.powi(decimal_places as i32);
    let used = extra.used_credits / scale;
    let limit = extra.monthly_limit / scale;
    let pct = if limit > 0.0 {
        ((used / limit) * 100.0).round().clamp(0.0, 100.0) as u32
    } else {
        0
    };

    Some(ExtraUsageInfo {
        pct,
        used,
        limit,
        currency: extra.currency,
    })
}

/// The state driving the tray icon, tooltip and menu text.
#[derive(Clone)]
pub enum TrayState {
    Ok {
        five_hour_pct: u32,
        five_hour_resets: DateTime<Utc>,
        seven_day_pct: u32,
        seven_day_resets: DateTime<Utc>,
        extra_usage: Option<ExtraUsageInfo>,
    },
    /// Data could not be obtained. `detail` is logged to stderr (not shown
    /// verbatim in the UI); `reason` drives what the UI actually says.
    Unavailable {
        #[allow(dead_code)]
        detail: String,
        reason: UnavailableReason,
    },
}

impl TrayState {
    /// The failure category, when this is an `Unavailable` state. Used by
    /// the polling loop in `main.rs` to pick a backoff schedule: only
    /// `RateLimited` warrants the long, cautious exponential backoff --
    /// transient connectivity blips (e.g. stale connections after the
    /// machine sleeps/wakes) recover far faster and shouldn't make the user
    /// wait minutes to find out.
    pub fn unavailable_reason(&self) -> Option<UnavailableReason> {
        match self {
            TrayState::Unavailable { reason, .. } => Some(*reason),
            TrayState::Ok { .. } => None,
        }
    }

    /// Highest of the two utilization percentages, used to pick the tray
    /// icon color. `None` when data is unavailable (renders gray).
    pub fn max_utilization(&self) -> Option<u32> {
        match self {
            TrayState::Ok {
                five_hour_pct,
                seven_day_pct,
                ..
            } => Some((*five_hour_pct).max(*seven_day_pct)),
            TrayState::Unavailable { .. } => None,
        }
    }

    /// (`five_hour`, `seven_day`) usage, when available. Used by the
    /// threshold-notification logic in `main.rs`.
    pub fn windows(&self) -> Option<(WindowUsage, WindowUsage)> {
        match self {
            TrayState::Ok {
                five_hour_pct,
                five_hour_resets,
                seven_day_pct,
                seven_day_resets,
                ..
            } => {
                let now = Utc::now();
                Some((
                    WindowUsage {
                        pct: *five_hour_pct,
                        resets_label: format_relative(*five_hour_resets, now),
                    },
                    WindowUsage {
                        pct: *seven_day_pct,
                        resets_label: format_weekday_time(*seven_day_resets),
                    },
                ))
            }
            TrayState::Unavailable { .. } => None,
        }
    }

    /// Extrapolated end-of-week weekly utilization, when usage data is
    /// available. `None` in the `Unavailable` state (nothing to project
    /// from). Used by the tooltip's/menu's/panel's third "projected" element.
    pub fn weekly_projection(&self) -> Option<WeeklyProjection> {
        match self {
            TrayState::Ok {
                seven_day_pct,
                seven_day_resets,
                ..
            } => Some(project_weekly(*seven_day_pct, *seven_day_resets, Utc::now())),
            TrayState::Unavailable { .. } => None,
        }
    }

    /// Short label describing why data is unavailable, when it is. Used by
    /// the floating usage panel (`panel.rs`) so it shows the same accurate
    /// reason as the tray tooltip/menu instead of a generic message.
    pub fn unavailable_short_label(&self) -> Option<&'static str> {
        match self {
            TrayState::Unavailable { reason, .. } => Some(reason.short_label()),
            TrayState::Ok { .. } => None,
        }
    }

    /// Text for the "Projected" at-a-glance menu line -- the run-rate
    /// extrapolation of where weekly utilization lands at reset. Mirrors the
    /// tooltip's third line and the panel's third bar. `None` when data is
    /// unavailable (the caller shows the same short reason on the weekly line
    /// already, so an extra "projected: unavailable" line would just be
    /// noise).
    pub fn projected_menu_line(&self, bar_width: usize) -> Option<String> {
        let projection = self.weekly_projection()?;
        Some(format!(
            "Projected [{}] {}%  ({})",
            bar(projection.projected_pct, bar_width),
            projection.projected_pct,
            projection.tag()
        ))
    }

    /// Text for the third, informational "extra usage / credit balance"
    /// menu line. `None` when unavailable or (the common case) not enabled
    /// on this account -- the caller should not show a menu line at all then.
    pub fn extra_usage_line(&self, bar_width: usize) -> Option<String> {
        match self {
            TrayState::Ok {
                extra_usage: Some(extra),
                ..
            } => Some(format!(
                "Extra usage  [{}] {}%  {:.2}/{:.2} {}",
                bar(extra.pct, bar_width),
                extra.pct,
                extra.used,
                extra.limit,
                extra.currency
            )),
            _ => None,
        }
    }

    /// Short (3 line) tooltip shown on hover.
    ///
    /// Every line here is written to a hard 63-character budget; see
    /// [`TOOLTIP_MAX_CHARS`] for why that number, and
    /// `tooltip_worst_case_fits_the_shell_limit` for the test that keeps it
    /// honest. Changing the wording without re-checking that budget is how
    /// the "Projected" line got silently eaten in 0.5.0.
    pub fn tooltip(&self) -> String {
        let text = match self {
            TrayState::Unavailable { reason, .. } => {
                format!("Claude usage: {}", reason.tooltip_label())
            }
            TrayState::Ok {
                five_hour_pct,
                five_hour_resets,
                seven_day_pct,
                seven_day_resets,
                ..
            } => {
                let now = Utc::now();
                let projection = project_weekly(*seven_day_pct, *seven_day_resets, now);
                format!(
                    "Session {}% {}\nWeekly {}% {}\nProjected {}% {}",
                    five_hour_pct,
                    format_relative(*five_hour_resets, now),
                    seven_day_pct,
                    format_weekday_time(*seven_day_resets),
                    projection.projected_pct,
                    projection.tag()
                )
            }
        };
        fit_tooltip(text)
    }

    /// Text for the two disabled "at a glance" menu entries: (session, weekly).
    pub fn menu_lines(&self, bar_width: usize) -> (String, String) {
        match self {
            TrayState::Unavailable { reason, .. } => (
                format!("Session  {}", reason.short_label()),
                format!("Weekly   {}", reason.short_label()),
            ),
            TrayState::Ok {
                five_hour_pct,
                five_hour_resets,
                seven_day_pct,
                seven_day_resets,
                ..
            } => {
                let now = Utc::now();
                let session = format!(
                    "Session  [{}] {}%  resets {}",
                    bar(*five_hour_pct, bar_width),
                    five_hour_pct,
                    format_relative(*five_hour_resets, now)
                );
                let weekly = format!(
                    "Weekly   [{}] {}%  resets {}",
                    bar(*seven_day_pct, bar_width),
                    seven_day_pct,
                    format_weekday_time(*seven_day_resets)
                );
                (session, weekly)
            }
        }
    }
}

/// Result of one [`fetch_state`] call: the state to show, plus (only set
/// when the last HTTP attempt failed with 429 and sent a parseable
/// `Retry-After` header) how long the server asked us to wait before trying
/// again. The polling/backoff loop in `main.rs` is the only consumer of the
/// latter.
pub struct FetchOutcome {
    pub state: TrayState,
    pub retry_after_secs: Option<u64>,
}

/// Internal fetch error, carrying the category the UI should show plus an
/// optional `Retry-After` hint alongside the usual human-readable message.
struct FetchError {
    message: String,
    reason: UnavailableReason,
    retry_after_secs: Option<u64>,
}

impl From<String> for FetchError {
    /// Used for errors that don't have a more specific category (e.g. an
    /// unparseable response body) -- defaults to `Other`.
    fn from(message: String) -> Self {
        FetchError {
            message,
            reason: UnavailableReason::Other,
            retry_after_secs: None,
        }
    }
}

/// Reads the cached token and fetches current usage, collapsing every
/// failure mode into `TrayState::Unavailable` so the caller never has to
/// handle an error (and the app never crashes on a bad response).
pub fn fetch_state(client: &reqwest::blocking::Client) -> FetchOutcome {
    let token = match read_access_token() {
        Ok(token) => token,
        Err(detail) => {
            eprintln!("[claude-usage-widget] {detail}");
            return FetchOutcome {
                state: TrayState::Unavailable {
                    detail,
                    reason: UnavailableReason::TokenProblem,
                },
                retry_after_secs: None,
            };
        }
    };

    match fetch_usage(client, &token) {
        Ok(response) => match to_state(response) {
            Some(state) => FetchOutcome {
                state,
                retry_after_secs: None,
            },
            None => {
                let detail = "usage response had an unparseable reset timestamp".to_string();
                eprintln!("[claude-usage-widget] {detail}");
                FetchOutcome {
                    state: TrayState::Unavailable {
                        detail,
                        reason: UnavailableReason::Other,
                    },
                    retry_after_secs: None,
                }
            }
        },
        Err(e) => {
            eprintln!("[claude-usage-widget] {}", e.message);
            FetchOutcome {
                state: TrayState::Unavailable {
                    detail: e.message,
                    reason: e.reason,
                },
                retry_after_secs: e.retry_after_secs,
            }
        }
    }
}

fn to_state(response: UsageResponse) -> Option<TrayState> {
    let five_hour_resets = parse_reset(&response.five_hour.resets_at)?;
    let seven_day_resets = parse_reset(&response.seven_day.resets_at)?;
    let extra_usage = parse_extra_usage(&response.extra_usage);
    Some(TrayState::Ok {
        five_hour_pct: to_percent(response.five_hour.utilization),
        five_hour_resets,
        seven_day_pct: to_percent(response.seven_day.utilization),
        seven_day_resets,
        extra_usage,
    })
}

/// Rounds a raw utilization value to a display-friendly 0-100 percentage.
fn to_percent(raw: f64) -> u32 {
    raw.round().clamp(0.0, 100.0) as u32
}

fn parse_reset(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn read_access_token() -> Result<String, String> {
    let profile_dir = std::env::var("USERPROFILE")
        .map_err(|_| "USERPROFILE environment variable is not set".to_string())?;
    let path = std::path::Path::new(&profile_dir)
        .join(".claude")
        .join(".credentials.json");

    let contents = std::fs::read_to_string(&path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;

    let parsed: CredentialsFile = serde_json::from_str(&contents)
        .map_err(|e| format!("could not parse {}: {e}", path.display()))?;

    let token = parsed.claude_ai_oauth.access_token.trim().to_string();
    if token.is_empty() {
        return Err(format!("{} has an empty access token", path.display()));
    }
    Ok(token)
}

fn fetch_usage(
    client: &reqwest::blocking::Client,
    token: &str,
) -> Result<UsageResponse, FetchError> {
    let response = client
        .get(USAGE_URL)
        .header("Authorization", format!("Bearer {token}"))
        .header("anthropic-beta", ANTHROPIC_BETA)
        .header("Content-Type", "application/json")
        .send()
        .map_err(|e| FetchError {
            message: format!("network error calling usage endpoint: {e}"),
            reason: UnavailableReason::NetworkError,
            retry_after_secs: None,
        })?;

    let status = response.status();
    if !status.is_success() {
        // This undocumented endpoint has been observed rate-limiting overly
        // frequent polling with HTTP 429; when it does, respect any
        // `Retry-After` hint it sends rather than guessing purely from our
        // own backoff schedule. 401/403 genuinely mean the token itself is
        // bad, which -- unlike a 429 -- really does need the user to
        // re-authenticate, so it's categorized separately.
        let code = status.as_u16();
        let (reason, retry_after_secs) = match code {
            429 => (UnavailableReason::RateLimited, parse_retry_after(&response)),
            401 | 403 => (UnavailableReason::TokenProblem, None),
            _ => (UnavailableReason::Other, None),
        };
        return Err(FetchError {
            message: format!("usage endpoint returned HTTP {status}"),
            reason,
            retry_after_secs,
        });
    }

    response
        .json::<UsageResponse>()
        .map_err(|e| format!("could not parse usage endpoint response: {e}").into())
}

/// Simple integer-seconds parsing of the `Retry-After` header -- the only
/// form this endpoint is expected to send, if it sends one at all.
/// Deliberately does not attempt full HTTP-date parsing for the rarer
/// `Retry-After: <date>` form; callers fall back to the computed exponential
/// backoff instead when this returns `None`.
fn parse_retry_after(response: &reqwest::blocking::Response) -> Option<u64> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

/// "3h40m" / "40m" / "now" style duration until `reset` as seen from `now`.
fn format_relative(reset: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let remaining = reset - now;
    if remaining.num_seconds() <= 0 {
        return "now".to_string();
    }
    let total_minutes = remaining.num_minutes();
    let hours = total_minutes / 60;
    let minutes = total_minutes % 60;
    if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else {
        format!("{minutes}m")
    }
}

/// "Wed 18:00" style local weekday/time for a reset that may be days away.
fn format_weekday_time(reset: DateTime<Utc>) -> String {
    reset.with_timezone(&Local).format("%a %H:%M").to_string()
}

/// Renders a Unicode block-character progress bar, e.g. `██████░░░░`.
fn bar(pct: u32, width: usize) -> String {
    let pct = pct.min(100) as f64;
    let filled = ((pct / 100.0) * width as f64).round() as usize;
    let filled = filled.min(width);
    let empty = width - filled;
    format!("{}{}", "\u{2588}".repeat(filled), "\u{2591}".repeat(empty))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resets_in(secs_from_now: i64, now: DateTime<Utc>) -> DateTime<Utc> {
        now + Duration::seconds(secs_from_now)
    }

    #[test]
    fn projection_scales_current_usage_by_time_remaining() {
        let now = Utc::now();
        // Exactly halfway through the week (reset is 3.5 days out), 40% used.
        // Run-rate says we land at ~80% by reset -> "under".
        let resets = resets_in(WEEKLY_WINDOW_SECS / 2, now);
        let p = project_weekly(40, resets, now);
        assert_eq!(p.projected_pct, 80);
        assert!(!p.over_limit);
        assert_eq!(p.tag(), "under");
    }

    #[test]
    fn projection_flags_overuse_when_on_pace_to_exceed() {
        let now = Utc::now();
        // Halfway through the week but already at 70% -> projects to 140%.
        let resets = resets_in(WEEKLY_WINDOW_SECS / 2, now);
        let p = project_weekly(70, resets, now);
        assert_eq!(p.projected_pct, 140);
        assert!(p.over_limit);
        assert_eq!(p.tag(), "over");
    }

    #[test]
    fn projection_at_end_of_window_equals_current_usage() {
        let now = Utc::now();
        // Reset is imminent (1 second away) -> the window has essentially
        // fully elapsed, so the projection should just be the current value.
        let resets = resets_in(1, now);
        let p = project_weekly(63, resets, now);
        assert_eq!(p.projected_pct, 63);
        assert!(!p.over_limit);
    }

    #[test]
    fn first_hour_noise_is_capped_not_absurd() {
        let now = Utc::now();
        // 5% used only a few minutes into a fresh window. Without the
        // MIN_ELAPSED_SECS floor this would extrapolate to many thousands of
        // percent; with it, the denominator is pinned at 1 hour of 168, so
        // 5% * 168 = 840% -- high (correctly screaming "slow down") but not
        // nonsensical, and never above the 999 clamp.
        let resets = resets_in(WEEKLY_WINDOW_SECS - 300, now);
        let p = project_weekly(5, resets, now);
        assert_eq!(p.projected_pct, 840);
        assert!(p.over_limit);
    }

    #[test]
    fn projection_is_clamped_to_999() {
        let now = Utc::now();
        let resets = resets_in(WEEKLY_WINDOW_SECS - 60, now);
        let p = project_weekly(100, resets, now);
        assert_eq!(p.projected_pct, 999);
    }

    #[test]
    fn zero_usage_projects_to_zero() {
        let now = Utc::now();
        let resets = resets_in(WEEKLY_WINDOW_SECS / 4, now);
        let p = project_weekly(0, resets, now);
        assert_eq!(p.projected_pct, 0);
        assert!(!p.over_limit);
        assert_eq!(p.tag(), "under");
    }

    fn utf16_len(s: &str) -> usize {
        s.encode_utf16().count()
    }

    /// The widest tooltip the `Ok` state can physically produce: three-digit
    /// percentages on both windows, a full "4h59m" session countdown, and a
    /// projection pinned at the 999 clamp.
    fn worst_case_ok_state() -> TrayState {
        let now = Utc::now();
        TrayState::Ok {
            five_hour_pct: 100,
            five_hour_resets: now + Duration::minutes(4 * 60 + 59),
            seven_day_pct: 100,
            // A weekly window only `MIN_ELAPSED_SECS` old with 100% already
            // burned extrapolates past the clamp, giving the widest possible
            // projected value (999).
            seven_day_resets: now + Duration::seconds(WEEKLY_WINDOW_SECS - MIN_ELAPSED_SECS),
            extra_usage: None,
        }
    }

    /// Regression test for the 0.5.0 "tooltip shows only `Pr`" bug.
    ///
    /// Windows silently truncates the tray tooltip at `TOOLTIP_MAX_CHARS`
    /// (see that constant for the full why), and nothing in the crate, the
    /// API return value, or the compiler warns about it -- the only symptom
    /// is a mangled string on screen, which no automated check would have
    /// caught. So the budget gets asserted here instead.
    #[test]
    fn tooltip_worst_case_fits_the_shell_limit() {
        let tip = worst_case_ok_state().tooltip();
        assert!(
            utf16_len(&tip) <= TOOLTIP_MAX_CHARS,
            "tooltip is {} units, over the {TOOLTIP_MAX_CHARS} limit: {tip:?}",
            utf16_len(&tip)
        );
    }

    /// The whole point of the tooltip fix: the projected line must survive
    /// intact, not as the `Pr` fragment the shell used to leave behind.
    #[test]
    fn tooltip_keeps_the_whole_projected_line() {
        let tip = worst_case_ok_state().tooltip();
        assert!(tip.contains("Projected 999%"), "projected line missing: {tip:?}");
        assert!(tip.contains("Session"), "session line missing: {tip:?}");
        assert!(tip.contains("Weekly"), "weekly line missing: {tip:?}");
        // Three lines, none of them empty or half-eaten.
        assert_eq!(tip.lines().count(), 3, "expected 3 tooltip lines: {tip:?}");
    }

    /// The `Unavailable` tooltips were over budget too -- `TokenProblem`'s
    /// wording got cut mid-word, eating the "run `claude`" instruction that
    /// was the only actionable part of the message.
    #[test]
    fn unavailable_tooltips_fit_the_shell_limit() {
        for reason in [
            UnavailableReason::TokenProblem,
            UnavailableReason::RateLimited,
            UnavailableReason::NetworkError,
            UnavailableReason::Other,
        ] {
            let tip = TrayState::Unavailable {
                detail: "irrelevant to the tooltip".to_string(),
                reason,
            }
            .tooltip();
            assert!(
                utf16_len(&tip) <= TOOLTIP_MAX_CHARS,
                "{reason:?} tooltip is {} units, over the {TOOLTIP_MAX_CHARS} limit: {tip:?}",
                utf16_len(&tip)
            );
        }

        // The instruction that used to be truncated away must be complete.
        let tip = TrayState::Unavailable {
            detail: String::new(),
            reason: UnavailableReason::TokenProblem,
        }
        .tooltip();
        assert!(tip.contains("Run `claude` once to refresh."), "{tip:?}");
    }

    #[test]
    fn fit_tooltip_passes_short_text_through_untouched() {
        let short = "Session 42% 3h12m".to_string();
        assert_eq!(fit_tooltip(short.clone()), short);
    }

    #[test]
    fn fit_tooltip_truncates_on_a_character_boundary() {
        // Multi-byte characters must not be split; the guard trims whole
        // chars, unlike the shell's blind UTF-16 cut.
        let long = "é".repeat(100);
        let fitted = fit_tooltip(long);
        assert_eq!(utf16_len(&fitted), TOOLTIP_MAX_CHARS);
        assert!(fitted.chars().all(|c| c == 'é'));
    }
}
