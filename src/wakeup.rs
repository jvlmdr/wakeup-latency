//! Linux absolute monotonic sleeps and periodic deadline advancement.
use std::io;
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

// Preserve cyclictest's strict comparison: a deadline equal to now is not skipped.
pub fn advance(deadline: u64, now: u64, interval: u64) -> (u64, u64) {
    let next = deadline + interval;
    let skipped = now.saturating_sub(next).div_ceil(interval);
    (next + skipped * interval, skipped)
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
