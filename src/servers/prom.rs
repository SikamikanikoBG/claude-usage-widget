// A deliberately small parser for the Prometheus text exposition format.
//
// Only the subset an inference server actually emits is handled: `# HELP` /
// `# TYPE` comment lines, and sample lines of the form
//
//     name{label="value",other="value"} 123.45
//     name 123.45
//     name{...} 123.45 1699999999000
//
// That's enough for vLLM's `/metrics`, and it's ~150 lines instead of a new
// dependency tree. The widget is a single-binary tray app whose whole point
// is being cheap to run; pulling in a full Prometheus client crate (which
// exists to *expose* metrics, not to consume them) to read a dozen numbers
// would be the wrong trade.
//
// Label handling matters more than it looks: vLLM tags every sample with at
// least `model_name` and `engine`, and a multi-engine server emits the SAME
// metric once per engine. Anything that reads a single sample and stops
// would silently under-report on exactly the setups where throughput numbers
// matter most, so the accessors here are sum-across-label-sets by default --
// with `mean` kept separate for ratio gauges, where summing is nonsense.

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub labels: Vec<(String, String)>,
    pub value: f64,
}

impl Sample {
    pub fn label(&self, name: &str) -> Option<&str> {
        self.labels
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Default, Clone)]
pub struct Metrics {
    by_name: HashMap<String, Vec<Sample>>,
}

impl Metrics {
    pub fn parse(text: &str) -> Self {
        let mut by_name: HashMap<String, Vec<Sample>> = HashMap::new();

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((name, sample)) = parse_line(line) else {
                continue;
            };
            by_name.entry(name).or_default().push(sample);
        }

        Self { by_name }
    }

    pub fn samples(&self, name: &str) -> &[Sample] {
        self.by_name.get(name).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Sum of every sample of `name`, across all label sets. `None` when the
    /// metric isn't present at all -- which is a meaningfully different state
    /// from "present and zero" (an unsupported or renamed metric vs an idle
    /// server), and the callers rely on being able to tell them apart.
    pub fn sum(&self, name: &str) -> Option<f64> {
        let samples = self.by_name.get(name)?;
        let total: f64 = samples
            .iter()
            .map(|s| s.value)
            .filter(|v| v.is_finite())
            .sum();
        Some(total)
    }

    /// Like [`Metrics::sum`], but only over samples whose `label` equals
    /// `value` (e.g. `finished_reason="error"`).
    pub fn sum_where(&self, name: &str, label: &str, value: &str) -> Option<f64> {
        let samples = self.by_name.get(name)?;
        let total: f64 = samples
            .iter()
            .filter(|s| s.label(label) == Some(value))
            .map(|s| s.value)
            .filter(|v| v.is_finite())
            .sum();
        Some(total)
    }

    /// Mean of every sample of `name`. The right accessor for a *ratio* gauge
    /// like `kv_cache_usage_perc`, where summing two engines that are each
    /// 60% full would produce a nonsensical 120%.
    pub fn mean(&self, name: &str) -> Option<f64> {
        let samples = self.by_name.get(name)?;
        let vals: Vec<f64> = samples
            .iter()
            .map(|s| s.value)
            .filter(|v| v.is_finite())
            .collect();
        if vals.is_empty() {
            return None;
        }
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }

    /// Distinct values seen for `label` across all samples of `name`, in
    /// first-seen order. Used to name the model(s) a server is serving.
    pub fn label_values(&self, name: &str, label: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for sample in self.samples(name) {
            if let Some(v) = sample.label(label) {
                if !out.iter().any(|existing| existing == v) {
                    out.push(v.to_string());
                }
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

/// Splits one sample line into `(metric name, sample)`.
///
/// Returns `None` rather than erroring for anything unparseable: a metrics
/// endpoint that grows a line shape we don't understand should cost us that
/// one line, never the whole scrape.
fn parse_line(line: &str) -> Option<(String, Sample)> {
    if let Some(brace) = line.find('{') {
        let name = line[..brace].trim().to_string();
        let close = line.rfind('}')?;
        if close < brace {
            return None;
        }
        let labels = parse_labels(&line[brace + 1..close]);
        return finish(name, labels, line[close + 1..].trim());
    }

    let mut parts = line.splitn(2, char::is_whitespace);
    let name = parts.next()?.to_string();
    let rest = parts.next()?;
    finish(name, Vec::new(), rest.trim())
}

fn finish(
    name: String,
    labels: Vec<(String, String)>,
    value_part: &str,
) -> Option<(String, Sample)> {
    // A trailing millisecond timestamp is legal and ignorable; take the
    // first whitespace-separated token as the value.
    let raw = value_part.split_whitespace().next()?;
    let value: f64 = raw.parse().ok()?;
    Some((name, Sample { labels, value }))
}

/// Parses `a="1",b="2"` into pairs, honouring the format's escape sequences
/// inside quoted values so a label value that itself contains a comma or a
/// quote can't shift every following pair.
fn parse_labels(input: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut chars = input.chars().peekable();

    loop {
        let mut key = String::new();
        let mut saw_equals = false;
        while let Some(c) = chars.next() {
            if c == '=' {
                saw_equals = true;
                break;
            }
            if !c.is_whitespace() && c != ',' {
                key.push(c);
            }
        }
        if key.is_empty() || !saw_equals {
            break;
        }

        // Opening quote (tolerated as absent rather than bailing out).
        if chars.peek() == Some(&'"') {
            chars.next();
        }

        let mut value = String::new();
        let mut escaped = false;
        for c in chars.by_ref() {
            if escaped {
                value.push(match c {
                    'n' => '\n',
                    other => other,
                });
                escaped = false;
                continue;
            }
            match c {
                '\\' => escaped = true,
                '"' => break,
                other => value.push(other),
            }
        }

        out.push((key, value));

        while let Some(&c) = chars.peek() {
            if c == ',' || c.is_whitespace() {
                chars.next();
            } else {
                break;
            }
        }
        if chars.peek().is_none() {
            break;
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = concat!(
        "# HELP vllm:num_requests_running Number of requests running.\n",
        "# TYPE vllm:num_requests_running gauge\n",
        "vllm:num_requests_running{engine=\"0\",model_name=\"qwen3.8-27b\"} 3.0\n",
        "vllm:generation_tokens_total{engine=\"0\",model_name=\"qwen3.8-27b\"} 122295.0\n",
        "vllm:generation_tokens_total{engine=\"1\",model_name=\"qwen3.8-27b\"} 1000.0\n",
        "vllm:request_success_total{engine=\"0\",finished_reason=\"stop\"} 1184.0\n",
        "vllm:request_success_total{engine=\"0\",finished_reason=\"error\"} 7.0\n",
        "vllm:kv_cache_usage_perc{engine=\"0\"} 0.5\n",
        "vllm:kv_cache_usage_perc{engine=\"1\"} 0.1\n",
        "vllm:prompt_tokens_created{engine=\"0\"} 1.787060873165574e+09\n",
        "bare_metric 42\n",
        "with_timestamp{a=\"b\"} 7 1699999999000\n",
    );

    #[test]
    fn sums_across_engines() {
        let m = Metrics::parse(SAMPLE);
        assert_eq!(m.sum("vllm:generation_tokens_total"), Some(123295.0));
    }

    #[test]
    fn filters_by_label() {
        let m = Metrics::parse(SAMPLE);
        assert_eq!(
            m.sum_where("vllm:request_success_total", "finished_reason", "error"),
            Some(7.0)
        );
        assert_eq!(
            m.sum_where("vllm:request_success_total", "finished_reason", "stop"),
            Some(1184.0)
        );
    }

    #[test]
    fn ratio_gauges_are_averaged_not_summed() {
        let m = Metrics::parse(SAMPLE);
        // Summing a 50%-full and a 10%-full engine into 60% would be nonsense.
        assert_eq!(m.mean("vllm:kv_cache_usage_perc"), Some(0.3));
    }

    #[test]
    fn missing_metric_is_none_not_zero() {
        let m = Metrics::parse(SAMPLE);
        assert_eq!(m.sum("vllm:not_a_metric"), None);
    }

    #[test]
    fn handles_bare_timestamped_and_scientific_lines() {
        let m = Metrics::parse(SAMPLE);
        assert_eq!(m.sum("bare_metric"), Some(42.0));
        assert_eq!(m.sum("with_timestamp"), Some(7.0));
        assert_eq!(m.sum("vllm:prompt_tokens_created"), Some(1787060873.165574));
    }

    #[test]
    fn reads_label_values() {
        let m = Metrics::parse(SAMPLE);
        assert_eq!(
            m.label_values("vllm:generation_tokens_total", "model_name"),
            vec!["qwen3.8-27b".to_string()]
        );
    }

    #[test]
    fn escaped_label_values_do_not_shift_the_pairs() {
        let m = Metrics::parse("x{a=\"he said \\\"hi\\\", ok\",b=\"2\"} 1");
        let s = &m.samples("x")[0];
        assert_eq!(s.label("a"), Some("he said \"hi\", ok"));
        assert_eq!(s.label("b"), Some("2"));
    }

    #[test]
    fn comment_lines_are_not_parsed_as_samples() {
        let m = Metrics::parse(SAMPLE);
        assert!(m.samples("# HELP").is_empty());
        assert!(!m.is_empty());
    }
}
