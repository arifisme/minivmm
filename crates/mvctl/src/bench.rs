use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use mv_proto::api::{HEADER_DURATION, HEADER_RESTORE, HEADER_START};
use reqwest::blocking::Client;
use serde::Serialize;

#[derive(Clone, Debug)]
pub struct Sample {
    pub status: u16,
    pub latency: Duration,
    pub server_ms: Option<f64>,
    pub restore_ms: Option<f64>,
    pub cold: bool,
}

#[derive(Debug, Default, PartialEq, Serialize)]
pub struct Percentiles {
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub max: f64,
    pub mean: f64,
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub requests: usize,
    pub concurrency: usize,
    pub seconds: f64,
    pub throughput: f64,
    pub errors: usize,
    pub throttled: usize,
    pub cold: usize,
    pub warm: usize,
    pub latency_ms: Percentiles,
    pub cold_latency_ms: Option<Percentiles>,
    pub warm_latency_ms: Option<Percentiles>,
    pub restore_ms: Option<Percentiles>,
    pub server_ms: Option<Percentiles>,
}

pub fn percentiles(values: &mut [f64]) -> Option<Percentiles> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let at = |fraction: f64| {
        let index = ((values.len() as f64 * fraction).ceil() as usize).clamp(1, values.len()) - 1;
        values[index]
    };
    Some(Percentiles {
        p50: at(0.50),
        p90: at(0.90),
        p99: at(0.99),
        max: values[values.len() - 1],
        mean: values.iter().sum::<f64>() / values.len() as f64,
    })
}

pub fn summarize(samples: &[Sample], concurrency: usize, elapsed: Duration) -> Summary {
    let millis = |filter: &dyn Fn(&Sample) -> bool| {
        samples
            .iter()
            .filter(|s| filter(s))
            .map(|s| s.latency.as_secs_f64() * 1000.0)
            .collect::<Vec<f64>>()
    };
    let ok = |s: &Sample| s.status == 200;
    let mut all = millis(&ok);
    let mut cold = millis(&|s| ok(s) && s.cold);
    let mut warm = millis(&|s| ok(s) && !s.cold);
    let mut restore: Vec<f64> = samples.iter().filter_map(|s| s.restore_ms).collect();
    let mut server: Vec<f64> = samples
        .iter()
        .filter(|s| ok(s))
        .filter_map(|s| s.server_ms)
        .collect();
    Summary {
        requests: samples.len(),
        concurrency,
        seconds: elapsed.as_secs_f64(),
        throughput: samples.iter().filter(|s| ok(s)).count() as f64
            / elapsed.as_secs_f64().max(1e-9),
        errors: samples
            .iter()
            .filter(|s| s.status != 200 && s.status != 429)
            .count(),
        throttled: samples.iter().filter(|s| s.status == 429).count(),
        cold: cold.len(),
        warm: warm.len(),
        latency_ms: percentiles(&mut all).unwrap_or_default(),
        cold_latency_ms: percentiles(&mut cold),
        warm_latency_ms: percentiles(&mut warm),
        restore_ms: percentiles(&mut restore),
        server_ms: percentiles(&mut server),
    }
}

pub struct Load<'a> {
    pub client: &'a Client,
    pub url: String,
    pub token: Option<String>,
    pub body: String,
    pub requests: usize,
    pub concurrency: usize,
}

fn header_ms(response: &reqwest::blocking::Response, name: &str) -> Option<f64> {
    response.headers().get(name)?.to_str().ok()?.parse().ok()
}

pub fn run(load: &Load<'_>) -> (Vec<Sample>, Duration) {
    let remaining = Arc::new(Mutex::new(load.requests));
    let samples = Arc::new(Mutex::new(Vec::with_capacity(load.requests)));
    let started = Instant::now();
    thread::scope(|scope| {
        for _ in 0..load.concurrency.max(1) {
            let remaining = remaining.clone();
            let samples = samples.clone();
            scope.spawn(move || {
                loop {
                    {
                        let mut left = remaining.lock().unwrap_or_else(|e| e.into_inner());
                        if *left == 0 {
                            return;
                        }
                        *left -= 1;
                    }
                    let mut request = load.client.post(&load.url).body(load.body.clone());
                    if let Some(token) = &load.token {
                        request = request.bearer_auth(token);
                    }
                    let begun = Instant::now();
                    let sample = match request.send() {
                        Ok(response) => {
                            let sample = Sample {
                                status: response.status().as_u16(),
                                latency: Duration::ZERO,
                                server_ms: header_ms(&response, HEADER_DURATION),
                                restore_ms: header_ms(&response, HEADER_RESTORE),
                                cold: response
                                    .headers()
                                    .get(HEADER_START)
                                    .is_some_and(|v| v == "cold"),
                            };
                            let _ = response.bytes();
                            Sample {
                                latency: begun.elapsed(),
                                ..sample
                            }
                        }
                        Err(_) => Sample {
                            status: 0,
                            latency: begun.elapsed(),
                            server_ms: None,
                            restore_ms: None,
                            cold: false,
                        },
                    };
                    samples
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(sample);
                }
            });
        }
    });
    let elapsed = started.elapsed();
    let samples = std::mem::take(&mut *samples.lock().unwrap_or_else(|e| e.into_inner()));
    (samples, elapsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(status: u16, ms: u64, cold: bool) -> Sample {
        Sample {
            status,
            latency: Duration::from_millis(ms),
            server_ms: Some(ms as f64 - 0.5),
            restore_ms: cold.then_some(3.0),
            cold,
        }
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let mut values: Vec<f64> = (1..=100).map(f64::from).collect();
        let p = percentiles(&mut values).unwrap();
        assert_eq!((p.p50, p.p90, p.p99, p.max), (50.0, 90.0, 99.0, 100.0));
        assert_eq!(p.mean, 50.5);
        assert_eq!(percentiles(&mut [7.0]).unwrap().p99, 7.0);
        assert!(percentiles(&mut []).is_none());
    }

    #[test]
    fn summaries_split_cold_warm_and_failures() {
        let samples = vec![
            sample(200, 40, true),
            sample(200, 2, false),
            sample(200, 4, false),
            sample(429, 1, false),
            sample(502, 9, false),
        ];
        let summary = summarize(&samples, 2, Duration::from_secs(1));
        assert_eq!(summary.requests, 5);
        assert_eq!((summary.cold, summary.warm), (1, 2));
        assert_eq!((summary.errors, summary.throttled), (1, 1));
        assert_eq!(summary.throughput, 3.0);
        assert_eq!(summary.cold_latency_ms.unwrap().p50, 40.0);
        assert_eq!(summary.warm_latency_ms.unwrap().max, 4.0);
        assert_eq!(summary.restore_ms.unwrap().p50, 3.0);
    }
}
