// Inference-server monitoring: vLLM and Ollama, as many as you like, each
// with as many tray icons as you like.
//
// Design notes worth keeping in view:
//
// * **Configuration is a JSON file, not the registry.** Every existing
//   setting is a scalar with a fixed name, which is exactly what HKCU is good
//   at. A user-defined *list* of servers, each with a user-defined list of
//   tray icons, is not; it wants to be hand-editable, diffable and
//   copy-pasteable between machines. So this half of the app reads
//   `%LOCALAPPDATA%\ClaudeUsageWidget\servers.json` (right next to the log
//   file, which is the first place anyone already looks when something
//   misbehaves).
//
// * **Everything a provider produces is a flat `metric id -> f64` map.** The
//   tray-icon layer never learns what a vLLM is; it looks up a string id from
//   the config and renders the number. Adding a metric is a line in the
//   catalog plus a line in the provider, with no plumbing in between, and
//   adding a whole new server kind doesn't touch the icon code at all.
//
// * **Rates are computed from counter deltas, here, not by the icons.**
//   Prometheus counters are cumulative-since-start, so a "tokens per second"
//   is always a difference between two scrapes. The subtle part is *which*
//   denominator: dividing by wall-clock time answers "what is this box
//   producing" (near zero whenever it's idle), while dividing by the
//   server's own accumulated decode time answers "how fast does it generate
//   when it is generating" -- the number people actually quote. Both are
//   offered, as `tps_wall` and `tps`.

pub mod ollama;
pub mod prom;
pub mod vllm;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Hard floor on how often any one server is polled. These are LAN/tailnet
/// endpoints on the user's own hardware rather than a rate-limited third-party
/// API, so this is about not wasting cycles rather than about being a good
/// citizen -- hence 2 seconds, not the usage endpoint's 1 minute.
pub const MIN_POLL_SECS: u64 = 2;

/// Backoff after a failed scrape: 5s doubling to 60s. Deliberately the
/// *connectivity* schedule (fast, self-healing) rather than the cautious
/// rate-limit one used for api.anthropic.com -- a laptop that just came off
/// the docking station and can't see `vader` for ten seconds should show a
/// gray icon for ten seconds, not for five minutes.
const BACKOFF_BASE: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Windows tray tooltips truncate at 63 characters, not the 128 the
/// `NOTIFYICONDATAW.szTip` field would suggest -- and they truncate
/// *silently*. See the note in `usage.rs`; this half of the app has to
/// respect the same limit, so every tooltip goes through [`clamp_tooltip`].
pub const MAX_TOOLTIP_CHARS: usize = 63;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerKind {
    Vllm,
    Ollama,
}

impl ServerKind {
    pub fn label(self) -> &'static str {
        match self {
            ServerKind::Vllm => "vLLM",
            ServerKind::Ollama => "Ollama",
        }
    }
}

/// Which silhouette a metric's tray icon is drawn as. Mirrors
/// `icon::BadgeShape`, but lives here so the config file's vocabulary
/// ("circle" / "square" / "hex") is decoupled from the renderer's enum.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum IconShape {
    Circle,
    Square,
    /// The default for server metrics: the two existing icons already own the
    /// circle (Claude usage) and the square (CPU temperature), so a third
    /// silhouette is what keeps a glance at the tray unambiguous.
    #[default]
    Hex,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Short display name; also the key tray icons refer to.
    pub name: String,
    pub kind: ServerKind,
    /// Base URL, e.g. `http://vader:8010`. Paths are appended per provider.
    pub url: String,
    #[serde(default = "default_poll_secs")]
    pub poll_secs: u64,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Name of an environment variable holding a bearer token, for servers
    /// behind auth. Deliberately indirect: the token itself never lands in a
    /// config file that gets copied between machines or pasted into an issue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_env: Option<String>,
    /// Ollama only. See [`ProbeConfig`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<ProbeConfig>,
}

/// Ollama exposes no aggregate metrics at all -- no `/metrics`, no
/// `/debug/vars`; per-request timings exist only inside each response body.
/// So the only way to get a real tokens/sec number out of it is to ask it to
/// generate something and read the timings back.
///
/// That is genuinely load the widget itself creates, so it is off by default
/// and hedged with one non-negotiable rule, enforced in `ollama.rs`: probe
/// only a model that is ALREADY loaded (present in `/api/ps`). Without that
/// rule, a tray widget on a laptop could silently pull tens of gigabytes into
/// a remote box's VRAM just by being open.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProbeConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_probe_interval_secs")]
    pub interval_secs: u64,
    #[serde(default = "default_probe_tokens")]
    pub num_predict: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrayIconConfig {
    /// `ServerConfig::name` this icon reads from.
    pub server: String,
    /// Metric id from the catalog -- see [`metric_catalog`].
    pub metric: String,
    #[serde(default)]
    pub shape: IconShape,
    #[serde(default = "default_true")]
    pub visible: bool,
    /// Optional value at/above which the badge turns amber / red. Left unset,
    /// each metric uses the sensible default in [`MetricDef::thresholds`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crit: Option<f64>,
    /// Overrides the tooltip's metric word. Tooltips are capped at 63 chars,
    /// so a long server name plus a long metric label is a real constraint --
    /// this is the escape hatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub servers: Vec<ServerConfig>,
    #[serde(default)]
    pub tray_icons: Vec<TrayIconConfig>,
}

fn default_true() -> bool {
    true
}
fn default_poll_secs() -> u64 {
    10
}
fn default_timeout_ms() -> u64 {
    2000
}
fn default_probe_interval_secs() -> u64 {
    300
}
fn default_probe_tokens() -> u32 {
    32
}

impl Config {
    /// Sanitises a freshly-loaded (i.e. hand-edited) config: clamps intervals
    /// to the floor, drops servers with no name/url, and drops tray icons
    /// that name a server or a metric that doesn't exist. A typo in this file
    /// should cost the user that one line, never the whole app.
    pub fn normalize(&mut self) {
        self.servers.retain(|s| {
            let ok = !s.name.trim().is_empty() && !s.url.trim().is_empty();
            if !ok {
                eprintln!("[claude-usage-widget] servers.json: dropping a server with no name or url");
            }
            ok
        });
        for server in &mut self.servers {
            server.url = server.url.trim_end_matches('/').to_string();
            server.poll_secs = server.poll_secs.max(MIN_POLL_SECS);
            server.timeout_ms = server.timeout_ms.clamp(200, 30_000);
            if let Some(probe) = &mut server.probe {
                probe.interval_secs = probe.interval_secs.max(60);
                probe.num_predict = probe.num_predict.clamp(1, 512);
            }
        }

        let servers = self.servers.clone();
        self.tray_icons.retain(|icon| {
            let Some(server) = servers.iter().find(|s| s.name == icon.server) else {
                eprintln!(
                    "[claude-usage-widget] servers.json: tray icon refers to unknown server {:?}",
                    icon.server
                );
                return false;
            };
            match metric_def(&icon.metric) {
                None => {
                    // A typo here is the single likeliest way to end up
                    // staring at a tray that's missing an icon, so this
                    // prints the whole catalog rather than just complaining.
                    eprintln!(
                        "[claude-usage-widget] servers.json: unknown metric {:?}. Available:",
                        icon.metric
                    );
                    for def in metric_catalog() {
                        eprintln!(
                            "[claude-usage-widget]   {:<12} {:<9} {}",
                            def.id,
                            match def.kinds {
                                None => "any",
                                Some(k) => k.label(),
                            },
                            def.help
                        );
                    }
                    false
                }
                Some(def) if !def.badgeable => {
                    eprintln!(
                        "[claude-usage-widget] servers.json: metric {:?} is too large to render as a tray badge; it stays in the menu only",
                        icon.metric
                    );
                    false
                }
                Some(def) if !def.applies_to(server.kind) => {
                    eprintln!(
                        "[claude-usage-widget] servers.json: metric {:?} is not available from a {} server",
                        icon.metric,
                        server.kind.label()
                    );
                    false
                }
                Some(_) => true,
            }
        });
    }

    pub fn enabled_servers(&self) -> Vec<ServerConfig> {
        self.servers.iter().filter(|s| s.enabled).cloned().collect()
    }
}

/// `%LOCALAPPDATA%\ClaudeUsageWidget\servers.json`
pub fn config_path() -> Option<PathBuf> {
    let local_app_data = std::env::var_os("LOCALAPPDATA")?;
    let mut path = PathBuf::from(local_app_data);
    path.push("ClaudeUsageWidget");
    path.push("servers.json");
    Some(path)
}

/// Loads the config, creating a commented-by-example template on first run.
///
/// A missing or unparseable file is never fatal: the widget falls back to an
/// empty config, which behaves exactly like the version before this feature
/// existed. Losing the Claude usage icon because of a stray comma in an
/// optional file would be a bad trade.
pub fn load_config() -> Config {
    let Some(path) = config_path() else {
        eprintln!("[claude-usage-widget] no %LOCALAPPDATA%; server monitoring disabled");
        return Config::default();
    };

    if !path.exists() {
        let template = template_config();
        match write_config(&template) {
            Ok(()) => eprintln!(
                "[claude-usage-widget] wrote starter server config to {}",
                path.display()
            ),
            Err(e) => eprintln!("[claude-usage-widget] could not write {}: {e}", path.display()),
        }
        return template;
    }

    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("[claude-usage-widget] could not read {}: {e}", path.display());
            return Config::default();
        }
    };

    match serde_json::from_str::<Config>(&text) {
        Ok(mut config) => {
            config.normalize();
            eprintln!(
                "[claude-usage-widget] loaded {} server(s) and {} server tray icon(s) from {}",
                config.servers.len(),
                config.tray_icons.len(),
                path.display()
            );
            config
        }
        Err(e) => {
            eprintln!(
                "[claude-usage-widget] {} is not valid JSON ({e}); server monitoring disabled until it is fixed",
                path.display()
            );
            Config::default()
        }
    }
}

pub fn write_config(config: &Config) -> std::io::Result<()> {
    let Some(path) = config_path() else {
        return Err(std::io::Error::other("no %LOCALAPPDATA%"));
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(config)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    std::fs::write(path, json + "\n")
}

/// The first-run file. Both example servers are `"enabled": false` and no
/// tray icons are defined, so a fresh install behaves identically to before
/// this feature landed -- the file exists to be *read and edited*, not to
/// start firing requests at localhost on someone's behalf.
fn template_config() -> Config {
    Config {
        servers: vec![
            ServerConfig {
                name: "vllm".to_string(),
                kind: ServerKind::Vllm,
                url: "http://localhost:8000".to_string(),
                poll_secs: 10,
                timeout_ms: 2000,
                enabled: false,
                auth_env: None,
                probe: None,
            },
            ServerConfig {
                name: "ollama".to_string(),
                kind: ServerKind::Ollama,
                url: "http://localhost:11434".to_string(),
                poll_secs: 30,
                timeout_ms: 2000,
                enabled: false,
                auth_env: None,
                probe: Some(ProbeConfig {
                    enabled: false,
                    interval_secs: 300,
                    num_predict: 32,
                }),
            },
        ],
        tray_icons: vec![
            TrayIconConfig {
                server: "vllm".to_string(),
                metric: "tps".to_string(),
                shape: IconShape::Hex,
                visible: true,
                warn: None,
                crit: None,
                label: None,
            },
            TrayIconConfig {
                server: "vllm".to_string(),
                metric: "running".to_string(),
                shape: IconShape::Hex,
                visible: true,
                warn: None,
                crit: None,
                label: None,
            },
        ],
    }
}

// ---------------------------------------------------------------------------
// Metric catalog
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unit {
    TokensPerSec,
    Count,
    PerMinute,
    Percent,
    /// Rendered on the badge as tenths of a second (a badge holds 3 digits,
    /// and sub-second latencies are the interesting range), spelled out in
    /// full in the tooltip.
    Seconds,
    Gigabytes,
    /// 1 or 0; the badge shows no digits at all, only the status colour.
    Flag,
}

pub struct MetricDef {
    pub id: &'static str,
    /// Short word used in tooltips and menu lines. Kept genuinely short: the
    /// tooltip budget is 63 characters including the server name.
    pub label: &'static str,
    pub unit: Unit,
    /// `None` = available from every server kind.
    pub kinds: Option<ServerKind>,
    /// (warn, crit) defaults; `None` means the metric is informational and
    /// stays green at any value -- the number is the feature, the colour is
    /// secondary.
    pub thresholds: Option<(f64, f64)>,
    /// Whether this can sensibly be a tray badge. Lifetime totals (millions
    /// of tokens) can't: three digits is the legible ceiling, so they live in
    /// the menu instead of pretending to fit.
    pub badgeable: bool,
    pub help: &'static str,
}

impl MetricDef {
    pub fn applies_to(&self, kind: ServerKind) -> bool {
        match self.kinds {
            None => true,
            Some(k) => k == kind,
        }
    }
}

pub fn metric_catalog() -> &'static [MetricDef] {
    &[
        MetricDef {
            id: "tps",
            label: "tok/s",
            unit: Unit::TokensPerSec,
            kinds: None,
            thresholds: None,
            badgeable: true,
            help: "Decode speed while actually generating (generated tokens / the server's own decode time). Idle-immune: this is the number benchmarks quote.",
        },
        MetricDef {
            id: "tps_wall",
            label: "tok/s avg",
            unit: Unit::TokensPerSec,
            kinds: Some(ServerKind::Vllm),
            thresholds: None,
            badgeable: true,
            help: "Throughput over wall-clock time, idle included. What the box actually produced this interval.",
        },
        MetricDef {
            id: "prefill_tps",
            label: "prefill",
            unit: Unit::TokensPerSec,
            kinds: None,
            thresholds: None,
            badgeable: true,
            help: "Prompt-processing speed (tokens actually computed, i.e. excluding prefix-cache hits).",
        },
        MetricDef {
            id: "running",
            label: "running",
            unit: Unit::Count,
            kinds: Some(ServerKind::Vllm),
            thresholds: None,
            badgeable: true,
            help: "Requests currently in the execution batch -- live connections doing work.",
        },
        MetricDef {
            id: "waiting",
            label: "queued",
            unit: Unit::Count,
            kinds: Some(ServerKind::Vllm),
            thresholds: Some((1.0, 5.0)),
            badgeable: true,
            help: "Requests admitted but waiting for capacity. Anything above zero means the server is the bottleneck.",
        },
        MetricDef {
            id: "reqmin",
            label: "req/min",
            unit: Unit::PerMinute,
            kinds: Some(ServerKind::Vllm),
            thresholds: None,
            badgeable: true,
            help: "Requests finished per minute over the last interval.",
        },
        MetricDef {
            id: "ttft",
            label: "TTFT",
            unit: Unit::Seconds,
            kinds: None,
            thresholds: None,
            badgeable: true,
            help: "Average time to first token over the last interval (held from the previous interval when nothing ran).",
        },
        MetricDef {
            id: "tpot",
            label: "TPOT",
            unit: Unit::Seconds,
            kinds: Some(ServerKind::Vllm),
            thresholds: None,
            badgeable: true,
            help: "Average time per output token -- the reciprocal of a single stream's perceived speed.",
        },
        MetricDef {
            id: "kv",
            label: "KV",
            unit: Unit::Percent,
            kinds: Some(ServerKind::Vllm),
            thresholds: Some((80.0, 95.0)),
            badgeable: true,
            help: "KV-cache occupancy. Climbing toward 100% is what precedes preemptions.",
        },
        MetricDef {
            id: "prefix",
            label: "cache",
            unit: Unit::Percent,
            kinds: Some(ServerKind::Vllm),
            thresholds: None,
            badgeable: true,
            help: "Prefix-cache hit rate over the last interval -- how much of each prompt did not have to be recomputed.",
        },
        MetricDef {
            id: "errors",
            label: "errors",
            unit: Unit::Count,
            kinds: Some(ServerKind::Vllm),
            thresholds: Some((1.0, 3.0)),
            badgeable: true,
            help: "Requests that finished as error or abort during the last interval.",
        },
        MetricDef {
            id: "preempt",
            label: "preempt",
            unit: Unit::Count,
            kinds: Some(ServerKind::Vllm),
            thresholds: Some((1.0, 5.0)),
            badgeable: true,
            help: "Preemptions during the last interval: requests evicted mid-flight because KV cache ran out.",
        },
        MetricDef {
            id: "models",
            label: "loaded",
            unit: Unit::Count,
            kinds: Some(ServerKind::Ollama),
            thresholds: None,
            badgeable: true,
            help: "Models currently resident (Ollama /api/ps).",
        },
        MetricDef {
            id: "vram",
            label: "VRAM",
            unit: Unit::Gigabytes,
            kinds: Some(ServerKind::Ollama),
            thresholds: None,
            badgeable: true,
            help: "VRAM held by currently-loaded models.",
        },
        MetricDef {
            id: "installed",
            label: "models",
            unit: Unit::Count,
            kinds: Some(ServerKind::Ollama),
            thresholds: None,
            badgeable: true,
            help: "Models available to pull from local storage (Ollama /api/tags).",
        },
        MetricDef {
            id: "up",
            label: "up",
            unit: Unit::Flag,
            kinds: None,
            thresholds: None,
            badgeable: true,
            help: "Reachability only: green when the last scrape succeeded, red when it did not.",
        },
        MetricDef {
            id: "reqs",
            label: "requests",
            unit: Unit::Count,
            kinds: Some(ServerKind::Vllm),
            thresholds: None,
            badgeable: false,
            help: "Total requests handled since the server started.",
        },
        MetricDef {
            id: "tokens",
            label: "tokens",
            unit: Unit::Count,
            kinds: Some(ServerKind::Vllm),
            thresholds: None,
            badgeable: false,
            help: "Total tokens generated since the server started.",
        },
    ]
}

pub fn metric_def(id: &str) -> Option<&'static MetricDef> {
    metric_catalog().iter().find(|d| d.id == id)
}

// ---------------------------------------------------------------------------
// Samples
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ServerSample {
    pub server: String,
    pub kind: ServerKind,
    /// `None` when the scrape succeeded; the failure text otherwise.
    pub error: Option<String>,
    pub metrics: BTreeMap<&'static str, f64>,
    /// Model name(s) the server reports, for the menu line.
    pub model: Option<String>,
}

impl ServerSample {
    pub fn get(&self, id: &str) -> Option<f64> {
        self.metrics.get(id).copied()
    }

    fn down(server: &str, kind: ServerKind, error: String) -> Self {
        Self {
            server: server.to_string(),
            kind,
            error: Some(error),
            metrics: BTreeMap::new(),
            model: None,
        }
    }

    /// One line for the tray menu, e.g.
    /// `vader-vllm  47 tok/s · 1 running · 0 queued · KV 3%`.
    pub fn menu_line(&self) -> String {
        if let Some(err) = &self.error {
            return format!("{}  unreachable ({})", self.server, short_error(err));
        }

        let mut parts: Vec<String> = Vec::new();
        match self.kind {
            ServerKind::Vllm => {
                // The served model comes free with the scrape (it's a label
                // on every sample), and it's the first thing you want to
                // confirm when a number looks wrong.
                if let Some(model) = &self.model {
                    parts.push(model.clone());
                }
                if let Some(v) = self.get("tps") {
                    parts.push(format!("{} tok/s", v.round() as i64));
                }
                if let Some(v) = self.get("running") {
                    parts.push(format!("{} running", v.round() as i64));
                }
                if let Some(v) = self.get("waiting") {
                    parts.push(format!("{} queued", v.round() as i64));
                }
                if let Some(v) = self.get("kv") {
                    parts.push(format!("KV {}%", v.round() as i64));
                }
                if let Some(v) = self.get("reqs") {
                    parts.push(format!("{} reqs", thousands(v)));
                }
            }
            ServerKind::Ollama => {
                if let Some(v) = self.get("models") {
                    parts.push(format!("{} loaded", v.round() as i64));
                }
                if let Some(v) = self.get("vram") {
                    parts.push(format!("{} VRAM", format_value(Unit::Gigabytes, v)));
                }
                if let Some(v) = self.get("tps") {
                    parts.push(format!("{} tok/s probed", v.round() as i64));
                }
                if let Some(v) = self.get("installed") {
                    parts.push(format!("{} installed", v.round() as i64));
                }
            }
        }

        if parts.is_empty() {
            parts.push("up".to_string());
        }
        format!("{}  {}", self.server, parts.join("  ·  "))
    }
}

fn thousands(v: f64) -> String {
    let n = v.round() as i64;
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 {
        format!("-{out}")
    } else {
        out
    }
}

/// Trims a reqwest error down to something that fits in a menu line.
fn short_error(err: &str) -> String {
    let first = err.split(':').next_back().unwrap_or(err).trim();
    let first = if first.is_empty() { err } else { first };
    if first.chars().count() > 40 {
        format!("{}...", first.chars().take(37).collect::<String>())
    } else {
        first.to_string()
    }
}

// ---------------------------------------------------------------------------
// Rendering one metric onto a badge
// ---------------------------------------------------------------------------

/// What a single tray icon should currently display.
pub struct MetricDisplay {
    /// Digits for the badge; `None` renders the digitless "no data" badge.
    pub badge: Option<u32>,
    /// Status: 0 = ok, 1 = warn, 2 = critical, 3 = unavailable.
    pub level: Level,
    pub tooltip: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Ok,
    Warn,
    Crit,
    Unavailable,
}

pub fn display_for(cfg: &TrayIconConfig, sample: Option<&ServerSample>) -> MetricDisplay {
    let def = metric_def(&cfg.metric);
    let label = cfg
        .label
        .clone()
        .or_else(|| def.map(|d| d.label.to_string()))
        .unwrap_or_else(|| cfg.metric.clone());

    let Some(sample) = sample else {
        return MetricDisplay {
            badge: None,
            level: Level::Unavailable,
            tooltip: clamp_tooltip(&format!("{} {}: waiting", cfg.server, label)),
        };
    };

    if let Some(err) = &sample.error {
        return MetricDisplay {
            badge: None,
            level: Level::Unavailable,
            tooltip: clamp_tooltip(&format!("{}: {}", cfg.server, short_error(err))),
        };
    }

    let Some(def) = def else {
        return MetricDisplay {
            badge: None,
            level: Level::Unavailable,
            tooltip: clamp_tooltip(&format!("{} {}: unknown metric", cfg.server, label)),
        };
    };

    let Some(value) = sample.get(&cfg.metric) else {
        return MetricDisplay {
            badge: None,
            level: Level::Unavailable,
            tooltip: clamp_tooltip(&format!("{} {}: n/a", cfg.server, label)),
        };
    };

    let badge = badge_digits(def.unit, value);
    let level = level_for(def, cfg, value);
    let tooltip = clamp_tooltip(&format!(
        "{} {} {}",
        sample.server,
        label,
        format_value(def.unit, value)
    ));

    MetricDisplay {
        badge,
        level,
        tooltip,
    }
}

/// Digits to draw on the badge. Three is the legible ceiling (see
/// `icon::digit_box_for_count`), so everything is clamped to 999 rather than
/// silently rendering a fourth digit nobody can read.
pub fn badge_digits(unit: Unit, value: f64) -> Option<u32> {
    if !value.is_finite() {
        return None;
    }
    let n = match unit {
        Unit::Flag => return None,
        // Tenths of a second: 1.58s -> "16". Sub-second is the interesting
        // range for TTFT, and "2" would throw away everything that matters.
        Unit::Seconds => (value * 10.0).round(),
        _ => value.round(),
    };
    Some(n.clamp(0.0, 999.0) as u32)
}

pub fn format_value(unit: Unit, value: f64) -> String {
    // Rust's `Sum` for f64 uses -0.0 as its identity, so summing an empty
    // iterator (an Ollama with nothing loaded, say) yields negative zero --
    // which formats as the faintly alarming "-0.0 GB". Seen live before this
    // line existed.
    let value = if value == 0.0 { 0.0 } else { value };
    match unit {
        Unit::TokensPerSec => format!("{} tok/s", value.round() as i64),
        Unit::Count => thousands(value),
        Unit::PerMinute => format!("{}/min", value.round() as i64),
        Unit::Percent => format!("{}%", value.round() as i64),
        Unit::Seconds => {
            if value >= 10.0 {
                format!("{:.0}s", value)
            } else if value >= 1.0 {
                format!("{:.1}s", value)
            } else {
                format!("{:.0}ms", value * 1000.0)
            }
        }
        Unit::Gigabytes => format!("{:.1} GB", value),
        Unit::Flag => {
            if value >= 0.5 {
                "up".to_string()
            } else {
                "down".to_string()
            }
        }
    }
}

fn level_for(def: &MetricDef, cfg: &TrayIconConfig, value: f64) -> Level {
    if def.unit == Unit::Flag {
        return if value >= 0.5 { Level::Ok } else { Level::Crit };
    }
    let thresholds = match (cfg.warn, cfg.crit) {
        (None, None) => def.thresholds,
        (warn, crit) => {
            let (dw, dc) = def.thresholds.unwrap_or((f64::MAX, f64::MAX));
            Some((warn.unwrap_or(dw), crit.unwrap_or(dc)))
        }
    };
    match thresholds {
        None => Level::Ok,
        Some((warn, crit)) => {
            if value >= crit {
                Level::Crit
            } else if value >= warn {
                Level::Warn
            } else {
                Level::Ok
            }
        }
    }
}

/// Enforces the real (63-character) Windows tray tooltip limit, which the
/// shell applies silently -- `Shell_NotifyIconW` happily returns success and
/// simply renders the wrong string.
pub fn clamp_tooltip(text: &str) -> String {
    if text.chars().count() <= MAX_TOOLTIP_CHARS {
        return text.to_string();
    }
    text.chars().take(MAX_TOOLTIP_CHARS - 1).collect::<String>() + "\u{2026}"
}

// ---------------------------------------------------------------------------
// Polling
// ---------------------------------------------------------------------------

pub enum WorkerCommand {
    /// Replace the server list wholesale (config reloaded from disk).
    Reconfigure(Vec<ServerConfig>),
    /// Scrape everything now, ignoring schedules and backoff.
    RefreshNow,
    Shutdown,
}

enum Poller {
    Vllm(vllm::VllmPoller),
    Ollama(ollama::OllamaPoller),
}

struct Slot {
    config: ServerConfig,
    poller: Poller,
    next_due: Instant,
    failures: u32,
    /// Whether the last scrape succeeded, so the log can record *transitions*
    /// rather than every single poll.
    ///
    /// Logging nothing on success is a trap this app has already fallen into
    /// once: with a silent success path, "is it frozen or just quiet?" is a
    /// coin flip during an actual incident. Logging every success instead
    /// would write a line every 10 seconds per server, forever. Transitions
    /// (plus the first scrape, plus the first few failures) are the middle
    /// ground -- one line when a server comes up, one when it goes away.
    was_up: Option<bool>,
}

impl Slot {
    fn new(config: ServerConfig) -> Self {
        let poller = match config.kind {
            ServerKind::Vllm => Poller::Vllm(vllm::VllmPoller::default()),
            ServerKind::Ollama => Poller::Ollama(ollama::OllamaPoller::default()),
        };
        Self {
            config,
            poller,
            next_due: Instant::now(),
            failures: 0,
            was_up: None,
        }
    }

    fn poll(&mut self, client: &reqwest::blocking::Client) -> ServerSample {
        let result = match &mut self.poller {
            Poller::Vllm(p) => p.poll(client, &self.config),
            Poller::Ollama(p) => p.poll(client, &self.config),
        };

        match result {
            Ok(sample) => {
                self.failures = 0;
                self.next_due = Instant::now() + Duration::from_secs(self.config.poll_secs);
                if self.was_up != Some(true) {
                    eprintln!("[claude-usage-widget] {}", sample.menu_line());
                }
                self.was_up = Some(true);
                sample
            }
            Err(e) => {
                self.failures = self.failures.saturating_add(1);
                let backoff = (BACKOFF_BASE * 2u32.saturating_pow(self.failures.min(5) - 1))
                    .min(BACKOFF_MAX)
                    .max(Duration::from_secs(self.config.poll_secs));
                self.next_due = Instant::now() + backoff;
                // One line per failure, but only the first few, so a machine
                // left off the LAN overnight doesn't write a gigabyte of log.
                if self.failures <= 3 || self.was_up == Some(true) {
                    eprintln!(
                        "[claude-usage-widget] {} scrape failed ({}): retrying in {}s",
                        self.config.name,
                        e,
                        backoff.as_secs()
                    );
                }
                self.was_up = Some(false);
                ServerSample::down(&self.config.name, self.config.kind, e)
            }
        }
    }
}

/// Spawns the single worker thread that scrapes every configured server.
///
/// One thread for all servers, not one per server: each scrape has a hard
/// timeout (`timeout_ms`, default 2s) so a dead host delays the others by at
/// most that much, and the alternative -- N threads plus N proxies -- buys
/// nothing at the handful-of-servers scale this is for.
pub fn spawn_worker<F>(servers: Vec<ServerConfig>, on_sample: F) -> Sender<WorkerCommand>
where
    F: Fn(ServerSample) + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<WorkerCommand>();

    thread::spawn(move || {
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!("claude-usage-widget/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new());

        let mut slots: Vec<Slot> = servers.into_iter().map(Slot::new).collect();

        loop {
            let now = Instant::now();
            for slot in slots.iter_mut() {
                if slot.next_due <= now {
                    let sample = slot.poll(&client);
                    on_sample(sample);
                }
            }

            // Sleep until the earliest next scrape, but stay responsive to
            // commands (and never spin when nothing is configured).
            let wait = slots
                .iter()
                .map(|s| s.next_due.saturating_duration_since(Instant::now()))
                .min()
                .unwrap_or(Duration::from_secs(60))
                .clamp(Duration::from_millis(100), Duration::from_secs(60));

            match rx.recv_timeout(wait) {
                Ok(WorkerCommand::Reconfigure(new_servers)) => {
                    eprintln!(
                        "[claude-usage-widget] reconfiguring server monitoring: {} server(s)",
                        new_servers.len()
                    );
                    slots = new_servers.into_iter().map(Slot::new).collect();
                }
                Ok(WorkerCommand::RefreshNow) => {
                    for slot in slots.iter_mut() {
                        slot.next_due = Instant::now();
                        slot.failures = 0;
                    }
                }
                Ok(WorkerCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    });

    tx
}

/// Shared by both providers: a GET with the configured timeout and optional
/// bearer token, returning the body text.
pub(crate) fn get_text(
    client: &reqwest::blocking::Client,
    config: &ServerConfig,
    path: &str,
) -> Result<String, String> {
    let url = format!("{}{}", config.url, path);
    let mut req = client
        .get(&url)
        .timeout(Duration::from_millis(config.timeout_ms));
    if let Some(token) = auth_token(config) {
        req = req.bearer_auth(token);
    }
    let response = req.send().map_err(|e| describe(&e))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    response.text().map_err(|e| describe(&e))
}

pub(crate) fn auth_token(config: &ServerConfig) -> Option<String> {
    let var = config.auth_env.as_ref()?;
    match std::env::var(var) {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => {
            eprintln!(
                "[claude-usage-widget] {}: auth_env {:?} is not set",
                config.name, var
            );
            None
        }
    }
}

/// reqwest's Display is a long chain; the tray has 63 characters.
pub(crate) fn describe(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timeout".to_string()
    } else if e.is_connect() {
        "no route".to_string()
    } else if e.is_decode() {
        "bad response".to_string()
    } else {
        e.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn icon(metric: &str) -> TrayIconConfig {
        TrayIconConfig {
            server: "vader-vllm".to_string(),
            metric: metric.to_string(),
            shape: IconShape::Hex,
            visible: true,
            warn: None,
            crit: None,
            label: None,
        }
    }

    fn sample(pairs: &[(&'static str, f64)]) -> ServerSample {
        ServerSample {
            server: "vader-vllm".to_string(),
            kind: ServerKind::Vllm,
            error: None,
            metrics: pairs.iter().copied().collect(),
            model: Some("qwen3.8-27b".to_string()),
        }
    }

    #[test]
    fn every_catalog_id_is_unique() {
        let mut ids: Vec<&str> = metric_catalog().iter().map(|d| d.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate metric id in the catalog");
    }

    #[test]
    fn badge_clamps_to_three_digits() {
        assert_eq!(badge_digits(Unit::TokensPerSec, 47.4), Some(47));
        assert_eq!(badge_digits(Unit::TokensPerSec, 1523.0), Some(999));
        assert_eq!(badge_digits(Unit::Count, -3.0), Some(0));
        assert_eq!(badge_digits(Unit::Flag, 1.0), None);
        assert_eq!(badge_digits(Unit::TokensPerSec, f64::NAN), None);
    }

    #[test]
    fn seconds_render_as_tenths_on_the_badge() {
        // 1.58s of TTFT has to read as "16", not as "2".
        assert_eq!(badge_digits(Unit::Seconds, 1.58), Some(16));
        assert_eq!(format_value(Unit::Seconds, 1.58), "1.6s");
        assert_eq!(format_value(Unit::Seconds, 0.42), "420ms");
    }

    #[test]
    fn negative_zero_never_reaches_the_screen() {
        // An Ollama with nothing loaded sums an empty iterator, and f64's Sum
        // identity is -0.0. It rendered as "-0.0 GB VRAM" in the menu once.
        let empty: f64 = [].iter().sum();
        assert_eq!(format_value(Unit::Gigabytes, empty), "0.0 GB");
        assert_eq!(format_value(Unit::TokensPerSec, -0.0), "0 tok/s");
    }

    #[test]
    fn thresholds_default_per_metric_and_can_be_overridden() {
        let kv = icon("kv");
        let s = sample(&[("kv", 96.0)]);
        assert_eq!(display_for(&kv, Some(&s)).level, Level::Crit);

        let mut relaxed = icon("kv");
        relaxed.warn = Some(97.0);
        relaxed.crit = Some(99.0);
        assert_eq!(display_for(&relaxed, Some(&s)).level, Level::Ok);
    }

    #[test]
    fn informational_metrics_stay_green_at_any_value() {
        let s = sample(&[("tps", 900.0)]);
        assert_eq!(display_for(&icon("tps"), Some(&s)).level, Level::Ok);
    }

    #[test]
    fn unreachable_server_shows_no_digits() {
        let down = ServerSample::down("vader-vllm", ServerKind::Vllm, "timeout".to_string());
        let d = display_for(&icon("tps"), Some(&down));
        assert_eq!(d.badge, None);
        assert_eq!(d.level, Level::Unavailable);
        assert!(d.tooltip.contains("timeout"));
    }

    #[test]
    fn tooltips_never_exceed_the_real_windows_limit() {
        // The worst realistic case: a long server name and a long label.
        let mut cfg = icon("prefill_tps");
        cfg.server = "a-very-long-inference-server-name-from-the-config".to_string();
        let mut s = sample(&[("prefill_tps", 817.0)]);
        s.server = cfg.server.clone();
        let d = display_for(&cfg, Some(&s));
        assert!(
            d.tooltip.chars().count() <= MAX_TOOLTIP_CHARS,
            "tooltip was {} chars: {:?}",
            d.tooltip.chars().count(),
            d.tooltip
        );
    }

    #[test]
    fn normalize_drops_icons_pointing_at_nothing() {
        let mut config = Config {
            servers: vec![ServerConfig {
                name: "a".to_string(),
                kind: ServerKind::Vllm,
                url: "http://host:8000/".to_string(),
                poll_secs: 0,
                timeout_ms: 1,
                enabled: true,
                auth_env: None,
                probe: None,
            }],
            tray_icons: vec![
                icon("tps"),                 // unknown server -> dropped
                TrayIconConfig {
                    server: "a".to_string(),
                    metric: "nope".to_string(),
                    ..icon("tps")
                }, // unknown metric -> dropped
                TrayIconConfig {
                    server: "a".to_string(),
                    metric: "tokens".to_string(),
                    ..icon("tps")
                }, // not badgeable -> dropped
                TrayIconConfig {
                    server: "a".to_string(),
                    metric: "models".to_string(),
                    ..icon("tps")
                }, // ollama-only on a vllm server -> dropped
                TrayIconConfig {
                    server: "a".to_string(),
                    metric: "tps".to_string(),
                    ..icon("tps")
                }, // kept
            ],
        };
        config.normalize();
        assert_eq!(config.tray_icons.len(), 1);
        assert_eq!(config.tray_icons[0].metric, "tps");
        // Intervals clamped, trailing slash trimmed.
        assert_eq!(config.servers[0].poll_secs, MIN_POLL_SECS);
        assert_eq!(config.servers[0].timeout_ms, 200);
        assert_eq!(config.servers[0].url, "http://host:8000");
    }

    #[test]
    fn menu_line_reads_as_a_sentence() {
        let s = sample(&[
            ("tps", 47.0),
            ("running", 1.0),
            ("waiting", 0.0),
            ("kv", 3.0),
            ("reqs", 1192.0),
        ]);
        assert_eq!(
            s.menu_line(),
            "vader-vllm  qwen3.8-27b  ·  47 tok/s  ·  1 running  ·  0 queued  ·  KV 3%  ·  1,192 reqs"
        );
    }

    #[test]
    fn template_is_inert_until_edited() {
        let mut t = template_config();
        t.normalize();
        assert!(t.servers.iter().all(|s| !s.enabled));
        assert!(t.enabled_servers().is_empty());
    }
}
