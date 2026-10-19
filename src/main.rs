//! Per-CPU timer wake-up latency, for bounded diagnostics or node_exporter.
#[cfg(target_os = "linux")]
mod wakeup;

#[cfg(target_os = "linux")]
mod linux {
    use crate::wakeup::{advance, clock, sleep_until};
    use serde_json::json;
    use std::{
        fmt::Write as _,
        fs, io, mem,
        path::PathBuf,
        sync::{
            Arc, Mutex, OnceLock,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    static STOP: AtomicBool = AtomicBool::new(false);
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

    #[derive(Clone)]
    struct Sample {
        bins: Vec<u64>,
        count: u64,
        sum_ns: u128,
        max_ns: u64,
        skipped: u64,
        start_ns: u64,
        observed_ns: u64,
        updated_unix: f64,
    }

    // Inclusive decimal boundaries: ..., 99, 100, 110, ... in nanoseconds.
    // The final boundary covers every representable delay; no tail is discarded.
    fn fine_bounds() -> &'static [u64] {
        static BOUNDS: OnceLock<Vec<u64>> = OnceLock::new();
        BOUNDS.get_or_init(|| {
            let mut bounds: Vec<u64> = (1..10).collect();
            let mut scale = 1_u64;
            loop {
                for digits in 10_u64..100 {
                    let Some(bound) = digits.checked_mul(scale) else {
                        bounds.push(u64::MAX);
                        return bounds;
                    };
                    bounds.push(bound);
                }
                scale *= 10;
            }
        })
    }

    impl Default for Sample {
        fn default() -> Self {
            Self {
                bins: vec![0; fine_bounds().len()],
                count: 0,
                sum_ns: 0,
                max_ns: 0,
                skipped: 0,
                start_ns: 0,
                observed_ns: 0,
                updated_unix: 0.0,
            }
        }
    }

    // Both boundary lists are ordered, and each output boundary is an input boundary.
    // Consume each fine bin once, producing exact cumulative Prometheus counts.
    fn prometheus_counts(bins: &[u64]) -> Vec<u64> {
        let mut input = fine_bounds().iter().zip(bins).peekable();
        let mut count = 0;
        PROMETHEUS_BOUNDS_NS
            .iter()
            .map(|bound| {
                while input.peek().is_some_and(|(upper, _)| **upper <= *bound) {
                    count += input.next().unwrap().1;
                }
                count
            })
            .collect()
    }

    fn percentiles(sample: &Sample) -> [String; 3] {
        let mut result = std::array::from_fn(|_| "unknown".to_owned());
        if sample.count == 0 {
            return result;
        }
        let targets = [90_u128, 95, 99].map(|p| (u128::from(sample.count) * p).div_ceil(100));
        let (mut count, mut next) = (0_u128, 0);
        for (&upper, &frequency) in fine_bounds().iter().zip(&sample.bins) {
            count += u128::from(frequency);
            while next < targets.len() && count >= targets[next] {
                let value = format!("{:.3}", upper as f64 / 1000.);
                result[next] = format!("≤{}", value.trim_end_matches('0').trim_end_matches('.'));
                next += 1;
            }
            if next == targets.len() {
                break;
            }
        }
        result
    }

    impl Sample {
        fn observe(&mut self, delay: u64) {
            self.bins[fine_bounds().partition_point(|&bound| bound < delay)] += 1;
            self.count += 1;
            self.sum_ns += u128::from(delay);
            self.max_ns = self.max_ns.max(delay);
        }
    }

    struct Options {
        duration: Option<u64>,
        interval_us: u64,
        textfile: Option<PathBuf>,
        json: bool,
    }

    fn options() -> Result<Option<Options>, Box<dyn std::error::Error>> {
        let mut result = Options {
            duration: Some(60),
            interval_us: 10_000,
            textfile: None,
            json: false,
        };
        let mut continuous = false;
        let mut duration_set = false;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--help" | "-h" => {
                    println!(
                        "wakeup-latency [--duration-seconds N | --continuous] [--interval-us N] [--json] [--textfile PATH]\n\nDefaults: 60 seconds, 10000 us interval, normal scheduling, all allowed CPUs.\nUse taskset to restrict CPUs. Continuous mode requires --textfile.\nMetrics update every five seconds; observations are published by workers every second.\nMeasures observed userspace timer wake-up latency, not host unavailable time."
                    );
                    return Ok(None);
                }
                "--continuous" => {
                    continuous = true;
                    result.duration = None;
                }
                "--json" => result.json = true,
                "--duration-seconds" => {
                    duration_set = true;
                    result.duration = Some(args.next().ok_or("missing duration")?.parse()?);
                }
                "--interval-us" => {
                    result.interval_us = args.next().ok_or("missing interval")?.parse()?
                }
                "--textfile" => result.textfile = Some(args.next().ok_or("missing path")?.into()),
                _ => return Err(format!("unknown option: {arg}").into()),
            }
        }
        if continuous && duration_set {
            return Err("choose duration or continuous mode".into());
        }
        if continuous && (result.textfile.is_none() || result.json) {
            return Err("continuous mode requires --textfile and does not use --json".into());
        }
        if !(1000..=1_000_000).contains(&result.interval_us)
            || result.duration.is_some_and(|d| !(1..=86400).contains(&d))
        {
            return Err(
                "interval must be 1000..1000000 us; duration must be 1..86400 seconds".into(),
            );
        }
        Ok(Some(result))
    }

    extern "C" fn stop(_: libc::c_int) {
        STOP.store(true, Ordering::Relaxed);
    }

    fn publish(state: &Mutex<Sample>, sample: &mut Sample) {
        sample.updated_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        *state.lock().unwrap() = sample.clone();
    }

    fn sample(
        cpu: usize,
        duration: Option<u64>,
        interval: u64,
        state: &Mutex<Sample>,
    ) -> io::Result<()> {
        let mut affinity: libc::cpu_set_t = unsafe { mem::zeroed() };
        unsafe {
            libc::CPU_SET(cpu, &mut affinity);
        }
        if unsafe { libc::sched_setaffinity(0, mem::size_of_val(&affinity), &affinity) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let start = clock()?;
        let end = duration.map(|seconds| start + seconds * 1_000_000_000);
        let mut result = Sample {
            start_ns: start,
            observed_ns: start,
            ..Sample::default()
        };
        let mut deadline = start + interval;
        let mut next_publish = start + 1_000_000_000;
        publish(state, &mut result);
        while !STOP.load(Ordering::Relaxed) {
            if end.is_some_and(|end| deadline > end) {
                break;
            }
            sleep_until(deadline)?;
            let now = clock()?;
            let delay = now
                .checked_sub(deadline)
                .ok_or_else(|| io::Error::other("early wake-up"))?;
            result.observe(delay);
            let (next, missed) = advance(deadline, now, interval);
            // Only count skipped deadlines that belong to this bounded run.
            result.skipped += end.map_or(missed, |end| missed.min((end - deadline) / interval));
            result.observed_ns = now;
            if now >= next_publish {
                publish(state, &mut result);
                next_publish = now + 1_000_000_000;
            }
            deadline = next;
        }
        publish(state, &mut result);
        Ok(())
    }

    fn exposition(samples: &[(usize, Sample)], interval_us: u64) -> String {
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
                .zip(prometheus_counts(&value.bins))
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
                value.count
            )
            .unwrap();
            writeln!(
                out,
                "wakeup_latency_seconds_count{{cpu=\"{cpu}\"}} {}",
                value.count
            )
            .unwrap();
            writeln!(
                out,
                "wakeup_latency_seconds_sum{{cpu=\"{cpu}\"}} {}",
                value.sum_ns as f64 / 1e9
            )
            .unwrap();
            for (name, number) in [
                ("max_seconds", value.max_ns as f64 / 1e9),
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

    fn write_metrics(
        path: &std::path::Path,
        samples: &[(usize, Sample)],
        interval: u64,
    ) -> io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        // The parent directory must belong to this process; only one writer per path.
        let temporary = path.with_extension("tmp");
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

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let Some(options) = options()? else {
            return Ok(());
        };
        if unsafe { libc::sched_getscheduler(0) } != libc::SCHED_OTHER {
            return Err("requires normal scheduling (SCHED_OTHER)".into());
        }
        let mut allowed: libc::cpu_set_t = unsafe { mem::zeroed() };
        if unsafe { libc::sched_getaffinity(0, mem::size_of_val(&allowed), &mut allowed) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        let cpus: Vec<_> = (0..libc::CPU_SETSIZE as usize)
            .filter(|&cpu| unsafe { libc::CPU_ISSET(cpu, &allowed) })
            .collect();
        if cpus.is_empty() {
            return Err("no allowed CPUs".into());
        }
        unsafe {
            libc::signal(libc::SIGINT, stop as *const () as libc::sighandler_t);
            libc::signal(libc::SIGTERM, stop as *const () as libc::sighandler_t);
        }
        let states: Vec<_> = cpus
            .iter()
            .map(|_| Arc::new(Mutex::new(Sample::default())))
            .collect();
        let workers: Vec<_> = cpus
            .iter()
            .zip(&states)
            .map(|(&cpu, state)| {
                let state = state.clone();
                let (duration, interval) = (options.duration, options.interval_us * 1000);
                thread::spawn(move || {
                    let outcome = sample(cpu, duration, interval, &state);
                    if outcome.is_err() {
                        STOP.store(true, Ordering::Relaxed);
                    }
                    outcome
                })
            })
            .collect();
        let snapshot = || {
            cpus.iter()
                .zip(&states)
                .map(|(&cpu, state)| (cpu, state.lock().unwrap().clone()))
                .collect::<Vec<_>>()
        };
        let mut error = None;
        let mut ticks = 0;
        while !workers.iter().all(|worker| worker.is_finished()) {
            if options.duration.is_none() && workers.iter().any(|worker| worker.is_finished()) {
                STOP.store(true, Ordering::Relaxed);
            }
            thread::sleep(Duration::from_millis(100));
            ticks += 1;
            if ticks % 50 == 0 {
                if let Some(path) = &options.textfile {
                    if let Err(e) = write_metrics(path, &snapshot(), options.interval_us) {
                        error = Some(e.to_string());
                        STOP.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
        for worker in workers {
            match worker.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => error = Some(e.to_string()),
                Err(_) => error = Some("sampling thread panicked".into()),
            }
        }
        if let Some(error) = error {
            return Err(error.into());
        }
        let samples = snapshot();
        if let Some(path) = &options.textfile {
            write_metrics(path, &samples, options.interval_us)?;
        }
        if options.duration.is_some() {
            if options.json {
                let values: Vec<_> = samples.iter().map(|(cpu, s)| json!({"cpu": cpu, "observations": s.count, "skipped_deadlines": s.skipped,
                    "sum_ns": s.sum_ns, "max_ns": s.max_ns, "bins": s.bins.to_vec(), "start_monotonic_ns": s.start_ns, "last_observation_monotonic_ns": s.observed_ns})).collect();
                println!(
                    "{}",
                    json!({"clock": "CLOCK_MONOTONIC", "policy": "SCHED_OTHER", "interval_us": options.interval_us, "bucket_upper_bounds_ns": fine_bounds(), "cpus": values})
                );
            } else {
                println!(
                    "Userspace timer wake-up latency; SCHED_OTHER; interval {} us",
                    options.interval_us
                );
                println!(
                    " CPU   Observations   Skipped    Mean (us)     Max (ms)       p90 (us)       p95 (us)       p99 (us)"
                );
                for (cpu, s) in samples {
                    let [p90, p95, p99] = percentiles(&s);
                    println!(
                        "{cpu:4} {:14} {:9} {:12.2} {:12.3} {p90:>14} {p95:>14} {p99:>14}",
                        s.count,
                        s.skipped,
                        s.sum_ns as f64 / s.count.max(1) as f64 / 1000.,
                        s.max_ns as f64 / 1e6
                    );
                }
                println!(
                    "Percentiles show histogram upper bounds. Skipped deadlines are not observations."
                );
            }
        }
        Ok(())
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    if let Err(error) = linux::run() {
        eprintln!("wakeup-latency: {error}");
        std::process::exit(1);
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("wakeup-latency requires Linux");
        std::process::exit(1);
    }
}
