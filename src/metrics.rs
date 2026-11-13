//! In-memory only observability.  Nothing here is persisted; a restart resets
//! every counter, which is exactly the trade-off this gateway makes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

pub const LATENCY_BUCKETS_MS: [u64; 12] = [
    5, 10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000,
];

#[derive(Default)]
pub struct Metrics {
    pub requests: AtomicU64,
    pub errors: AtomicU64,
    pub active: AtomicU64,
    pub upstream_errors: AtomicU64,
    latency: [AtomicU64; 12],
    by_channel: Mutex<std::collections::BTreeMap<String, Arc<ChannelMetrics>>>,
}

#[derive(Default)]
pub struct ChannelMetrics {
    pub requests: AtomicU64,
    pub errors: AtomicU64,
}

impl Metrics {
    pub fn channel(&self, name: &str) -> Arc<ChannelMetrics> {
        let mut map = self.by_channel.lock();
        map.entry(name.to_string()).or_default().clone()
    }

    pub fn record(&self, channel: &str, status: u16, latency_ms: u64) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        if status >= 400 {
            self.errors.fetch_add(1, Ordering::Relaxed);
            self.channel(channel).errors.fetch_add(1, Ordering::Relaxed);
        }
        self.channel(channel)
            .requests
            .fetch_add(1, Ordering::Relaxed);
        for (index, bucket) in LATENCY_BUCKETS_MS.iter().enumerate() {
            if latency_ms <= *bucket {
                self.latency[index].fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("# HELP axonhub_rs_requests_total Requests handled.\n");
        out.push_str("# TYPE axonhub_rs_requests_total counter\n");
        out.push_str(&format!(
            "axonhub_rs_requests_total {}\n",
            self.requests.load(Ordering::Relaxed)
        ));
        out.push_str("# TYPE axonhub_rs_errors_total counter\n");
        out.push_str(&format!(
            "axonhub_rs_errors_total {}\n",
            self.errors.load(Ordering::Relaxed)
        ));
        out.push_str("# TYPE axonhub_rs_active_requests gauge\n");
        out.push_str(&format!(
            "axonhub_rs_active_requests {}\n",
            self.active.load(Ordering::Relaxed)
        ));
        out.push_str("# TYPE axonhub_rs_latency_ms histogram\n");
        let mut cumulative = 0u64;
        for (index, bucket) in LATENCY_BUCKETS_MS.iter().enumerate() {
            cumulative += self.latency[index].load(Ordering::Relaxed);
            out.push_str(&format!(
                "axonhub_rs_latency_ms_bucket{{le=\"{bucket}\"}} {cumulative}\n"
            ));
        }
        out.push_str(&format!(
            "axonhub_rs_latency_ms_count {}\n",
            self.requests.load(Ordering::Relaxed)
        ));
        for (name, channel) in self.by_channel.lock().iter() {
            out.push_str(&format!(
                "axonhub_rs_channel_requests_total{{channel=\"{name}\"}} {}\n",
                channel.requests.load(Ordering::Relaxed)
            ));
            out.push_str(&format!(
                "axonhub_rs_channel_errors_total{{channel=\"{name}\"}} {}\n",
                channel.errors.load(Ordering::Relaxed)
            ));
        }
        out
    }
}
