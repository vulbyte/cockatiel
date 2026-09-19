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
    pub msgs_per_min_projected: f64,
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
            "msgs_per_min_projected": self.msgs_per_min_projected,
            "notes": self.notes,
        })
    }
}