//! Per-CPU timer wake-up latency, for bounded diagnostics or node_exporter.
#[cfg(any(target_os = "linux", test))]
mod histogram;
#[cfg(target_os = "linux")]
mod report;
#[cfg(target_os = "linux")]
mod wakeup;

#[cfg(target_os = "linux")]
mod linux {
    use crate::{
        report,
        wakeup::{self, Measurement, STOP},
    };
    use std::{
        io,
        path::PathBuf,
        sync::{Mutex, atomic::Ordering, mpsc},
        thread,
        time::{Duration, Instant},
    };

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

    fn snapshot(samples: &mut [(usize, Measurement)], states: &[Mutex<Measurement>]) {
        for ((_, sample), state) in samples.iter_mut().zip(states) {
            sample.copy_from(&state.lock().unwrap());
        }
    }

    fn collect(options: &Options, cpus: &[usize]) -> io::Result<Vec<(usize, Measurement)>> {
        let states: Vec<_> = cpus
            .iter()
            .map(|_| Mutex::new(Measurement::default()))
            .collect();
        let mut samples: Vec<_> = cpus
            .iter()
            .map(|&cpu| (cpu, Measurement::default()))
            .collect();
        thread::scope(|scope| {
            let (completed, results) = mpsc::channel();
            for (&cpu, state) in cpus.iter().zip(&states) {
                let completed = completed.clone();
                thread::Builder::new()
                    .spawn_scoped(scope, move || {
                        let outcome = std::panic::catch_unwind(|| {
                            wakeup::measure(
                                cpu,
                                options.duration,
                                options.interval_us * 1000,
                                state,
                            )
                        })
                        .unwrap_or_else(|_| Err(io::Error::other("sampling thread panicked")));
                        let _ = completed.send((cpu, outcome));
                    })
                    .map_err(|cause| {
                        STOP.store(true, Ordering::Relaxed);
                        io::Error::other(format!("CPU {cpu}: {cause}"))
                    })?;
            }
            drop(completed);
            let mut remaining = cpus.len();
            let mut error = None;
            let publication_interval = Duration::from_secs(5);
            let mut next_publish = Instant::now() + publication_interval;
            while remaining != 0 {
                match results.recv_timeout(next_publish.saturating_duration_since(Instant::now())) {
                    Ok((cpu, outcome)) => {
                        remaining -= 1;
                        if let Err(cause) = outcome {
                            error.get_or_insert_with(|| {
                                io::Error::other(format!("CPU {cpu}: {cause}"))
                            });
                            STOP.store(true, Ordering::Relaxed);
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if let Some(path) = &options.textfile {
                            snapshot(&mut samples, &states);
                            if let Err(cause) =
                                report::write_metrics(path, &samples, options.interval_us)
                            {
                                error.get_or_insert(cause);
                                STOP.store(true, Ordering::Relaxed);
                            }
                        }
                        next_publish = Instant::now() + publication_interval;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        unreachable!("workers report before exiting")
                    }
                }
            }
            if let Some(error) = error {
                return Err(error);
            }
            snapshot(&mut samples, &states);
            Ok(samples)
        })
    }

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let Some(options) = options()? else {
            return Ok(());
        };
        if unsafe { libc::sched_getscheduler(0) } != libc::SCHED_OTHER {
            return Err("requires normal scheduling (SCHED_OTHER)".into());
        }
        let cpus = wakeup::allowed_cpus()?;
        if cpus.is_empty() {
            return Err("no allowed CPUs".into());
        }
        unsafe {
            libc::signal(libc::SIGINT, stop as *const () as libc::sighandler_t);
            libc::signal(libc::SIGTERM, stop as *const () as libc::sighandler_t);
        }
        let samples = collect(&options, &cpus)?;
        if let Some(path) = &options.textfile {
            report::write_metrics(path, &samples, options.interval_us)?;
        }
        if options.duration.is_some() {
            let output = if options.json {
                report::diagnostic_json(&samples, options.interval_us)
            } else {
                report::diagnostic_text(&samples, options.interval_us)
            };
            println!("{output}");
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
