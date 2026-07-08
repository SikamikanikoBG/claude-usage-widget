// Fetching and modeling Claude Code subscription usage.
//
// Talks to the (undocumented) `GET https://api.anthropic.com/api/oauth/usage`
// endpoint using the OAuth access token Claude Code itself caches at
// `%USERPROFILE%\.claude\.credentials.json`. This is the same endpoint used
// by community tools such as `claude-code-statusline`; there is no public
// token-refresh flow, so on any failure (expired token, offline, endpoint
// shape change, ...) we fall back to a neutral "unavailable" state instead
// of crashing, and simply try again on the next timer tick.

use chrono::{DateTime, Local, Utc};
use serde::Deserialize;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const ANTHROPIC_BETA: &str = "oauth-2025-04-20";

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
    /// Short tooltip/menu-line text for this reason. Every variant other
    /// than `TokenProblem` explicitly says "retrying automatically" so it's
    /// clear no user action is needed.
    fn label(self) -> &'static str {
        match self {
            UnavailableReason::TokenProblem => {
                "sign-in token expired or unavailable\nRun `claude` once to refresh, then this will update."
            }
            UnavailableReason::RateLimited => {
                "rate-limited by Anthropic right now\nThis clears on its own -- retrying automatically."
            }
            UnavailableReason::NetworkError => {
                "network error reaching Anthropic\nRetrying automatically."
            }
            UnavailableReason::Other => "temporarily unavailable\nRetrying automatically.",
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

    /// Short label describing why data is unavailable, when it is. Used by
    /// the floating usage panel (`panel.rs`) so it shows the same accurate
    /// reason as the tray tooltip/menu instead of a generic message.
    pub fn unavailable_short_label(&self) -> Option<&'static str> {
        match self {
            TrayState::Unavailable { reason, .. } => Some(reason.short_label()),
            TrayState::Ok { .. } => None,
        }
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

    /// Short (~2 line) tooltip shown on hover.
    pub fn tooltip(&self) -> String {
        match self {
            TrayState::Unavailable { reason, .. } => format!("Claude usage: {}", reason.label()),
            TrayState::Ok {
                five_hour_pct,
                five_hour_resets,
                seven_day_pct,
                seven_day_resets,
                ..
            } => {
                let now = Utc::now();
                format!(
                    "Session: {}% (resets in {})\nWeekly: {}% (resets {})",
                    five_hour_pct,
                    format_relative(*five_hour_resets, now),
                    seven_day_pct,
                    format_weekday_time(*seven_day_resets)
                )
            }
        }
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
