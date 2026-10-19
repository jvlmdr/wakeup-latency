# Reference

## Measurement contract

Each observation is actual resume time minus the scheduled deadline, measured in nanoseconds.
The internal histogram records observed wake-ups with two-significant-digit decimal upper boundaries in nanoseconds: 1, 2, ..., 99, 100, 110, ..., 990, 1000, 1100, and so on.
Bins are lower-exclusive and upper-inclusive, except the first includes zero.
The final boundary is the maximum representable nanosecond delay.
At output time, an ordered scan merges fine bins into the existing 22 coarse Prometheus buckets and the +Inf bucket.
Every exported finite boundary is a fine boundary, so rebinning is exact and linear in the number of bins.
Diagnostic percentiles retain the fine resolution; historical Prometheus percentiles retain the coarser exported resolution.
Sum and maximum retain actual values, including overflow observations.
A delayed wake-up advances to the next deadline at or after the observed time, counting strictly expired intervening deadlines separately.
Bounded runs count only skipped deadlines within their requested interval; a final wake-up may occur after the requested end.
The histogram is not a reconstruction of the skipped deadlines and is not a uniformly sampled-time availability distribution.

Observed span is elapsed time from the worker's start to its last observation, not continuous coverage.
Sampling can miss delays entirely between scheduled deadlines.
Worker update timestamps expose stalled or stale workers; counters reset when the process restarts.
Normal-priority wake-up latency includes guest scheduling and other sources of delay, and does not identify the cause.
Use timerlat or osnoise for targeted diagnosis.

## Validation

On Linux, with Python 3 and Prometheus's `promtool` available:

```sh
cargo fmt --check
cargo test --locked
cargo build --release --locked
python3 tests/check.py target/release/wakeup-latency
```

Python and promtool are test tools, not runtime dependencies.
The executable uses libc and serde_json; it has no async runtime or HTTP server.
The live checks exercise real sampling, JSON and Prometheus count agreement, continuous publication, and graceful shutdown.

## Removing the service

```sh
sudo systemctl disable --now wakeup-latency.service
sudo rm /etc/systemd/system/wakeup-latency.service
sudo systemctl daemon-reload
```

Remove `wakeup-latency.prom` from your node_exporter textfile directory and optionally remove `/usr/local/bin/wakeup-latency` and `/usr/local/share/doc/wakeup-latency`.
Prometheus retains previously collected data according to its retention policy.
