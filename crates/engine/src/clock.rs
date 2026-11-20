//! One clock for everything: QueryPerformanceCounter, expressed in 100 ns units ("hns"),
//! which is also Media Foundation's native time unit.

use std::sync::OnceLock;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

pub const HNS_PER_SEC: i64 = 10_000_000;

pub fn qpc() -> i64 {
    let mut v = 0i64;
    // Cannot fail on Windows XP and later.
    unsafe {
        let _ = QueryPerformanceCounter(&mut v);
    }
    v
}

pub fn qpc_freq() -> i64 {
    static FREQ: OnceLock<i64> = OnceLock::new();
    *FREQ.get_or_init(|| {
        let mut f = 0i64;
        unsafe {
            let _ = QueryPerformanceFrequency(&mut f);
        }
        f.max(1)
    })
}

/// QPC ticks → 100 ns units without intermediate overflow.
pub fn qpc_to_hns(ticks: i64) -> i64 {
    let f = qpc_freq();
    (ticks / f) * HNS_PER_SEC + (ticks % f) * HNS_PER_SEC / f
}

/// Current time in hns on the QPC timeline.
pub fn now_hns() -> i64 {
    qpc_to_hns(qpc())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversion_is_monotonic_and_scaled() {
        let f = qpc_freq();
        assert_eq!(qpc_to_hns(f), HNS_PER_SEC);
        assert_eq!(qpc_to_hns(f * 3 + f / 2), 3 * HNS_PER_SEC + HNS_PER_SEC / 2);
        let a = now_hns();
        let b = now_hns();
        assert!(b >= a);
    }
}
