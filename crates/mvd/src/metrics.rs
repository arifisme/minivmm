use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry,
    TextEncoder,
};

const LATENCY_BUCKETS: &[f64] = &[
    0.0005, 0.001, 0.002, 0.005, 0.01, 0.02, 0.035, 0.05, 0.075, 0.1, 0.15, 0.25, 0.5, 1.0, 2.5,
    5.0, 10.0, 30.0,
];

pub struct Metrics {
    registry: Registry,
    pub invocations: IntCounterVec,
    pub errors: IntCounterVec,
    pub timeouts: IntCounterVec,
    pub throttled: IntCounterVec,
    pub duration: HistogramVec,
    pub cold_start: HistogramVec,
    pub restore: HistogramVec,
    pub deployments: IntCounterVec,
    pub pool: IntGaugeVec,
    pub memory_limit: IntGauge,
    pub memory_reserved: IntGauge,
    pub memory_available: IntGauge,
    pub vcpus_busy: IntGauge,
    pub vcpu_limit: IntGauge,
}

fn counter(registry: &Registry, name: &str, help: &str, labels: &[&str]) -> IntCounterVec {
    let metric = IntCounterVec::new(Opts::new(name, help), labels).expect("valid counter");
    registry
        .register(Box::new(metric.clone()))
        .expect("unique counter");
    metric
}

fn histogram(registry: &Registry, name: &str, help: &str, labels: &[&str]) -> HistogramVec {
    let opts = HistogramOpts::new(name, help).buckets(LATENCY_BUCKETS.to_vec());
    let metric = HistogramVec::new(opts, labels).expect("valid histogram");
    registry
        .register(Box::new(metric.clone()))
        .expect("unique histogram");
    metric
}

fn gauge(registry: &Registry, name: &str, help: &str) -> IntGauge {
    let metric = IntGauge::new(name, help).expect("valid gauge");
    registry
        .register(Box::new(metric.clone()))
        .expect("unique gauge");
    metric
}

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();
        let pool = IntGaugeVec::new(
            Opts::new("mvd_pool_vms", "VMs per function by state"),
            &["function", "state"],
        )
        .expect("valid gauge");
        registry
            .register(Box::new(pool.clone()))
            .expect("unique gauge");
        Self {
            invocations: counter(
                &registry,
                "mvd_invocations_total",
                "Invocations by function and start type",
                &["function", "start"],
            ),
            errors: counter(
                &registry,
                "mvd_invocation_errors_total",
                "Failed invocations by function and error kind",
                &["function", "kind"],
            ),
            timeouts: counter(
                &registry,
                "mvd_invocation_timeouts_total",
                "Invocations that exceeded the function timeout",
                &["function"],
            ),
            throttled: counter(
                &registry,
                "mvd_throttled_total",
                "Requests refused with 429 by function and reason",
                &["function", "reason"],
            ),
            duration: histogram(
                &registry,
                "mvd_invocation_duration_seconds",
                "Request to response time of invocations",
                &["function", "start"],
            ),
            cold_start: histogram(
                &registry,
                "mvd_cold_start_seconds",
                "Time to obtain a ready VM on a cold start",
                &["function"],
            ),
            restore: histogram(
                &registry,
                "mvd_restore_seconds",
                "Time from spawning the VMM to a synchronized agent",
                &["runtime"],
            ),
            deployments: counter(
                &registry,
                "mvd_deployments_total",
                "Deployments by result",
                &["result"],
            ),
            memory_limit: gauge(
                &registry,
                "mvd_host_memory_limit_bytes",
                "Memory VMs may reserve",
            ),
            memory_reserved: gauge(
                &registry,
                "mvd_host_memory_reserved_bytes",
                "Memory reserved by live VMs",
            ),
            memory_available: gauge(
                &registry,
                "mvd_host_memory_available_bytes",
                "MemAvailable reported by the host",
            ),
            vcpus_busy: gauge(
                &registry,
                "mvd_host_vcpus_busy",
                "vCPUs serving invocations",
            ),
            vcpu_limit: gauge(
                &registry,
                "mvd_host_vcpu_limit",
                "vCPUs that may serve at once",
            ),
            pool,
            registry,
        }
    }

    pub fn render(&self) -> String {
        let mut buffer = Vec::new();
        let _ = TextEncoder::new().encode(&self.registry.gather(), &mut buffer);
        String::from_utf8(buffer).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_registered_metrics() {
        let metrics = Metrics::new();
        metrics
            .invocations
            .with_label_values(&["hello", "cold"])
            .inc();
        metrics
            .duration
            .with_label_values(&["hello", "cold"])
            .observe(0.012);
        metrics.pool.with_label_values(&["hello", "idle"]).set(2);
        metrics.memory_limit.set(1 << 30);
        let text = metrics.render();
        assert!(text.contains(r#"mvd_invocations_total{function="hello",start="cold"} 1"#));
        assert!(text.contains(
            r#"mvd_invocation_duration_seconds_bucket{function="hello",start="cold",le="0.02"} 1"#
        ));
        assert!(text.contains(r#"mvd_pool_vms{function="hello",state="idle"} 2"#));
        assert!(text.contains("mvd_host_memory_limit_bytes 1073741824"));
    }
}
