//! Per-CPU measurement using absolute monotonic sleeps under normal scheduling.
use crate::histogram::Histogram;
use std::{
    io, mem,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

pub static STOP: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
pub struct Measurement {
    pub histogram: Histogram,
    pub skipped: u64,
    pub start_ns: u64,
    pub observed_ns: u64,
    pub updated_unix: f64,
}

impl Measurement {
    pub fn copy_from(&mut self, source: &Self) {
        self.histogram.copy_from(&source.histogram);
        self.skipped = source.skipped;
        self.start_ns = source.start_ns;
        self.observed_ns = source.observed_ns;
        self.updated_unix = source.updated_unix;
    }
}

pub fn allowed_cpus() -> io::Result<Vec<usize>> {
    let mut allowed: libc::cpu_set_t = unsafe { mem::zeroed() };
    if unsafe { libc::sched_getaffinity(0, mem::size_of_val(&allowed), &mut allowed) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((0..libc::CPU_SETSIZE as usize)
        .filter(|&cpu| unsafe { libc::CPU_ISSET(cpu, &allowed) })
        .collect())
}

fn pin_to_cpu(cpu: usize) -> io::Result<()> {
    let mut affinity: libc::cpu_set_t = unsafe { mem::zeroed() };
    unsafe {
        libc::CPU_SET(cpu, &mut affinity);
    }
    if unsafe { libc::sched_setaffinity(0, mem::size_of_val(&affinity), &affinity) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn clock() -> io::Result<u64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64)
}

pub fn sleep_until(deadline: u64) -> io::Result<()> {
    let ts = libc::timespec {
        tv_sec: (deadline / 1_000_000_000) as _,
        tv_nsec: (deadline % 1_000_000_000) as _,
    };
    loop {
        let result = unsafe {
            libc::clock_nanosleep(
                libc::CLOCK_MONOTONIC,
                libc::TIMER_ABSTIME,
                &ts,
                std::ptr::null_mut(),
            )
        };
        match result {
            0 => return Ok(()),
            libc::EINTR => continue,
            code => return Err(io::Error::from_raw_os_error(code)),
        }
    }
}

/// Return the first subsequent scheduled deadline at or after `now`, and the number skipped.
pub fn advance(deadline: u64, now: u64, interval: u64) -> (u64, u64) {
    let next = deadline + interval;
    let skipped = now.saturating_sub(next).div_ceil(interval);
    (next + skipped * interval, skipped)
}

fn publish(state: &Mutex<Measurement>, sample: &mut Measurement) {
    sample.updated_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    state.lock().unwrap().copy_from(sample);
}

pub fn measure(
    cpu: usize,
    duration: Option<u64>,
    interval: u64,
    state: &Mutex<Measurement>,
) -> io::Result<()> {
    pin_to_cpu(cpu)?;
    let start = clock()?;
    let end = duration.map(|seconds| start + seconds * 1_000_000_000);
    let mut result = Measurement {
        start_ns: start,
        observed_ns: start,
        ..Measurement::default()
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
        result.histogram.observe(delay);
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

#[cfg(test)]
mod tests {
    use super::advance;

    #[test]
    fn advances_past_expired_deadlines_without_skipping_equal_deadlines() {
        assert_eq!(advance(100, 100, 100), (200, 0));
        assert_eq!(advance(100, 200, 100), (200, 0));
        assert_eq!(advance(100, 201, 100), (300, 1));
        assert_eq!(advance(100, 450, 100), (500, 3));
        assert_eq!(advance(100, 1_000_000_000, 100), (1_000_000_000, 9_999_998));
    }
}
