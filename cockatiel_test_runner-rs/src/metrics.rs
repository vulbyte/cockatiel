use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct Metrics {
    pub name: String,
    pub passed: u32,
    pub failed: u32,
    pub total_msgs: u64,
    pub duration_ms: u128,
    pub req_per_sec: f64,
    pub latency_p50_ms: f64,
    pub latency_p95_ms: f64,
    pub latency_p99_ms: f64,
    pub latency_min_ms: f64,
    pub latency_max_ms: f64,
    pub latency_avg_ms: f64,
    pub msgs_per_min_projected: f64,
    /// Per-test / per-module detail: `{ test, passed, failed, duration_ms,
    /// avg_ms, min_ms, max_ms, notes }` — how each module handled each test.
    pub detail: Vec<serde_json::Value>,
    pub notes: Vec<String>,
}

impl Metrics {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Default::default()
        }
    }

    pub fn record(&mut self, latencies: &[Duration]) {
        if latencies.is_empty() {
            return;
        }
        let mut sorted: Vec<f64> = latencies.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let p = |q: f64| -> f64 {
            if sorted.is_empty() {
                return 0.0;
            }
            let idx = ((sorted.len() as f64 - 1.0) * q).round() as usize;
            sorted[idx.min(sorted.len() - 1)]
        };
        self.latency_p50_ms = p(0.50);
        self.latency_p95_ms = p(0.95);
        self.latency_p99_ms = p(0.99);
        self.latency_min_ms = sorted[0];
        self.latency_max_ms = sorted[sorted.len() - 1];
        self.latency_avg_ms = sorted.iter().sum::<f64>() / sorted.len() as f64;
    }

    /// Record a single test outcome with its timing breakdown.
    #[allow(clippy::too_many_arguments)]
    pub fn push_detail(
        &mut self,
        test: impl Into<String>,
        passed: bool,
        duration_ms: u128,
        avg_ms: f64,
        min_ms: f64,
        max_ms: f64,
        note: impl Into<String>,
    ) {
        if passed {
            self.passed += 1;
        } else {
            self.failed += 1;
        }
        self.detail.push(serde_json::json!({
            "test": test.into(),
            "passed": passed,
            "failed": !passed,
            "duration_ms": duration_ms,
            "avg_ms": avg_ms,
            "min_ms": min_ms,
            "max_ms": max_ms,
            "notes": note.into(),
        }));
    }

    pub fn finalize(&mut self) {
        if self.duration_ms > 0 {
            self.req_per_sec = self.total_msgs as f64 / (self.duration_ms as f64 / 1000.0);
        }
        // 60 req/s is the "streamer chat" baseline: burst->sustained projection.
        // Messages per minute is a direct linear projection from observed req/s.
        self.msgs_per_min_projected = self.req_per_sec * 60.0;
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "name": self.name,
            "passed": self.passed,
            "failed": self.failed,
            "total_msgs": self.total_msgs,
            "duration_ms": self.duration_ms,
            "req_per_sec": self.req_per_sec,
            "latency_p50_ms": self.latency_p50_ms,
            "latency_p95_ms": self.latency_p95_ms,
            "latency_p99_ms": self.latency_p99_ms,
            "latency_min_ms": self.latency_min_ms,
            "latency_max_ms": self.latency_max_ms,
            "latency_avg_ms": self.latency_avg_ms,
            "msgs_per_min_projected": self.msgs_per_min_projected,
            "detail": self.detail,
            "notes": self.notes,
        })
    }
}