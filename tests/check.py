#!/usr/bin/env python3
"""Linux executable-contract checks; pass a built wakeup-latency binary."""
import json
import pathlib
import subprocess
import sys
import tempfile
import time

binary = str(pathlib.Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory() as directory:
    metrics = pathlib.Path(directory) / 'metrics.prom'
    completed = subprocess.run(
        [binary, '--duration-seconds', '2', '--interval-us', '10000', '--json', '--textfile', str(metrics)],
        check=True, capture_output=True, text=True, timeout=15,
    )
    report = json.loads(completed.stdout)
    assert report['cpus']
    assert report['policy'] == 'SCHED_OTHER'
    lines = metrics.read_text().splitlines()
    values = dict(line.rsplit(' ', 1) for line in lines if not line.startswith('#'))
    for cpu in report['cpus']:
        assert sum(cpu['bins']) == cpu['observations']
        assert cpu['observations'] + cpu['skipped_deadlines'] == 200
        assert cpu['sum_ns'] >= cpu['max_ns']
        suffix = '{cpu="' + str(cpu['cpu']) + '"}'
        assert int(values['wakeup_latency_seconds_count' + suffix]) == cpu['observations']
        assert int(values['wakeup_latency_skipped_deadlines_total' + suffix]) == cpu['skipped_deadlines']
        prefix = 'wakeup_latency_seconds_bucket{cpu="' + str(cpu['cpu']) + '",le="'
        for key, count in values.items():
            if not key.startswith(prefix) or '+Inf' in key:
                continue
            upper_ns = round(float(key[len(prefix):-2]) * 1e9)
            expected = sum(n for bound, n in zip(report['bucket_upper_bounds_ns'], cpu['bins']) if bound <= upper_ns)
            assert int(count) == expected
    subprocess.run(['promtool', 'check', 'metrics'], stdin=metrics.open(), check=True)
    metrics.unlink()
    process = subprocess.Popen([binary, '--continuous', '--textfile', str(metrics)])
    try:
        deadline = time.monotonic() + 15
        while not metrics.exists():
            if process.poll() is not None or time.monotonic() > deadline:
                raise AssertionError('continuous collector failed to publish')
            time.sleep(0.1)
        subprocess.run(['promtool', 'check', 'metrics'], stdin=metrics.open(), check=True)
        process.terminate()
        assert process.wait(timeout=5) == 0
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
    assert subprocess.run([binary, '--continuous', '--duration-seconds', '2'], capture_output=True).returncode != 0
    failed_output = subprocess.run(
        [binary, '--continuous', '--textfile', str(pathlib.Path(directory) / 'missing' / 'metrics.prom')],
        capture_output=True, timeout=15,
    )
    assert failed_output.returncode != 0
print('PASS: bounded histogram/counts, Prometheus export, continuous publication, SIGTERM, output failure, option validation')
