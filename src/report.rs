//! What a run measured, and how it is shown.

use crate::client::{Outcome, Sample};
use crate::hist::Histogram;
use crate::json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

/// Everything counted during the measured part of a run. Latencies are in microseconds.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    /// Time to first token, successful requests only.
    pub ttft: Histogram,
    /// Gaps between tokens.
    pub itl: Histogram,
    /// Intended start to end of stream, successful requests only.
    pub e2e: Histogram,
    /// How late requests started compared with their intended start.
    pub lag: Histogram,
    pub ok: u64,
    pub tokens: u64,
    /// Failed requests by kind.
    pub errors: BTreeMap<String, u64>,
    /// Requests the generator had to drop because too many were already waiting.
    pub dropped: u64,
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

fn error_kind(outcome: &Outcome) -> Option<String> {
    match outcome {
        Outcome::Ok => None,
        Outcome::Status(code) => Some(format!("status {code}")),
        Outcome::Timeout => Some("timeout".to_string()),
        Outcome::Failed(why) => {
            let short: String = why.chars().take(60).collect();
            Some(format!("failed: {short}"))
        }
    }
}

impl Stats {
    pub fn record(&mut self, sample: &Sample) {
        self.lag.record(micros(sample.lag));
        if let Some(kind) = error_kind(&sample.outcome) {
            *self.errors.entry(kind).or_insert(0) += 1;
            return;
        }
        self.ok += 1;
        self.tokens += sample.tokens;
        self.e2e.record(micros(sample.e2e));
        if let Some(ttft) = sample.ttft {
            self.ttft.record(micros(ttft));
        }
        for gap in &sample.itl {
            self.itl.record(micros(*gap));
        }
    }

    pub fn merge(&mut self, other: &Stats) {
        self.ttft.merge(&other.ttft);
        self.itl.merge(&other.itl);
        self.e2e.merge(&other.e2e);
        self.lag.merge(&other.lag);
        self.ok += other.ok;
        self.tokens += other.tokens;
        self.dropped += other.dropped;
        for (kind, n) in &other.errors {
            *self.errors.entry(kind.clone()).or_insert(0) += n;
        }
    }

    pub fn failed(&self) -> u64 {
        self.errors.values().sum()
    }
}

#[derive(Clone, Debug)]
pub struct Report {
    /// One line describing the run.
    pub title: String,
    /// The rate asked for, for an open-loop run.
    pub target_rate: Option<f64>,
    /// Requests that were meant to start in the measured window.
    pub offered: u64,
    /// Length of the measured window.
    pub window: Duration,
    /// From the start of the window to the last answer to a request meant to start in it. Longer
    /// than the window when the server was still working through a backlog afterwards.
    pub span: Duration,
    pub stats: Stats,
}

const PERCENTILES: [(&str, f64); 4] = [("p50", 0.5), ("p90", 0.9), ("p99", 0.99), ("p99.9", 0.999)];

fn ms(micros: u64) -> f64 {
    micros as f64 / 1000.0
}

impl Report {
    /// Successful requests per second of the measured window.
    pub fn rate(&self) -> f64 {
        self.stats.ok as f64 / self.span.as_secs_f64()
    }

    pub fn tokens_per_s(&self) -> f64 {
        self.stats.tokens as f64 / self.span.as_secs_f64()
    }

    fn row(name: &str, h: &Histogram) -> String {
        let mut line = format!("{name:<11}");
        if h.is_empty() {
            line.push_str("   (none)");
            return line;
        }
        for (_, q) in PERCENTILES {
            let _ = write!(line, "{:>10.1}", ms(h.value_at_quantile(q)));
        }
        let _ = write!(line, "{:>10.1}", ms(h.max()));
        line
    }

    pub fn text(&self) -> String {
        let s = &self.stats;
        let mut out = String::new();
        let _ = writeln!(out, "{}", self.title);
        let failed = s.failed();
        let _ = write!(
            out,
            "requests    offered {}  ok {}  failed {failed}  dropped {}",
            self.offered, s.ok, s.dropped
        );
        if failed > 0 {
            let kinds: Vec<String> = s.errors.iter().map(|(k, n)| format!("{k}: {n}")).collect();
            let _ = write!(out, " ({})", kinds.join(", "));
        }
        out.push('\n');
        let _ = write!(
            out,
            "throughput  {:.1} req/s  {:.0} tokens/s",
            self.rate(),
            self.tokens_per_s()
        );
        if let Some(target) = self.target_rate {
            let _ = write!(out, "   (target {target:.1} req/s)");
        }
        out.push('\n');
        if self.span.as_secs_f64() > self.window.as_secs_f64() * 1.1 {
            let _ = writeln!(
                out,
                "            the server needed {:.1} s, not {:.1} s, to answer them: it was behind",
                self.span.as_secs_f64(),
                self.window.as_secs_f64()
            );
        }
        out.push('\n');
        let _ = write!(out, "{:<11}", "latency ms");
        for (name, _) in PERCENTILES {
            let _ = write!(out, "{name:>10}");
        }
        let _ = writeln!(out, "{:>10}", "max");
        for (name, h) in [("ttft", &s.ttft), ("itl", &s.itl), ("e2e", &s.e2e)] {
            let _ = writeln!(out, "{}", Self::row(name, h));
        }
        out.push('\n');
        let _ = writeln!(out, "{}", Self::row("start lag", &s.lag));
        if s.lag.value_at_quantile(0.99) > 50_000 {
            out.push_str(
                "warning: requests started late (start lag above 50 ms at p99). The generator may\n\
                 be the bottleneck: raise --connections. Latencies include that wait, as they should.\n",
            );
        }
        if s.dropped > 0 {
            let _ = writeln!(
                out,
                "warning: {} requests were dropped because --max-queue was reached; the load \
                 offered to the server was lower than asked.",
                s.dropped
            );
        }
        if failed > 0 {
            out.push_str("note: latency percentiles cover successful requests only.\n");
        }
        out
    }

    pub fn json(&self) -> Value {
        let s = &self.stats;
        let percentiles = |h: &Histogram| {
            let mut members: Vec<(String, Value)> = PERCENTILES
                .iter()
                .map(|(name, q)| ((*name).to_string(), ms(h.value_at_quantile(*q)).into()))
                .collect();
            members.push(("max".to_string(), ms(h.max()).into()));
            members.push(("count".to_string(), h.len().into()));
            Value::Obj(members)
        };
        let errors = Value::Obj(
            s.errors
                .iter()
                .map(|(k, n)| (k.clone(), (*n).into()))
                .collect(),
        );
        Value::obj([
            ("title", self.title.as_str().into()),
            (
                "target_rate",
                self.target_rate.map_or(Value::Null, Into::into),
            ),
            ("window_s", self.window.as_secs_f64().into()),
            ("span_s", self.span.as_secs_f64().into()),
            ("offered", self.offered.into()),
            ("ok", s.ok.into()),
            ("failed", s.failed().into()),
            ("dropped", s.dropped.into()),
            ("errors", errors),
            ("rate", self.rate().into()),
            ("tokens_per_s", self.tokens_per_s().into()),
            ("ttft_ms", percentiles(&s.ttft)),
            ("itl_ms", percentiles(&s.itl)),
            ("e2e_ms", percentiles(&s.e2e)),
            ("start_lag_ms", percentiles(&s.lag)),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(
        outcome: Outcome,
        e2e_ms: u64,
        ttft_ms: Option<u64>,
        itl_ms: &[u64],
        tokens: u64,
    ) -> Sample {
        Sample {
            outcome,
            ttft: ttft_ms.map(Duration::from_millis),
            itl: itl_ms.iter().map(|&m| Duration::from_millis(m)).collect(),
            e2e: Duration::from_millis(e2e_ms),
            tokens,
            lag: Duration::from_millis(2),
        }
    }

    fn good(e2e_ms: u64) -> Sample {
        sample(Outcome::Ok, e2e_ms, Some(30), &[10, 12], 3)
    }

    #[test]
    fn a_successful_sample_feeds_every_histogram() {
        let mut stats = Stats::default();
        stats.record(&good(100));
        assert_eq!((stats.ok, stats.tokens, stats.failed()), (1, 3, 0));
        assert_eq!(
            (
                stats.e2e.len(),
                stats.ttft.len(),
                stats.itl.len(),
                stats.lag.len()
            ),
            (1, 1, 2, 1)
        );
        assert_eq!(stats.e2e.max(), 100_000);
        assert_eq!(stats.ttft.max(), 30_000);
    }

    #[test]
    fn failures_are_counted_by_kind_and_stay_out_of_the_latencies() {
        let mut stats = Stats::default();
        stats.record(&sample(Outcome::Status(503), 5, None, &[], 0));
        stats.record(&sample(Outcome::Status(503), 5, None, &[], 0));
        stats.record(&sample(Outcome::Timeout, 30_000, Some(10), &[1], 4));
        stats.record(&sample(
            Outcome::Failed("connect: refused".to_string()),
            1,
            None,
            &[],
            0,
        ));
        assert_eq!(stats.ok, 0);
        assert_eq!(stats.failed(), 4);
        assert_eq!(stats.errors["status 503"], 2);
        assert_eq!(stats.errors["timeout"], 1);
        assert_eq!(stats.errors["failed: connect: refused"], 1);
        assert!(stats.e2e.is_empty() && stats.ttft.is_empty() && stats.itl.is_empty());
        assert_eq!(
            stats.lag.len(),
            4,
            "lag is a property of the generator, whatever happened"
        );
        assert_eq!(stats.tokens, 0);
    }

    #[test]
    fn long_failure_messages_are_cut_so_kinds_do_not_multiply() {
        let mut stats = Stats::default();
        stats.record(&sample(Outcome::Failed("x".repeat(500)), 1, None, &[], 0));
        let key = stats.errors.keys().next().unwrap();
        assert_eq!(key.chars().count(), "failed: ".len() + 60);
    }

    #[test]
    fn merging_adds_everything() {
        let (mut a, mut b) = (Stats::default(), Stats::default());
        a.record(&good(100));
        a.record(&sample(Outcome::Timeout, 1, None, &[], 0));
        b.record(&good(300));
        b.record(&sample(Outcome::Timeout, 1, None, &[], 0));
        b.record(&sample(Outcome::Status(500), 1, None, &[], 0));
        b.dropped = 7;
        a.merge(&b);
        assert_eq!((a.ok, a.tokens, a.dropped, a.failed()), (2, 6, 7, 3));
        assert_eq!(a.errors["timeout"], 2);
        assert_eq!((a.e2e.len(), a.e2e.max()), (2, 300_000));
    }

    fn report(stats: Stats) -> Report {
        Report {
            title: "llm-loadgen  test run".to_string(),
            target_rate: Some(10.0),
            offered: 20,
            window: Duration::from_secs(2),
            span: Duration::from_secs(2),
            stats,
        }
    }

    #[test]
    fn rates_are_per_second_of_the_window() {
        let mut stats = Stats::default();
        for _ in 0..20 {
            stats.record(&good(100));
        }
        let r = report(stats);
        assert!((r.rate() - 10.0).abs() < 1e-9);
        assert!((r.tokens_per_s() - 30.0).abs() < 1e-9);
    }

    #[test]
    fn a_server_that_was_behind_is_judged_over_the_time_it_took() {
        let mut stats = Stats::default();
        for _ in 0..20 {
            stats.record(&good(100));
        }
        let mut r = report(stats);
        assert!(!r.text().contains("it was behind"));
        r.span = Duration::from_secs(4);
        assert!((r.rate() - 5.0).abs() < 1e-9, "20 answers in 4 s");
        assert!(r
            .text()
            .contains("the server needed 4.0 s, not 2.0 s, to answer them: it was behind"));
        assert_eq!(r.json().get("span_s").and_then(Value::as_f64), Some(4.0));
    }

    #[test]
    fn the_text_report_shows_counts_percentiles_and_the_target() {
        let mut stats = Stats::default();
        for ms in 1..=100 {
            stats.record(&good(ms));
        }
        let text = report(stats).text();
        assert!(text.starts_with("llm-loadgen  test run\n"));
        assert!(
            text.contains("requests    offered 20  ok 100  failed 0  dropped 0"),
            "{text}"
        );
        assert!(text.contains("(target 10.0 req/s)"));
        let e2e = text.lines().find(|l| l.starts_with("e2e")).unwrap();
        let cells: Vec<&str> = e2e.split_whitespace().collect();
        assert_eq!(cells[0], "e2e");
        assert_eq!(cells.len(), 6, "name, p50, p90, p99, p99.9, max: {e2e}");
        assert!((cells[1].parse::<f64>().unwrap() - 50.0).abs() < 1.0);
        assert!((cells[2].parse::<f64>().unwrap() - 90.0).abs() < 1.0);
        assert_eq!(cells[5], "100.0");
        assert!(!text.contains("warning"));
        assert!(!text.contains("note:"));
    }

    #[test]
    fn empty_histograms_say_none() {
        let text = report(Stats::default()).text();
        assert!(text
            .lines()
            .find(|l| l.starts_with("e2e"))
            .unwrap()
            .contains("(none)"));
    }

    #[test]
    fn failures_dropped_requests_and_late_starts_come_with_a_warning() {
        let mut stats = Stats::default();
        let mut late = good(100);
        late.lag = Duration::from_millis(120);
        for _ in 0..10 {
            stats.record(&late);
        }
        stats.record(&sample(Outcome::Status(503), 1, None, &[], 0));
        stats.dropped = 4;
        let text = report(stats).text();
        assert!(
            text.contains("failed 1  dropped 4 (status 503: 1)"),
            "{text}"
        );
        assert!(text.contains("warning: requests started late"));
        assert!(text.contains("warning: 4 requests were dropped"));
        assert!(text.contains("note: latency percentiles cover successful requests only."));
    }

    #[test]
    fn the_json_report_has_the_same_numbers() {
        let mut stats = Stats::default();
        for ms in 1..=100 {
            stats.record(&good(ms));
        }
        stats.record(&sample(Outcome::Timeout, 1, None, &[], 0));
        let json = report(stats).json();
        let text = json.to_string();
        let parsed = crate::json::parse(&text).unwrap();
        assert_eq!(parsed, json);
        assert_eq!(parsed.get("ok").and_then(Value::as_u64), Some(100));
        assert_eq!(parsed.get("failed").and_then(Value::as_u64), Some(1));
        assert_eq!(
            parsed.get("target_rate").and_then(Value::as_f64),
            Some(10.0)
        );
        let e2e = parsed.get("e2e_ms").unwrap();
        assert_eq!(e2e.get("count").and_then(Value::as_u64), Some(100));
        assert!((e2e.get("p50").and_then(Value::as_f64).unwrap() - 50.0).abs() < 1.0);
        assert_eq!(e2e.get("max").and_then(Value::as_f64), Some(100.0));
        assert_eq!(
            parsed
                .get("errors")
                .and_then(|e| e.get("timeout"))
                .and_then(Value::as_u64),
            Some(1)
        );
    }

    #[test]
    fn a_closed_loop_run_has_no_target_in_the_json() {
        let mut r = report(Stats::default());
        r.target_rate = None;
        assert_eq!(r.json().get("target_rate"), Some(&Value::Null));
        assert!(!r.text().contains("target"));
    }
}
