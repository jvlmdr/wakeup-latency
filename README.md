# wakeup-latency

A small Linux tool for measuring per-CPU timer wake-up latency: how late a thread resumes after its scheduled wake-up time.
Run a quick diagnostic or collect continuous Prometheus metrics.
The probe uses normal scheduling and needs no root privileges.

## Build and run a quick check

Build on Linux with Rust supporting edition 2024:

```sh
cargo build --release --locked
./target/release/wakeup-latency --duration-seconds 60
```

The report shows observations, skipped deadlines, mean, maximum, and p90/p95/p99 per CPU.
Percentiles are histogram upper bounds: `≤240` in a microsecond column means the percentile is at most 240 µs.
Ctrl-C ends the diagnostic and prints partial results.

```sh
# Save a machine-readable histogram and summary.
./target/release/wakeup-latency --duration-seconds 60 --json > latency.json

# Sample every 100 ms instead of the default 10 ms.
./target/release/wakeup-latency --duration-seconds 60 --interval-us 100000

# Restrict measurement to two CPUs.
taskset -c 0,1 ./target/release/wakeup-latency --duration-seconds 60
```

For another machine, build there or copy the binary to a temporary directory on a compatible Linux system, then run it directly.
No installation or Prometheus is needed for a quick check.
Stop any installed probe before making a controlled comparison; otherwise the diagnostic adds another set of sampling threads.

## Install continuous monitoring

Prerequisites: systemd and node_exporter configured with a textfile collector directory, for example `--collector.textfile.directory=/var/lib/prometheus/node-exporter`.
Prometheus must scrape that node_exporter.
The installer does not install or configure either of those tools.

After building:

```sh
bash install /var/lib/prometheus/node-exporter
```

Pass your actual textfile directory.
Run the installer as your normal user; it invokes sudo only for privileged steps.
It installs the executable in `/usr/local/bin`, enables the service at boot, and starts it immediately.
The service uses an unprivileged identity and samples every 10 ms on each allowed CPU.
It does not stop or disable unrelated collectors.

```sh
# Check operation and read the exported metrics.
systemctl status wakeup-latency
journalctl -u wakeup-latency -n 50 --no-pager
cat /run/wakeup-latency/metrics.prom

# Pause and resume collection.
sudo systemctl stop wakeup-latency
sudo systemctl start wakeup-latency

# After installation, run a foreground diagnostic by name.
wakeup-latency --duration-seconds 60
```

To update, rebuild and run the installer again with the same textfile directory.
Installation restarts the service and resets its counters; Prometheus's `rate()` handles counter resets.
For persistent service changes, use `sudo systemctl edit wakeup-latency` and then restart the service.
For example, to change its sampling interval to 100 ms:

```ini
[Service]
ExecStart=
ExecStart=/usr/local/bin/wakeup-latency --continuous --interval-us 100000 --textfile /run/wakeup-latency/metrics.prom
```

## Useful Prometheus queries

Paste these into your Prometheus query interface.
The examples use a five-minute rate window; it should contain several scrapes.
They assume one collection of these metrics per instance.

### Per-CPU p99 latency, milliseconds

```promql
1000 * histogram_quantile(
  0.99,
  rate(wakeup_latency_seconds_bucket[5m])
)
```

Replace `0.99` with `0.95` or `0.90` for p95 or p90.

### P99 pooled across CPUs, milliseconds

```promql
1000 * histogram_quantile(
  0.99,
  sum by (instance, le) (
    rate(wakeup_latency_seconds_bucket[5m])
  )
)
```

This pools observations, rather than averaging per-CPU percentiles.
Keeping `instance` prevents different machines from being mixed together.
Keep the per-CPU view available: pooling can hide a problem affecting one CPU.

### Percentage of observed wake-ups more than 10 ms late, per CPU

```promql
100 * (
  1 -
  rate(wakeup_latency_seconds_bucket{le="0.01"}[5m])
  /
  rate(wakeup_latency_seconds_count[5m])
)
```

### Mean observed latency, milliseconds, per CPU

```promql
1000 * rate(wakeup_latency_seconds_sum[5m])
     / rate(wakeup_latency_seconds_count[5m])
```

### Skipped deadlines per second, per CPU

```promql
rate(wakeup_latency_skipped_deadlines_total[5m])
```

### Worker freshness, seconds since publication

```promql
time() - wakeup_latency_worker_update_timestamp_seconds
```

This includes worker publication, file publication, and scrape delays.
An absent series means missing data, not a healthy worker; inspect the service and node_exporter if it disappears.

### Maximum since the probe started, milliseconds, per CPU

```promql
1000 * wakeup_latency_max_seconds
```

This is a lifetime maximum, not the maximum within the selected graph window.

## How it works

One thread per allowed CPU is pinned using `sched_setaffinity()`.
Each thread sleeps until an absolute `CLOCK_MONOTONIC` deadline using `clock_nanosleep(..., TIMER_ABSTIME, ...)`, then reads `clock_gettime()` to measure how late it resumed.
It records the observed delay in a histogram and counts strictly expired intervening deadlines separately.
Threads use normal Linux scheduling (`SCHED_OTHER`), without busy-waiting or real-time privileges.

Workers publish snapshots approximately once a second; the main thread writes an atomic Prometheus textfile every five seconds.
Node_exporter exposes that file, and Prometheus handles history.

## Interpreting resolution and delays

- **Sampling interval:** defaults to 10 ms per CPU; delays entirely between scheduled wake-ups can go undetected.
- **Publication interval:** metrics update every five seconds, using the latest worker snapshots.
- **Scrape interval:** determines how frequently Prometheus stores those counters.
- **Query window:** `[5m]` combines observations over the preceding five minutes.
- **Graph step:** controls how often the query is evaluated; use a step no longer than the query window to avoid leaving periods unrepresented.

Diagnostic percentiles use fine decimal bins; Prometheus exports coarser buckets and interpolates percentile estimates within them.
Skipped deadlines are not synthesized as histogram observations, so these percentiles describe observed wake-ups rather than all scheduled deadlines.
The measurements include guest scheduling and other sources of delay; they do not identify the cause or measure host availability.
Use timerlat or osnoise for targeted diagnosis when necessary.

See the [reference](REFERENCE.md) for histogram details, validation, and removal instructions.

## License

[MIT](LICENSE).
