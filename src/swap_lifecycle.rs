//! Bounded, conservative scheduling for owned swap-file retirement.
//!
//! The controller supplies its existing sustained-health permission. The
//! worker rechecks pressure and both memory/capacity reserves immediately before
//! touching a file. A pathname absent from /proc/swaps is never deemed inactive.

use zmem::Snapshot;

pub const INTERVAL_SECS: u64 = 60;
pub const SCAN_LIMIT: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Cleanup,
    Shrink,
}

impl Mode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "cleanup" => Some(Self::Cleanup),
            "shrink" => Some(Self::Shrink),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cleanup => "cleanup",
            Self::Shrink => "shrink",
        }
    }
}

#[derive(Default)]
pub struct Schedule {
    last_attempt: Option<u64>,
    cleanup_round: u64,
    shrink_round: u64,
}

impl Schedule {
    pub fn next(&mut self, mode: Option<Mode>, now_secs: u64) -> Option<(Mode, u64)> {
        let mode = mode?;
        if self
            .last_attempt
            .is_some_and(|last| now_secs.saturating_sub(last) < INTERVAL_SECS)
        {
            return None;
        }
        self.last_attempt = Some(now_secs);
        let counter = match mode {
            Mode::Cleanup => &mut self.cleanup_round,
            Mode::Shrink => &mut self.shrink_round,
        };
        let round = *counter;
        *counter = counter.wrapping_add(1);
        Some((mode, round))
    }
}

pub fn candidate_index(round: u64, count: usize) -> Option<usize> {
    (count != 0).then(|| (round % count as u64) as usize)
}

pub fn cleanup_defer_reason(
    sample: Snapshot,
    reserve: u64,
    critical_full: u16,
) -> Option<&'static str> {
    if sample.total_bytes == 0 || sample.available_bytes > sample.total_bytes {
        return Some("invalid_memory_sample");
    }
    if sample.available_bytes < reserve / 2 || sample.memory_full_bp >= critical_full {
        return Some("critical_pressure");
    }
    if sample.io_full_bp > 100 || zmem::io_congested(sample) {
        return Some("io_pressure");
    }
    None
}

pub fn defer_reason(
    sample: Snapshot,
    reserve: u64,
    keep_swap_free: u64,
    file_bytes: u64,
) -> Option<&'static str> {
    if sample.memory_some_bp > 200 || sample.memory_full_bp > 100 || sample.io_full_bp > 100 {
        return Some("pressure");
    }
    // Even a currently empty file can receive pages before swapoff starts.
    // Its full size bounds the migration cost, including orphan mount aliases
    // whose per-file usage cannot be established from /proc/swaps path strings.
    let needed = reserve
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(file_bytes));
    if needed.is_none_or(|needed| sample.available_bytes < needed) {
        return Some("memory_headroom");
    }
    // Assume the entire file is active and free. This can under-estimate the
    // capacity left by an inactive/partly-used file, never over-estimate it.
    if sample
        .swap_free_bytes
        .checked_sub(file_bytes)
        .is_none_or(|free| free < keep_swap_free)
    {
        return Some("swap_headroom");
    }
    None
}

pub fn cleanup_may_be_attempted(errno: Option<i32>) -> bool {
    // EINVAL can also mean another swapoff is still draining. It permits only
    // an ordinary, kernel-guarded unlink attempt after identity and inventory
    // revalidation; it does NOT prove inactivity. Unknown results never qualify.
    matches!(errno, Some(0 | 22))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zmem::GIB;

    fn healthy() -> Snapshot {
        Snapshot {
            total_bytes: 32 * GIB,
            available_bytes: 12 * GIB,
            swap_total_bytes: 16 * GIB,
            swap_free_bytes: 12 * GIB,
            ..Snapshot::default()
        }
    }

    #[test]
    fn maintenance_needs_permission_and_cools_down_failed_attempts_too() {
        let mut schedule = Schedule::default();
        assert_eq!(schedule.next(None, 600), None);
        assert_eq!(
            schedule.next(Some(Mode::Shrink), 600),
            Some((Mode::Shrink, 0))
        );
        assert_eq!(schedule.next(Some(Mode::Cleanup), 659), None);
        assert_eq!(schedule.next(None, 660), None);
        assert_eq!(
            schedule.next(Some(Mode::Cleanup), 700),
            Some((Mode::Cleanup, 0))
        );
        assert_eq!(schedule.next(Some(Mode::Shrink), 699), None);
        assert_eq!(
            schedule.next(Some(Mode::Shrink), 760),
            Some((Mode::Shrink, 1))
        );
    }

    #[test]
    fn round_robin_does_not_starve_later_files_after_a_deferred_attempt() {
        assert_eq!(candidate_index(0, 0), None);
        let attempts: Vec<_> = (0..6).map(|round| candidate_index(round, 3)).collect();
        assert_eq!(
            attempts,
            vec![Some(0), Some(1), Some(2), Some(0), Some(1), Some(2)]
        );
        assert_eq!(candidate_index(u64::MAX, 1), Some(0));
    }

    #[test]
    fn whole_file_cost_is_reserved_even_when_an_alias_hides_usage() {
        let sample = healthy();
        assert_eq!(defer_reason(sample, 3 * GIB, 4 * GIB, 6 * GIB), None);
        assert_eq!(
            defer_reason(sample, 3 * GIB, 4 * GIB, 6 * GIB + 1),
            Some("memory_headroom")
        );
        let sample = Snapshot {
            available_bytes: 20 * GIB,
            ..sample
        };
        assert_eq!(
            defer_reason(sample, 3 * GIB, 4 * GIB, 8 * GIB + 1),
            Some("swap_headroom")
        );
    }

    #[test]
    fn current_memory_or_io_pressure_cancels_an_earlier_permission() {
        for sample in [
            Snapshot {
                memory_some_bp: 201,
                ..healthy()
            },
            Snapshot {
                memory_full_bp: 101,
                ..healthy()
            },
            Snapshot {
                io_full_bp: 101,
                ..healthy()
            },
        ] {
            assert_eq!(
                defer_reason(sample, GIB, 4 * GIB, 2 * GIB),
                Some("pressure")
            );
        }
    }

    #[test]
    fn arithmetic_overflow_or_missing_capacity_defers_retirement() {
        assert_eq!(
            defer_reason(healthy(), u64::MAX, 0, 0),
            Some("memory_headroom")
        );
        let sample = Snapshot {
            swap_free_bytes: 0,
            ..healthy()
        };
        assert_eq!(defer_reason(sample, GIB, 0, 1), Some("swap_headroom"));
    }

    #[test]
    fn cleanup_needs_no_free_swap_or_ram_for_migration_but_defers_during_stalls() {
        let sample = Snapshot {
            available_bytes: GIB,
            swap_total_bytes: 0,
            swap_free_bytes: 0,
            ..healthy()
        };
        assert_eq!(cleanup_defer_reason(sample, GIB, 2000), None);
        assert_eq!(
            cleanup_defer_reason(
                Snapshot {
                    memory_full_bp: 2000,
                    ..sample
                },
                GIB,
                2000
            ),
            Some("critical_pressure")
        );
        assert_eq!(
            cleanup_defer_reason(
                Snapshot {
                    io_full_bp: 101,
                    ..sample
                },
                GIB,
                2000
            ),
            Some("io_pressure")
        );
        assert_eq!(
            cleanup_defer_reason(
                Snapshot {
                    available_bytes: GIB / 2 - 1,
                    ..sample
                },
                GIB,
                2000
            ),
            Some("critical_pressure")
        );
    }

    #[test]
    fn only_confirmed_completion_or_einval_permits_kernel_guarded_cleanup() {
        assert!(cleanup_may_be_attempted(Some(0)));
        assert!(cleanup_may_be_attempted(Some(22)));
        for errno in [None, Some(1), Some(12), Some(16), Some(-1), Some(4096)] {
            assert!(!cleanup_may_be_attempted(errno));
        }
    }
}
