// vLLM provider: scrapes `/metrics` and turns cumulative counters into the
// live rates the tray icons show.
//
// vLLM's Prometheus endpoint is unusually complete -- running/waiting queues,
// per-phase timings, cache hit rates, per-finish-reason request counts -- so
// almost everything here is arithmetic on counters rather than heroics.
//
// The two things that need care:
//
// 1. **Counters are cumulative since the engine started.** Every rate is a
//    delta between two scrapes. On the very first scrape there is no delta,
//    so rates report 0 while *averages* (TTFT, TPOT, cache hit rate) fall
//    back to the lifetime figures -- an average is meaningful without an
//    interval, a rate is not, and showing something real immediately beats a
//    blank icon for the first ten seconds.
//
// 2. **The server can restart under us.** Then every counter resets to zero
//    and a naive delta goes hugely negative (or, worse, is clamped to zero
//    and quietly wrong for one interval). Any counter moving backwards is
//    treated as a restart: the baseline is dropped and that interval reports
//    gauges only.

use std::collections::BTreeMap;
use std::time::Instant;

use super::prom::Metrics;
use super::{get_text, ServerConfig, ServerKind, ServerSample};

/// The counters we diff between scrapes. Everything is `Option` because a
/// metric can be absent on older vLLM builds -- absent must not be confused
/// with zero, or a missing metric would render as a confident "0 tok/s".
#[derive(Clone, Copy, Debug, Default)]
struct Snapshot {
    gen_tokens: Option<f64>,
    prefill_tokens: Option<f64>,
    decode_time: Option<f64>,
    prefill_time: Option<f64>,
    ttft_sum: Option<f64>,
    ttft_count: Option<f64>,
    tpot_sum: Option<f64>,
    tpot_count: Option<f64>,
    requests: Option<f64>,
    errors: Option<f64>,
    preemptions: Option<f64>,
    prefix_queries: Option<f64>,
    prefix_hits: Option<f64>,
}

impl Snapshot {
    /// True if any counter in `self` is smaller than in `prev` -- i.e. the
    /// engine restarted (or the endpoint now points at a different one).
    fn went_backwards(&self, prev: &Snapshot) -> bool {
        let pairs: [(Option<f64>, Option<f64>); 13] = [
            (self.gen_tokens, prev.gen_tokens),
            (self.prefill_tokens, prev.prefill_tokens),
            (self.decode_time, prev.decode_time),
            (self.prefill_time, prev.prefill_time),
            (self.ttft_sum, prev.ttft_sum),
            (self.ttft_count, prev.ttft_count),
            (self.tpot_sum, prev.tpot_sum),
            (self.tpot_count, prev.tpot_count),
            (self.requests, prev.requests),
            (self.errors, prev.errors),
            (self.preemptions, prev.preemptions),
            (self.prefix_queries, prev.prefix_queries),
            (self.prefix_hits, prev.prefix_hits),
        ];
        pairs.iter().any(|(now, before)| match (now, before) {
            // A tiny epsilon so float noise in a float-encoded counter can
            // never masquerade as a restart.
            (Some(n), Some(b)) => *n < *b - 1e-6,
            _ => false,
        })
    }
}

#[derive(Default)]
pub struct VllmPoller {
    prev: Option<(Instant, Snapshot)>,
    /// Averages carried across idle intervals. A latency is a property of
    /// requests, not of time: when nothing ran this interval the honest
    /// answer is the last measured value, not zero.
    last_ttft: Option<f64>,
    last_tpot: Option<f64>,
    last_prefix_pct: Option<f64>,
}

impl VllmPoller {
    pub fn poll(
        &mut self,
        client: &reqwest::blocking::Client,
        config: &ServerConfig,
    ) -> Result<ServerSample, String> {
        let body = get_text(client, config, "/metrics")?;
        let metrics = Metrics::parse(&body);
        if metrics.is_empty() {
            return Err("no metrics".to_string());
        }
        Ok(self.sample_from(&metrics, config, Instant::now()))
    }

    /// Split out from `poll` so the rate arithmetic is testable without a
    /// server: feed it two parsed scrapes and a time gap.
    fn sample_from(
        &mut self,
        metrics: &Metrics,
        config: &ServerConfig,
        now: Instant,
    ) -> ServerSample {
        let mut out: BTreeMap<&'static str, f64> = BTreeMap::new();
        out.insert("up", 1.0);

        // ---- gauges: valid on their own, no history needed -------------
        if let Some(v) = metrics.sum("vllm:num_requests_running") {
            out.insert("running", v);
        }
        if let Some(v) = metrics.sum("vllm:num_requests_waiting") {
            out.insert("waiting", v);
        }
        if let Some(v) = metrics.mean("vllm:kv_cache_usage_perc") {
            // Reported as a 0..1 fraction; the badge wants percent.
            out.insert("kv", v * 100.0);
        }

        let snap = read_snapshot(metrics);

        if let Some(v) = snap.requests {
            out.insert("reqs", v);
        }
        if let Some(v) = snap.gen_tokens {
            out.insert("tokens", v);
        }

        // ---- rates: need a previous scrape ------------------------------
        match self.prev.take() {
            Some((prev_at, prev)) if !snap.went_backwards(&prev) => {
                let dt = now.saturating_duration_since(prev_at).as_secs_f64();
                if dt > 0.0 {
                    let d_gen = delta(snap.gen_tokens, prev.gen_tokens);
                    let d_decode = delta(snap.decode_time, prev.decode_time);
                    let d_prefill_tok = delta(snap.prefill_tokens, prev.prefill_tokens);
                    let d_prefill_time = delta(snap.prefill_time, prev.prefill_time);

                    if let Some(g) = d_gen {
                        out.insert("tps_wall", g / dt);
                        // Decode speed: tokens per second of *generating*
                        // time. Falls back to wall-clock when the engine
                        // doesn't publish decode time.
                        let tps = match d_decode {
                            Some(d) if d > 0.01 => g / d,
                            _ => g / dt,
                        };
                        out.insert("tps", tps);
                    }
                    if let (Some(tok), Some(t)) = (d_prefill_tok, d_prefill_time) {
                        if t > 0.01 {
                            out.insert("prefill_tps", tok / t);
                        } else if tok == 0.0 {
                            out.insert("prefill_tps", 0.0);
                        }
                    }
                    if let Some(r) = delta(snap.requests, prev.requests) {
                        out.insert("reqmin", r / dt * 60.0);
                    }
                    if let Some(e) = delta(snap.errors, prev.errors) {
                        out.insert("errors", e);
                    }
                    if let Some(p) = delta(snap.preemptions, prev.preemptions) {
                        out.insert("preempt", p);
                    }

                    // Interval averages, held across idle intervals.
                    if let (Some(sum), Some(count)) = (
                        delta(snap.ttft_sum, prev.ttft_sum),
                        delta(snap.ttft_count, prev.ttft_count),
                    ) {
                        if count > 0.0 {
                            self.last_ttft = Some(sum / count);
                        }
                    }
                    if let (Some(sum), Some(count)) = (
                        delta(snap.tpot_sum, prev.tpot_sum),
                        delta(snap.tpot_count, prev.tpot_count),
                    ) {
                        if count > 0.0 {
                            self.last_tpot = Some(sum / count);
                        }
                    }
                    if let (Some(q), Some(h)) = (
                        delta(snap.prefix_queries, prev.prefix_queries),
                        delta(snap.prefix_hits, prev.prefix_hits),
                    ) {
                        if q > 0.0 {
                            self.last_prefix_pct = Some(h / q * 100.0);
                        }
                    }
                }
            }
            Some(_) => {
                // Counters moved backwards: the engine restarted. Drop the
                // baseline (already taken) and skip rates for this interval
                // rather than reporting a spike or a bogus zero.
                eprintln!(
                    "[claude-usage-widget] {}: counters reset (engine restart?); re-baselining",
                    config.name
                );
                self.last_ttft = None;
                self.last_tpot = None;
                self.last_prefix_pct = None;
            }
            None => {
                // First scrape of this server: no interval exists yet, so
                // rates stay absent, but lifetime averages are real numbers
                // and worth showing immediately.
                self.last_ttft = ratio(snap.ttft_sum, snap.ttft_count);
                self.last_tpot = ratio(snap.tpot_sum, snap.tpot_count);
                self.last_prefix_pct =
                    ratio(snap.prefix_hits, snap.prefix_queries).map(|v| v * 100.0);
            }
        }

        if let Some(v) = self.last_ttft {
            out.insert("ttft", v);
        }
        if let Some(v) = self.last_tpot {
            out.insert("tpot", v);
        }
        if let Some(v) = self.last_prefix_pct {
            out.insert("prefix", v);
        }

        self.prev = Some((now, snap));

        ServerSample {
            server: config.name.clone(),
            kind: ServerKind::Vllm,
            error: None,
            metrics: out,
            model: model_name(metrics),
        }
    }
}

fn delta(now: Option<f64>, before: Option<f64>) -> Option<f64> {
    match (now, before) {
        (Some(n), Some(b)) => Some((n - b).max(0.0)),
        _ => None,
    }
}

fn ratio(num: Option<f64>, den: Option<f64>) -> Option<f64> {
    match (num, den) {
        (Some(n), Some(d)) if d > 0.0 => Some(n / d),
        _ => None,
    }
}

/// Reads the counters, tolerating the renames vLLM has been through: newer
/// builds publish `request_time_per_output_token_seconds` and
/// `prompt_tokens_by_source_total`, older ones don't.
fn read_snapshot(m: &Metrics) -> Snapshot {
    let prefill_tokens = m
        .sum_where("vllm:prompt_tokens_by_source_total", "source", "local_compute")
        .or_else(|| {
            // Older builds: total prompt tokens minus the ones served from
            // cache is the same "actually computed" quantity.
            match (
                m.sum("vllm:prompt_tokens_total"),
                m.sum("vllm:prompt_tokens_cached_total"),
            ) {
                (Some(total), Some(cached)) => Some((total - cached).max(0.0)),
                (Some(total), None) => Some(total),
                _ => None,
            }
        });

    let (tpot_sum, tpot_count) = first_present(
        m,
        &[
            "vllm:request_time_per_output_token_seconds",
            "vllm:inter_token_latency_seconds",
            "vllm:time_per_output_token_seconds",
        ],
    );

    Snapshot {
        gen_tokens: m.sum("vllm:generation_tokens_total"),
        prefill_tokens,
        decode_time: m.sum("vllm:request_decode_time_seconds_sum"),
        prefill_time: m.sum("vllm:request_prefill_time_seconds_sum"),
        ttft_sum: m.sum("vllm:time_to_first_token_seconds_sum"),
        ttft_count: m.sum("vllm:time_to_first_token_seconds_count"),
        tpot_sum,
        tpot_count,
        requests: m.sum("vllm:request_success_total"),
        errors: match (
            m.sum_where("vllm:request_success_total", "finished_reason", "error"),
            m.sum_where("vllm:request_success_total", "finished_reason", "abort"),
        ) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
        },
        preemptions: m.sum("vllm:num_preemptions_total"),
        prefix_queries: m.sum("vllm:prefix_cache_queries_total"),
        prefix_hits: m.sum("vllm:prefix_cache_hits_total"),
    }
}

/// Returns `(sum, count)` for the first histogram base name that exists.
fn first_present(m: &Metrics, names: &[&str]) -> (Option<f64>, Option<f64>) {
    for name in names {
        let sum = m.sum(&format!("{name}_sum"));
        let count = m.sum(&format!("{name}_count"));
        if sum.is_some() && count.is_some() {
            return (sum, count);
        }
    }
    (None, None)
}

/// The model a server is serving, for the menu line. Read off the labels
/// rather than a second `/v1/models` request -- it's already in the scrape.
fn model_name(m: &Metrics) -> Option<String> {
    for metric in [
        "vllm:num_requests_running",
        "vllm:generation_tokens_total",
        "vllm:prompt_tokens_total",
    ] {
        let names = m.label_values(metric, "model_name");
        if !names.is_empty() {
            return Some(names.join(", "));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn config() -> ServerConfig {
        ServerConfig {
            name: "vader-vllm".to_string(),
            kind: ServerKind::Vllm,
            url: "http://vader:8010".to_string(),
            poll_secs: 10,
            timeout_ms: 2000,
            enabled: true,
            auth_env: None,
            probe: None,
        }
    }

    /// Shaped exactly like the live scrape from vader:8010 (vLLM as of
    /// 2026-08-19), including the label sets, so a future rename in a version
    /// bump shows up here as a failing test rather than as a blank icon.
    fn scrape(gen_tok: f64, decode: f64, reqs: f64, running: f64) -> String {
        format!(
            concat!(
                "vllm:num_requests_running{{engine=\"0\",model_name=\"qwen3.8-27b\"}} {running}\n",
                "vllm:num_requests_waiting{{engine=\"0\",model_name=\"qwen3.8-27b\"}} 0.0\n",
                "vllm:kv_cache_usage_perc{{engine=\"0\",model_name=\"qwen3.8-27b\"}} 0.12\n",
                "vllm:generation_tokens_total{{engine=\"0\",model_name=\"qwen3.8-27b\"}} {gen_tokens}\n",
                "vllm:prompt_tokens_total{{engine=\"0\",model_name=\"qwen3.8-27b\"}} 87238341.0\n",
                "vllm:prompt_tokens_by_source_total{{engine=\"0\",model_name=\"q\",source=\"local_compute\"}} 1254581.0\n",
                "vllm:prompt_tokens_cached_total{{engine=\"0\",model_name=\"q\"}} 85983760.0\n",
                "vllm:request_decode_time_seconds_sum{{engine=\"0\"}} {decode}\n",
                "vllm:request_decode_time_seconds_count{{engine=\"0\"}} 1192.0\n",
                "vllm:request_prefill_time_seconds_sum{{engine=\"0\"}} 1535.65\n",
                "vllm:time_to_first_token_seconds_sum{{engine=\"0\"}} 1886.42\n",
                "vllm:time_to_first_token_seconds_count{{engine=\"0\"}} 1194.0\n",
                "vllm:request_time_per_output_token_seconds_sum{{engine=\"0\"}} 47.0\n",
                "vllm:request_time_per_output_token_seconds_count{{engine=\"0\"}} 1192.0\n",
                "vllm:request_success_total{{engine=\"0\",finished_reason=\"stop\"}} {reqs}\n",
                "vllm:request_success_total{{engine=\"0\",finished_reason=\"error\"}} 0.0\n",
                "vllm:request_success_total{{engine=\"0\",finished_reason=\"abort\"}} 0.0\n",
                "vllm:num_preemptions_total{{engine=\"0\"}} 0.0\n",
                "vllm:prefix_cache_queries_total{{engine=\"0\"}} 87238341.0\n",
                "vllm:prefix_cache_hits_total{{engine=\"0\"}} 85983760.0\n",
            ),
            gen_tokens = gen_tok,
            decode = decode,
            reqs = reqs,
            running = running
        )
    }

    #[test]
    fn first_scrape_has_gauges_and_lifetime_averages_but_no_rates() {
        let mut p = VllmPoller::default();
        let now = Instant::now();
        let s = p.sample_from(&Metrics::parse(&scrape(122295.0, 4728.24, 1184.0, 3.0)), &config(), now);

        assert_eq!(s.get("running"), Some(3.0));
        assert_eq!(s.get("kv"), Some(12.0));
        assert_eq!(s.get("tps"), None, "a rate needs an interval");
        // 1886.42 / 1194 = 1.58s, the live lifetime average.
        let ttft = s.get("ttft").unwrap();
        assert!((ttft - 1.58).abs() < 0.01, "ttft was {ttft}");
        // 85983760 / 87238341 = 98.6%
        let prefix = s.get("prefix").unwrap();
        assert!((prefix - 98.56).abs() < 0.1, "prefix was {prefix}");
        assert_eq!(s.model.as_deref(), Some("qwen3.8-27b"));
    }

    #[test]
    fn decode_speed_divides_by_decode_time_not_wall_clock() {
        let mut p = VllmPoller::default();
        let t0 = Instant::now();
        p.sample_from(&Metrics::parse(&scrape(1000.0, 100.0, 10.0, 0.0)), &config(), t0);

        // 60 seconds later: 600 more tokens, but only 10s of them spent
        // generating. Wall-clock throughput is 10/s; decode speed is 60/s.
        let t1 = t0 + Duration::from_secs(60);
        let s = p.sample_from(&Metrics::parse(&scrape(1600.0, 110.0, 20.0, 1.0)), &config(), t1);

        assert!((s.get("tps").unwrap() - 60.0).abs() < 0.001);
        assert!((s.get("tps_wall").unwrap() - 10.0).abs() < 0.001);
        assert!((s.get("reqmin").unwrap() - 10.0).abs() < 0.001);
    }

    #[test]
    fn idle_interval_reports_zero_rate_but_keeps_the_last_latency() {
        let mut p = VllmPoller::default();
        let t0 = Instant::now();
        p.sample_from(&Metrics::parse(&scrape(1000.0, 100.0, 10.0, 0.0)), &config(), t0);
        let t1 = t0 + Duration::from_secs(10);
        let s = p.sample_from(&Metrics::parse(&scrape(1000.0, 100.0, 10.0, 0.0)), &config(), t1);

        assert_eq!(s.get("tps"), Some(0.0));
        assert_eq!(s.get("reqmin"), Some(0.0));
        // TTFT is a property of requests, not of time -- it holds.
        assert!(s.get("ttft").is_some());
    }

    #[test]
    fn a_restart_re_baselines_instead_of_reporting_a_spike() {
        let mut p = VllmPoller::default();
        let t0 = Instant::now();
        p.sample_from(&Metrics::parse(&scrape(122295.0, 4728.0, 1184.0, 0.0)), &config(), t0);

        // Engine restarted: every counter back near zero.
        let t1 = t0 + Duration::from_secs(10);
        let s = p.sample_from(&Metrics::parse(&scrape(12.0, 1.0, 1.0, 0.0)), &config(), t1);
        assert_eq!(s.get("tps"), None, "no bogus rate across a restart");

        // ...and the next interval works normally again from the new baseline.
        let t2 = t1 + Duration::from_secs(10);
        let s = p.sample_from(&Metrics::parse(&scrape(112.0, 3.0, 2.0, 0.0)), &config(), t2);
        assert_eq!(s.get("tps"), Some(50.0));
    }

    #[test]
    fn missing_metrics_are_absent_not_zero() {
        let mut p = VllmPoller::default();
        let s = p.sample_from(&Metrics::parse("vllm:num_requests_running{engine=\"0\"} 1.0\n"), &config(), Instant::now());
        assert_eq!(s.get("running"), Some(1.0));
        assert_eq!(s.get("kv"), None);
        assert_eq!(s.get("reqs"), None);
    }

    #[test]
    fn falls_back_to_older_metric_names() {
        // No `by_source` and no `request_time_per_output_token`, as on older
        // vLLM builds.
        let body = concat!(
            "vllm:prompt_tokens_total{engine=\"0\"} 1000.0\n",
            "vllm:prompt_tokens_cached_total{engine=\"0\"} 400.0\n",
            "vllm:inter_token_latency_seconds_sum{engine=\"0\"} 10.0\n",
            "vllm:inter_token_latency_seconds_count{engine=\"0\"} 100.0\n",
        );
        let snap = read_snapshot(&Metrics::parse(body));
        assert_eq!(snap.prefill_tokens, Some(600.0));
        assert_eq!(snap.tpot_sum, Some(10.0));
        assert_eq!(snap.tpot_count, Some(100.0));
    }
}
