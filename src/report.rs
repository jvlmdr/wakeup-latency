//! Text, JSON, and Prometheus representations of collected measurements.
use crate::{histogram, wakeup::Measurement};
use serde_json::json;
use std::{fmt::Write as _, fs, io, path::Path};

const PROMETHEUS_BOUNDS_NS: [u64; 22] = [
    10_000,
    20_000,
    50_000,
    100_000,
    200_000,
    500_000,
    1_000_000,
    2_000_000,
    5_000_000,
    10_000_000,
    20_000_000,
    50_000_000,
    100_000_000,
    200_000_000,
    500_000_000,
    1_000_000_000,
    2_000_000_000,
    5_000_000_000,
    10_000_000_000,
    20_000_000_000,
    50_000_000_000,
    100_000_000_000,
];

fn exposition(samples: &[(usize, Measurement)], interval_us: u64) -> String {
    let mut out = String::new();
    for (name, kind, help) in [
        (
            "wakeup_latency_seconds",
            "histogram",
            "Observed userspace timer wake-up lateness under SCHED_OTHER; skipped deadlines are not observations.",
        ),
        (
            "wakeup_latency_max_seconds",
            "gauge",
            "Largest observed lateness since process start.",
        ),
        (
            "wakeup_latency_skipped_deadlines_total",
            "counter",
            "Scheduled deadlines skipped after late wake-ups.",
        ),
        (
            "wakeup_latency_observed_span_seconds_total",
            "counter",
            "Monotonic elapsed time from worker start to its last observation; not continuous observation coverage.",
        ),
        (
            "wakeup_latency_worker_update_timestamp_seconds",
            "gauge",
            "Unix timestamp at the last worker publication, normally within one second of an observation.",
        ),
        (
            "wakeup_latency_interval_seconds",
            "gauge",
            "Configured interval between scheduled deadlines.",
        ),
    ] {
        writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}").unwrap();
    }
    writeln!(
        out,
        "wakeup_latency_interval_seconds {}",
        interval_us as f64 / 1e6
    )
    .unwrap();
    for (cpu, value) in samples {
        for (bound, count) in PROMETHEUS_BOUNDS_NS
            .iter()
            .zip(value.histogram.cumulative_counts(&PROMETHEUS_BOUNDS_NS))
        {
            writeln!(
                out,
                "wakeup_latency_seconds_bucket{{cpu=\"{cpu}\",le=\"{}\"}} {count}",
                *bound as f64 / 1e9
            )
            .unwrap();
        }
        writeln!(
            out,
            "wakeup_latency_seconds_bucket{{cpu=\"{cpu}\",le=\"+Inf\"}} {}",
            value.histogram.count()
        )
        .unwrap();
        writeln!(
            out,
            "wakeup_latency_seconds_count{{cpu=\"{cpu}\"}} {}",
            value.histogram.count()
        )
        .unwrap();
        writeln!(
            out,
            "wakeup_latency_seconds_sum{{cpu=\"{cpu}\"}} {}",
            value.histogram.sum_ns() as f64 / 1e9
        )
        .unwrap();
        for (name, number) in [
            ("max_seconds", value.histogram.max_ns() as f64 / 1e9),
            (
                "observed_span_seconds_total",
                value.observed_ns.saturating_sub(value.start_ns) as f64 / 1e9,
            ),
            ("worker_update_timestamp_seconds", value.updated_unix),
        ] {
            writeln!(out, "wakeup_latency_{name}{{cpu=\"{cpu}\"}} {number}").unwrap();
        }
        writeln!(
            out,
            "wakeup_latency_skipped_deadlines_total{{cpu=\"{cpu}\"}} {}",
            value.skipped
        )
        .unwrap();
    }
    out
}

pub fn write_metrics(
    path: &Path,
    samples: &[(usize, Measurement)],
    interval: u64,
) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    // The parent directory must belong to this process; only one writer per path.
    let mut temporary = path.as_os_str().to_os_string();
    temporary.push(".tmp");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(&temporary)?;
    file.write_all(exposition(samples, interval).as_bytes())?;
    drop(file);
    fs::rename(temporary, path)
}

fn percentile_labels(measurement: &Measurement) -> [String; 3] {
    measurement.histogram.percentiles().map_or_else(
        || std::array::from_fn(|_| "unknown".to_owned()),
        |bounds| {
            bounds.map(|upper| {
                let value = format!("{:.3}", upper as f64 / 1000.);
                format!("≤{}", value.trim_end_matches('0').trim_end_matches('.'))
            })
        },
    )
}

pub fn diagnostic_json(samples: &[(usize, Measurement)], interval_us: u64) -> String {
    let values: Vec<_> = samples
        .iter()
        .map(|(cpu, s)| {
            json!({
                "cpu": cpu,
                "observations": s.histogram.count(),
                "skipped_deadlines": s.skipped,
                "sum_ns": s.histogram.sum_ns(),
                "max_ns": s.histogram.max_ns(),
                "bins": s.histogram.bins(),
                "start_monotonic_ns": s.start_ns,
                "last_observation_monotonic_ns": s.observed_ns,
            })
        })
        .collect();
    json!({
        "clock": "CLOCK_MONOTONIC", "policy": "SCHED_OTHER",
        "interval_us": interval_us, "bucket_upper_bounds_ns": histogram::bounds(), "cpus": values,
    })
    .to_string()
}

pub fn diagnostic_text(samples: &[(usize, Measurement)], interval_us: u64) -> String {
    let mut out =
        format!("Userspace timer wake-up latency; SCHED_OTHER; interval {interval_us} us\n");
    writeln!(out, " CPU   Observations   Skipped    Mean (us)     Max (ms)       p90 (us)       p95 (us)       p99 (us)").unwrap();
    for (cpu, s) in samples {
        let [p90, p95, p99] = percentile_labels(s);
        writeln!(
            out,
            "{cpu:4} {:14} {:9} {:12.2} {:12.3} {p90:>14} {p95:>14} {p99:>14}",
            s.histogram.count(),
            s.skipped,
            s.histogram.sum_ns() as f64 / s.histogram.count().max(1) as f64 / 1000.,
            s.histogram.max_ns() as f64 / 1e6
        )
        .unwrap();
    }
    out.push_str(
        "Percentiles show histogram upper bounds. Skipped deadlines are not observations.",
    );
    out
}
