//! Bounded candidate selection and feedback for proactive file-page reclaim.
//!
//! This module performs no filesystem access and never chooses an unmanaged
//! cgroup. The adapter supplies already-authorized cgroup measurements and
//! serializes workers. PSI gating remains the caller's responsibility.

use crate::MIB;

pub const MIN_CURRENT_BYTES: u64 = 16 * MIB;
pub const MIN_REQUEST_BYTES: u64 = 4 * MIB;
pub const MAX_REQUEST_BYTES: u64 = 128 * MIB;
pub const NO_PROGRESS_BACKOFF_SECS: u64 = 60;
pub const GROWTH_RETRY_BYTES: u64 = 64 * MIB;
const TRACKED_GROUPS: usize = 64;

/// Values from one cgroup identity. `populated` comes from cgroup.events and
/// includes descendants; an empty parent cgroup.procs is not an empty subtree.
/// All memory.stat sizes are bytes. pgsteal_proactive is a page counter.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Sample {
    pub dev: u64,
    pub ino: u64,
    pub populated: bool,
    pub current_bytes: u64,
    pub file_bytes: u64,
    pub inactive_file_bytes: u64,
    pub dirty_bytes: u64,
    pub writeback_bytes: u64,
    pub shmem_bytes: u64,
    pub pgsteal_proactive: Option<u64>,
}

impl Sample {
    fn same_group(self, other: Self) -> bool {
        self.dev == other.dev && self.ino == other.ino
    }

    /// Conservative clean-file budget, not a promise that these pages can be
    /// reclaimed. Dirty/writeback overlap may underestimate it intentionally.
    pub fn clean_file_budget(self) -> u64 {
        self.inactive_file_bytes.min(
            self.file_bytes
                .saturating_sub(self.shmem_bytes)
                .saturating_sub(self.dirty_bytes)
                .saturating_sub(self.writeback_bytes),
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkipReason {
    InvalidIdentity,
    InvalidPageSize,
    Unpopulated,
    TooSmall,
    NoCleanFileBudget,
    RequestTooSmall,
    NoProgressBackoff,
}

impl SkipReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidIdentity => "invalid_cgroup_identity",
            Self::InvalidPageSize => "invalid_page_size",
            Self::Unpopulated => "unpopulated_cgroup",
            Self::TooSmall => "small_cgroup",
            Self::NoCleanFileBudget => "no_clean_file_budget",
            Self::RequestTooSmall => "small_reclaim_request",
            Self::NoProgressBackoff => "no_progress_backoff",
        }
    }
}

/// Keep this alongside the worker; return it to Tracker::finish after exit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Request {
    pub bytes: u64,
    pub before: Sample,
    page_size: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Progress {
    /// The cgroup's proactive-reclaim page counter advanced.
    Reclaimed,
    /// No comparable page counter was available; net usage declined by a page
    /// or more. This is a fallback observation, not attribution of reclaimed IO.
    NetReduction,
    NoProgress,
    /// The group disappeared, its identity changed, or its counter regressed.
    Unavailable,
}

impl Progress {
    pub const fn made_progress(self) -> bool {
        matches!(self, Self::Reclaimed | Self::NetReduction)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reclaimed => "reclaimed",
            Self::NetReduction => "net_reduction",
            Self::NoProgress => "no_progress",
            Self::Unavailable => "measurement_unavailable",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Outcome {
    pub progress: Progress,
    /// Actual counter delta when comparable pgsteal_proactive values exist.
    pub reclaimed_pages: Option<u64>,
    pub reclaimed_bytes: Option<u64>,
    /// Net cgroup usage reduction, measured independently of page reclaim.
    pub net_freed_bytes: u64,
    /// Zero after progress; otherwise an absolute monotonic-seconds retry deadline.
    pub retry_after_secs: u64,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    occupied: bool,
    dev: u64,
    ino: u64,
    failed_at_secs: u64,
    current_after_failure: u64,
}

impl Entry {
    const EMPTY: Self = Self {
        occupied: false,
        dev: 0,
        ino: 0,
        failed_at_secs: 0,
        current_after_failure: 0,
    };

    fn matches(self, sample: Sample) -> bool {
        self.occupied && self.dev == sample.dev && self.ino == sample.ino
    }
}

/// Fixed-size feedback cache keyed by device and inode, so a recreated cgroup
/// does not inherit another group's backoff. No heap allocation is required.
#[derive(Clone, Debug)]
pub struct Tracker {
    entries: [Entry; TRACKED_GROUPS],
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new()
    }
}

impl Tracker {
    pub const fn new() -> Self {
        Self {
            entries: [Entry::EMPTY; TRACKED_GROUPS],
        }
    }

    /// Select a page-aligned request of 4..=128 MiB, also bounded by one eighth
    /// of current usage, the clean-file estimate, and the caller's request.
    /// This does not reserve an in-flight worker; the adapter serializes those.
    pub fn plan(
        &mut self,
        sample: Sample,
        requested_bytes: u64,
        page_size: u64,
        now_secs: u64,
    ) -> Result<Request, SkipReason> {
        if sample.ino == 0 {
            return Err(SkipReason::InvalidIdentity);
        }
        if page_size == 0 || !page_size.is_power_of_two() {
            return Err(SkipReason::InvalidPageSize);
        }
        if !sample.populated {
            return Err(SkipReason::Unpopulated);
        }
        if sample.current_bytes < MIN_CURRENT_BYTES {
            return Err(SkipReason::TooSmall);
        }
        let clean = sample.clean_file_budget();
        if clean < MIN_REQUEST_BYTES {
            return Err(SkipReason::NoCleanFileBudget);
        }
        let amount = requested_bytes
            .min(MAX_REQUEST_BYTES)
            .min(sample.current_bytes / 8)
            .min(clean);
        let amount = amount - amount % page_size;
        if amount < MIN_REQUEST_BYTES {
            return Err(SkipReason::RequestTooSmall);
        }

        if let Some(entry) = self.entries.iter_mut().find(|e| e.matches(sample)) {
            if now_secs < entry.failed_at_secs {
                // A clock regression cannot make a deferred candidate ready.
                entry.failed_at_secs = now_secs;
            }
            let grew = sample
                .current_bytes
                .saturating_sub(entry.current_after_failure)
                >= GROWTH_RETRY_BYTES;
            let elapsed = now_secs.saturating_sub(entry.failed_at_secs) >= NO_PROGRESS_BACKOFF_SECS;
            if !grew && !elapsed {
                return Err(SkipReason::NoProgressBackoff);
            }
            entry.occupied = false;
        }
        Ok(Request {
            bytes: amount,
            before: sample,
            page_size,
        })
    }

    /// Record feedback after a worker exits, including a failed/timed-out one.
    /// Supply None if its cgroup disappeared or cannot be measured. The adapter
    /// should record exit status separately from this measured progress.
    pub fn finish(&mut self, request: Request, after: Option<Sample>, now_secs: u64) -> Outcome {
        let comparable = after.filter(|sample| sample.same_group(request.before));
        let mut outcome = Outcome {
            progress: Progress::Unavailable,
            reclaimed_pages: None,
            reclaimed_bytes: None,
            net_freed_bytes: 0,
            retry_after_secs: 0,
        };
        if let Some(after) = comparable {
            outcome.net_freed_bytes = request
                .before
                .current_bytes
                .saturating_sub(after.current_bytes);
            match (request.before.pgsteal_proactive, after.pgsteal_proactive) {
                (Some(before), Some(after)) if after >= before => {
                    let pages = after - before;
                    outcome.reclaimed_pages = Some(pages);
                    outcome.reclaimed_bytes = Some(pages.saturating_mul(request.page_size));
                    outcome.progress = if pages != 0 {
                        Progress::Reclaimed
                    } else {
                        Progress::NoProgress
                    };
                }
                (Some(_), Some(_)) => {}
                _ => {
                    outcome.progress = if outcome.net_freed_bytes >= request.page_size {
                        Progress::NetReduction
                    } else {
                        Progress::NoProgress
                    };
                }
            }
        }
        if outcome.progress.made_progress() {
            if let Some(entry) = self.entries.iter_mut().find(|e| e.matches(request.before)) {
                entry.occupied = false;
            }
        } else {
            let index = self
                .entries
                .iter()
                .position(|e| e.matches(request.before))
                .or_else(|| self.entries.iter().position(|e| !e.occupied))
                .unwrap_or_else(|| {
                    self.entries
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, e)| e.failed_at_secs)
                        .map(|(i, _)| i)
                        .unwrap_or(0)
                });
            self.entries[index] = Entry {
                occupied: true,
                dev: request.before.dev,
                ino: request.before.ino,
                failed_at_secs: now_secs,
                current_after_failure: comparable
                    .map_or(request.before.current_bytes, |s| s.current_bytes),
            };
            outcome.retry_after_secs = now_secs.saturating_add(NO_PROGRESS_BACKOFF_SECS);
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usable() -> Sample {
        Sample {
            dev: 27,
            ino: 1001,
            populated: true,
            current_bytes: 512 * MIB,
            file_bytes: 256 * MIB,
            inactive_file_bytes: 128 * MIB,
            pgsteal_proactive: Some(100),
            ..Sample::default()
        }
    }

    #[test]
    fn empty_or_tiny_cgroups_never_produce_a_worker_request() {
        let mut tracker = Tracker::new();
        let mut sample = usable();
        sample.current_bytes = 40960;
        sample.populated = false;
        assert_eq!(
            tracker.plan(sample, 64 * MIB, 4096, 0),
            Err(SkipReason::Unpopulated)
        );
        sample.populated = true;
        assert_eq!(
            tracker.plan(sample, 64 * MIB, 4096, 0),
            Err(SkipReason::TooSmall)
        );
    }

    #[test]
    fn anonymous_dirty_or_shmem_only_footprints_are_not_file_reclaim_candidates() {
        let mut tracker = Tracker::new();
        let mut sample = usable();
        sample.file_bytes = 0;
        assert_eq!(
            tracker.plan(sample, 64 * MIB, 4096, 0),
            Err(SkipReason::NoCleanFileBudget)
        );
        sample.file_bytes = 256 * MIB;
        sample.shmem_bytes = 256 * MIB;
        assert_eq!(
            tracker.plan(sample, 64 * MIB, 4096, 0),
            Err(SkipReason::NoCleanFileBudget)
        );
        sample.shmem_bytes = 0;
        sample.dirty_bytes = 128 * MIB;
        sample.writeback_bytes = 128 * MIB;
        assert_eq!(
            tracker.plan(sample, 64 * MIB, 4096, 0),
            Err(SkipReason::NoCleanFileBudget)
        );
    }

    #[test]
    fn requests_are_bounded_by_current_clean_pages_and_native_page_size() {
        let mut tracker = Tracker::new();
        let mut sample = usable();
        sample.inactive_file_bytes = 10 * MIB + 65535;
        let request = tracker.plan(sample, u64::MAX, 65536, 0).unwrap();
        assert_eq!(request.bytes, 10 * MIB);
        sample.current_bytes = u64::MAX;
        sample.file_bytes = u64::MAX;
        sample.inactive_file_bytes = u64::MAX;
        assert_eq!(
            tracker.plan(sample, u64::MAX, 4096, 0).unwrap().bytes,
            MAX_REQUEST_BYTES
        );
        assert_eq!(
            tracker.plan(sample, MIN_REQUEST_BYTES - 1, 4096, 0),
            Err(SkipReason::RequestTooSmall)
        );
    }

    #[test]
    fn reclaim_requests_never_fall_below_four_mib() {
        let mut tracker = Tracker::new();
        let mut sample = usable();
        sample.current_bytes = 32 * MIB - 1;
        assert_eq!(
            tracker.plan(sample, 64 * MIB, 4096, 0),
            Err(SkipReason::RequestTooSmall)
        );
        sample.current_bytes += 1;
        assert_eq!(
            tracker.plan(sample, 64 * MIB, 4096, 0).unwrap().bytes,
            MIN_REQUEST_BYTES
        );
    }

    #[test]
    fn zero_progress_blocks_repeated_attempts_until_sixty_seconds() {
        let mut tracker = Tracker::new();
        let sample = usable();
        let request = tracker.plan(sample, 64 * MIB, 4096, 0).unwrap();
        let outcome = tracker.finish(request, Some(sample), 2);
        assert_eq!(outcome.progress, Progress::NoProgress);
        assert_eq!(outcome.reclaimed_pages, Some(0));
        assert_eq!(outcome.retry_after_secs, 62);
        for now in [3, 10, 30, 61] {
            assert_eq!(
                tracker.plan(sample, 64 * MIB, 4096, now),
                Err(SkipReason::NoProgressBackoff)
            );
        }
        assert!(tracker.plan(sample, 64 * MIB, 4096, 62).is_ok());
    }

    #[test]
    fn growth_of_sixty_four_mib_reopens_an_otherwise_deferred_candidate() {
        let mut tracker = Tracker::new();
        let mut sample = usable();
        let request = tracker.plan(sample, 64 * MIB, 4096, 0).unwrap();
        tracker.finish(request, Some(sample), 1);
        sample.current_bytes += GROWTH_RETRY_BYTES - 1;
        assert_eq!(
            tracker.plan(sample, 64 * MIB, 4096, 2),
            Err(SkipReason::NoProgressBackoff)
        );
        sample.current_bytes += 1;
        assert!(tracker.plan(sample, 64 * MIB, 4096, 2).is_ok());
    }

    #[test]
    fn page_counter_progress_is_visible_even_when_allocations_hide_net_reduction() {
        let mut tracker = Tracker::new();
        let sample = usable();
        let request = tracker.plan(sample, 64 * MIB, 4096, 0).unwrap();
        let mut after = sample;
        after.current_bytes += 16 * MIB;
        after.pgsteal_proactive = Some(116);
        let outcome = tracker.finish(request, Some(after), 1);
        assert_eq!(outcome.progress, Progress::Reclaimed);
        assert_eq!(outcome.reclaimed_pages, Some(16));
        assert_eq!(outcome.reclaimed_bytes, Some(65536));
        assert_eq!(outcome.net_freed_bytes, 0);
        assert_eq!(outcome.retry_after_secs, 0);
        assert!(tracker.plan(after, 64 * MIB, 4096, 2).is_ok());
    }

    #[test]
    fn a_known_page_counter_takes_precedence_over_unattributed_usage_reduction() {
        let mut tracker = Tracker::new();
        let sample = usable();
        let request = tracker.plan(sample, 64 * MIB, 4096, 0).unwrap();
        let mut after = sample;
        after.current_bytes -= 8 * MIB;
        let outcome = tracker.finish(request, Some(after), 1);
        assert_eq!(outcome.progress, Progress::NoProgress);
        assert_eq!(outcome.net_freed_bytes, 8 * MIB);
    }

    #[test]
    fn missing_page_counters_use_a_separately_labeled_net_reduction() {
        let mut tracker = Tracker::new();
        let mut sample = usable();
        sample.pgsteal_proactive = None;
        let request = tracker.plan(sample, 64 * MIB, 4096, 0).unwrap();
        let mut after = sample;
        after.current_bytes -= 8 * MIB;
        let outcome = tracker.finish(request, Some(after), 1);
        assert_eq!(outcome.progress, Progress::NetReduction);
        assert_eq!(outcome.reclaimed_pages, None);
        assert_eq!(outcome.reclaimed_bytes, None);
    }

    #[test]
    fn recreated_groups_do_not_inherit_the_old_inode_backoff() {
        let mut tracker = Tracker::new();
        let sample = usable();
        let request = tracker.plan(sample, 64 * MIB, 4096, 0).unwrap();
        tracker.finish(request, None, 1);
        let mut replacement = sample;
        replacement.ino += 1;
        assert!(tracker.plan(replacement, 64 * MIB, 4096, 2).is_ok());
        assert_eq!(
            tracker.plan(sample, 64 * MIB, 4096, 2),
            Err(SkipReason::NoProgressBackoff)
        );
    }

    #[test]
    fn regressed_counters_and_clocks_do_not_fake_reclaim_progress() {
        let mut tracker = Tracker::new();
        let sample = usable();
        let request = tracker.plan(sample, 64 * MIB, 4096, 1000).unwrap();
        let mut after = sample;
        after.pgsteal_proactive = Some(0);
        assert_eq!(
            tracker.finish(request, Some(after), 1001).progress,
            Progress::Unavailable
        );
        assert_eq!(
            tracker.plan(after, 64 * MIB, 4096, 1),
            Err(SkipReason::NoProgressBackoff)
        );
        assert_eq!(
            tracker.plan(after, 64 * MIB, 4096, 60),
            Err(SkipReason::NoProgressBackoff)
        );
        assert!(tracker.plan(after, 64 * MIB, 4096, 61).is_ok());
    }

    #[test]
    fn invalid_identity_or_page_size_cannot_create_a_request() {
        let mut tracker = Tracker::new();
        let mut sample = usable();
        assert_eq!(
            tracker.plan(sample, 64 * MIB, 0, 0),
            Err(SkipReason::InvalidPageSize)
        );
        assert_eq!(
            tracker.plan(sample, 64 * MIB, 4097, 0),
            Err(SkipReason::InvalidPageSize)
        );
        sample.ino = 0;
        assert_eq!(
            tracker.plan(sample, 64 * MIB, 4096, 0),
            Err(SkipReason::InvalidIdentity)
        );
    }
}
