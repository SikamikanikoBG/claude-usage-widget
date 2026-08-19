// Ollama provider.
//
// Ollama publishes no aggregate metrics whatsoever -- verified against 0.32.14
// on 2026-08-19: `/metrics` is 404 and so is `/debug/vars`. Per-request
// timings (`eval_count`, `eval_duration`, ...) exist only inside individual
// response bodies, which a passive observer never sees. So unlike vLLM, there
// is no honest way to read live throughput off a running Ollama.
//
// What this provider does instead:
//
// * `/api/ps` -> which models are resident, how much VRAM they hold. That
//   also doubles as the reachability check, so the normal path is one request.
// * `/api/tags` -> how many models are installed locally.
// * An **opt-in** synthetic probe (`probe.enabled` in servers.json) that asks
//   the server to generate a handful of tokens and reads the timings back out
//   of the response. This is the only route to a real tokens/sec number, and
//   it is load the widget itself creates -- so it is off by default, floored
//   at one probe per minute, and refuses to touch a model that isn't already
//   loaded. That last rule is the important one: without it, a tray widget
//   sitting open on a laptop could pull tens of gigabytes into a remote box's
//   VRAM (and evict whatever was there) purely as a side effect of being
//   open.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::{auth_token, get_text, ServerConfig, ServerKind, ServerSample};

#[derive(Deserialize)]
struct PsResponse {
    #[serde(default)]
    models: Vec<PsModel>,
}

#[derive(Deserialize)]
struct PsModel {
    #[serde(default)]
    name: String,
    #[serde(default)]
    size_vram: f64,
}

#[derive(Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagModel>,
}

#[derive(Deserialize)]
struct TagModel {
    #[serde(default)]
    #[allow(dead_code)]
    name: String,
}

#[derive(Deserialize)]
struct GenerateResponse {
    #[serde(default)]
    eval_count: f64,
    /// Nanoseconds.
    #[serde(default)]
    eval_duration: f64,
    #[serde(default)]
    prompt_eval_count: f64,
    #[serde(default)]
    prompt_eval_duration: f64,
}

#[derive(Default)]
pub struct OllamaPoller {
    last_probe: Option<Instant>,
    /// Kept between scrapes: a probed speed stays on screen until the next
    /// probe replaces it, the same way a latency average does for vLLM.
    last_tps: Option<f64>,
    last_prefill_tps: Option<f64>,
    last_ttft: Option<f64>,
}

impl OllamaPoller {
    pub fn poll(
        &mut self,
        client: &reqwest::blocking::Client,
        config: &ServerConfig,
    ) -> Result<ServerSample, String> {
        let ps_body = get_text(client, config, "/api/ps")?;
        let ps: PsResponse =
            serde_json::from_str(&ps_body).map_err(|e| format!("bad /api/ps ({e})"))?;

        let mut out: BTreeMap<&'static str, f64> = BTreeMap::new();
        out.insert("up", 1.0);
        out.insert("models", ps.models.len() as f64);
        out.insert(
            "vram",
            ps.models.iter().map(|m| m.size_vram).sum::<f64>() / 1_000_000_000.0,
        );

        // Installed-model count is nice to have, never worth failing over:
        // an Ollama that answered /api/ps is up regardless.
        if let Ok(body) = get_text(client, config, "/api/tags") {
            if let Ok(tags) = serde_json::from_str::<TagsResponse>(&body) {
                out.insert("installed", tags.models.len() as f64);
            }
        }

        let loaded: Vec<String> = ps.models.iter().map(|m| m.name.clone()).collect();

        if let Some(target) = self.probe_target(config, &loaded) {
            match self.run_probe(client, config, &target) {
                Ok(()) => {}
                Err(e) => eprintln!("[claude-usage-widget] {}: probe failed ({e})", config.name),
            }
            // Recorded whether it succeeded or not, so a failing probe
            // retries on its own schedule instead of on every scrape.
            self.last_probe = Some(Instant::now());
        }

        if let Some(v) = self.last_tps {
            out.insert("tps", v);
        }
        if let Some(v) = self.last_prefill_tps {
            out.insert("prefill_tps", v);
        }
        if let Some(v) = self.last_ttft {
            out.insert("ttft", v);
        }

        Ok(ServerSample {
            server: config.name.clone(),
            kind: ServerKind::Ollama,
            error: None,
            metrics: out,
            model: if loaded.is_empty() {
                None
            } else {
                Some(loaded.join(", "))
            },
        })
    }

    /// Which model (if any) to probe right now.
    ///
    /// Returns `None` unless probing is enabled, the interval has elapsed,
    /// AND a model is already resident. Nothing loaded means nothing to
    /// probe -- deliberately, see the module comment.
    fn probe_target(&self, config: &ServerConfig, loaded: &[String]) -> Option<String> {
        let probe = config.probe.as_ref()?;
        if !probe.enabled {
            return None;
        }
        let first = loaded.first()?;
        let due = match self.last_probe {
            None => true,
            Some(at) => at.elapsed() >= Duration::from_secs(probe.interval_secs),
        };
        due.then(|| first.clone())
    }

    fn run_probe(
        &mut self,
        client: &reqwest::blocking::Client,
        config: &ServerConfig,
        model: &str,
    ) -> Result<(), String> {
        let num_predict = config
            .probe
            .as_ref()
            .map(|p| p.num_predict)
            .unwrap_or(32);

        let body = serde_json::json!({
            "model": model,
            "prompt": "Reply with the single word: ok",
            "stream": false,
            "options": { "num_predict": num_predict, "temperature": 0 },
        });

        // Generous relative to a scrape: this one deliberately waits for a
        // model to actually generate. Still bounded, so a wedged server can't
        // stall the poll loop.
        let timeout = Duration::from_millis(config.timeout_ms.max(15_000));
        let mut req = client
            .post(format!("{}/api/generate", config.url))
            .timeout(timeout)
            .json(&body);
        if let Some(token) = auth_token(config) {
            req = req.bearer_auth(token);
        }

        let response = req.send().map_err(|e| super::describe(&e))?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status().as_u16()));
        }
        let parsed: GenerateResponse = response.json().map_err(|e| super::describe(&e))?;

        if parsed.eval_count > 0.0 && parsed.eval_duration > 0.0 {
            self.last_tps = Some(parsed.eval_count / (parsed.eval_duration / 1e9));
        }
        if parsed.prompt_eval_count > 0.0 && parsed.prompt_eval_duration > 0.0 {
            self.last_prefill_tps =
                Some(parsed.prompt_eval_count / (parsed.prompt_eval_duration / 1e9));
            // Prompt evaluation is what happens before the first token, so
            // it's the closest thing Ollama gives to a TTFT.
            self.last_ttft = Some(parsed.prompt_eval_duration / 1e9);
        }

        eprintln!(
            "[claude-usage-widget] {}: probed {} -> {:.1} tok/s",
            config.name,
            model,
            self.last_tps.unwrap_or(0.0)
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::servers::ProbeConfig;

    fn config(probe: Option<ProbeConfig>) -> ServerConfig {
        ServerConfig {
            name: "vader-ollama".to_string(),
            kind: ServerKind::Ollama,
            url: "http://vader:11434".to_string(),
            poll_secs: 30,
            timeout_ms: 2000,
            enabled: true,
            auth_env: None,
            probe,
        }
    }

    #[test]
    fn never_probes_when_nothing_is_loaded() {
        // The rule that stops a tray widget from pulling 20GB into a remote
        // box's VRAM by itself.
        let poller = OllamaPoller::default();
        let cfg = config(Some(ProbeConfig {
            enabled: true,
            interval_secs: 60,
            num_predict: 8,
        }));
        assert_eq!(poller.probe_target(&cfg, &[]), None);
        assert_eq!(
            poller.probe_target(&cfg, &["qwen3:8b".to_string()]),
            Some("qwen3:8b".to_string())
        );
    }

    #[test]
    fn never_probes_unless_explicitly_enabled() {
        let poller = OllamaPoller::default();
        assert_eq!(poller.probe_target(&config(None), &["m".to_string()]), None);
        let off = config(Some(ProbeConfig {
            enabled: false,
            interval_secs: 60,
            num_predict: 8,
        }));
        assert_eq!(poller.probe_target(&off, &["m".to_string()]), None);
    }

    #[test]
    fn respects_the_probe_interval() {
        let mut poller = OllamaPoller::default();
        poller.last_probe = Some(Instant::now());
        let cfg = config(Some(ProbeConfig {
            enabled: true,
            interval_secs: 300,
            num_predict: 8,
        }));
        assert_eq!(poller.probe_target(&cfg, &["m".to_string()]), None);
    }

    #[test]
    fn parses_a_real_ps_response() {
        let body = concat!(
            "{\"models\":[{\"name\":\"qwen3:8b\",\"model\":\"qwen3:8b\",",
            "\"size\":6000000000,\"size_vram\":5500000000,",
            "\"expires_at\":\"2026-08-19T12:00:00Z\"}]}"
        );
        let ps: PsResponse = serde_json::from_str(body).unwrap();
        assert_eq!(ps.models.len(), 1);
        assert_eq!(ps.models[0].name, "qwen3:8b");
        assert_eq!(ps.models[0].size_vram, 5_500_000_000.0);
    }

    #[test]
    fn parses_an_empty_ps_response() {
        // What vader actually returns with nothing warm.
        let ps: PsResponse = serde_json::from_str("{\"models\":[]}").unwrap();
        assert!(ps.models.is_empty());
    }

    #[test]
    fn probe_timings_convert_nanoseconds_to_tokens_per_second() {
        let body = concat!(
            "{\"eval_count\":32,\"eval_duration\":1000000000,",
            "\"prompt_eval_count\":10,\"prompt_eval_duration\":100000000}"
        );
        let r: GenerateResponse = serde_json::from_str(body).unwrap();
        assert_eq!(r.eval_count / (r.eval_duration / 1e9), 32.0);
        assert_eq!(r.prompt_eval_count / (r.prompt_eval_duration / 1e9), 100.0);
    }
}
