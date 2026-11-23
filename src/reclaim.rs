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
pub const REFAULT_WINDOW_SECS: u64 = 30;
pub const REFAULT_BACKOFF_SECS: u64 = 60;
const TRACKED_GROUPS: usize = 64;

/// Values from one cgroup identity. `populated` comes from cgroup.events and
/// includes descendants; an empty parent cgroup.procs is not an empty subtree.
/// All memory.stat sizes are bytes. pgsteal_proactive and
/// workingset_refault_file are page counters.
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
    pub workingset_refault_file: Option<u64>,
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
    RefaultBackoff,
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
            Self::RefaultBackoff => "working_set_refault_backoff",
        }
    }
}

/// Keep this alongside the worker; return it to Tracker::finish after exit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Request {
    pub bytes: u64,
    pub before: Sample,
    page_size: u64,
    observed_at_secs: u64,
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
    /// Absolute monotonic-seconds retry deadline while attempts are deferred;
    /// zero when no backoff is active.
    pub retry_after_secs: u64,
}

#[derive(Clone, Copy, Debug)]
enum Feedback {
    Empty,
    NoProgress {
        current_bytes: u64,
    },
    RefaultWatch {
        before_pages: u64,
        last_pages: u64,
        threshold_pages: u64,
        page_size: u64,
    },
    RefaultBackoff,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    dev: u64,
    ino: u64,
    observed_at_secs: u64,
    feedback: Feedback,
}

impl Entry {
    const EMPTY: Self = Self {
        dev: 0,
        ino: 0,
        observed_at_secs: 0,
        feedback: Feedback::Empty,
    };

    fn matches(self, sample: Sample) -> bool {
        !matches!(self.feedback, Feedback::Empty)
            && self.dev == sample.dev
            && self.ino == sample.ino
    }

    fn expired(self, now_secs: u64) -> bool {
        let lifetime = match self.feedback {
            Feedback::Empty => return true,
            Feedback::NoProgress { .. } => NO_PROGRESS_BACKOFF_SECS,
            Feedback::RefaultWatch { .. } => REFAULT_WINDOW_SECS,
            Feedback::RefaultBackoff => REFAULT_BACKOFF_SECS,
        };
        now_secs
            .checked_sub(self.observed_at_secs)
            .is_some_and(|elapsed| elapsed >= lifetime)
    }

    fn defer(&mut self, sample: Sample, page_size: u64, now_secs: u64) -> Option<SkipReason> {
        if now_secs < self.observed_at_secs {
            // A reset clock invalidates a rate window, but must not release
            // an existing backoff before another complete interval passes.
            if matches!(self.feedback, Feedback::RefaultWatch { .. }) {
                self.feedback = Feedback::Empty;
            }
            self.observed_at_secs = now_secs;
        }
        let elapsed = now_secs.saturating_sub(self.observed_at_secs);
        match self.feedback {
            Feedback::NoProgress { current_bytes }
                if elapsed < NO_PROGRESS_BACKOFF_SECS
                    && sample.current_bytes.saturating_sub(current_bytes) < GROWTH_RETRY_BYTES =>
            {
                Some(SkipReason::NoProgressBackoff)
            }
            Feedback::RefaultBackoff if elapsed < REFAULT_BACKOFF_SECS => {
                Some(SkipReason::RefaultBackoff)
            }
            Feedback::RefaultWatch {
                before_pages,
                last_pages,
                threshold_pages,
                page_size: previous_page_size,
            } if elapsed < REFAULT_WINDOW_SECS && page_size == previous_page_size => {
                match sample.workingset_refault_file {
                    Some(pages) if pages >= last_pages => {
                        if pages - before_pages >= threshold_pages {
                            // Refaults do not identify which reclaim caused
                            // them. Pause optional work when the same group's
                            // file working set is being used again rapidly.
                            self.feedback = Feedback::RefaultBackoff;
                            self.observed_at_secs = now_secs;
                            Some(SkipReason::RefaultBackoff)
                        } else {
                            self.feedback = Feedback::RefaultWatch {
                                before_pages,
                                last_pages: pages,
                                threshold_pages,
                                page_size,
                            };
                            None
                        }
                    }
                    _ => {
                        self.feedback = Feedback::Empty;
                        None
                    }
                }
            }
            _ => {
                self.feedback = Feedback::Empty;
                None
            }
        }
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
    /// A successful reclaim is observed for 30 seconds. Refaults of at least
    /// 4 MiB and half its reclaimed pages pause further attempts for 60 seconds;
    /// without a reclaim counter, the request supplies the comparison amount.
    /// Further successful requests keep an existing observation's baseline
    /// and deadline until its window expires.
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

        if let Some(entry) = self.entries.iter_mut().find(|e| e.matches(sample))
            && let Some(reason) = entry.defer(sample, page_size, now_secs)
        {
            return Err(reason);
        }
        Ok(Request {
            bytes: amount,
            before: sample,
            page_size,
            observed_at_secs: now_secs,
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
            let refaults = request
                .before
                .workingset_refault_file
                .zip(comparable.and_then(|sample| sample.workingset_refault_file))
                .filter(|(before, after)| after >= before);
            if let Some((before_pages, last_pages)) = refaults.filter(|_| {
                now_secs >= request.observed_at_secs
                    && now_secs - request.observed_at_secs < REFAULT_WINDOW_SECS
            }) {
                if let Some((entry, after)) = self
                    .entries
                    .iter_mut()
                    .find(|entry| {
                        entry.matches(request.before)
                            && matches!(
                                entry.feedback,
                                Feedback::RefaultWatch { .. } | Feedback::RefaultBackoff
                            )
                    })
                    .zip(comparable)
                {
                    // Requests may succeed more often than the observation
                    // window ends. Keep its first baseline so slower refaults
                    // cannot disappear behind repeated successful requests.
                    entry.defer(after, request.page_size, now_secs);
                    match entry.feedback {
                        Feedback::RefaultWatch { .. } => return outcome,
                        Feedback::RefaultBackoff => {
                            outcome.retry_after_secs =
                                entry.observed_at_secs.saturating_add(REFAULT_BACKOFF_SECS);
                            return outcome;
                        }
                        _ => {}
                    }
                }
                let reclaimed_pages = outcome
                    .reclaimed_pages
                    .unwrap_or(request.bytes / request.page_size);
                let mut entry = Entry {
                    dev: request.before.dev,
                    ino: request.before.ino,
                    observed_at_secs: request.observed_at_secs,
                    feedback: Feedback::RefaultWatch {
                        before_pages,
                        last_pages,
                        threshold_pages: reclaimed_pages
                            .div_ceil(2)
                            .max(MIN_REQUEST_BYTES.div_ceil(request.page_size)),
                        page_size: request.page_size,
                    },
                };
                if let Some(after) = comparable
                    && entry.defer(after, request.page_size, now_secs)
                        == Some(SkipReason::RefaultBackoff)
                {
                    outcome.retry_after_secs = now_secs.saturating_add(REFAULT_BACKOFF_SECS);
                }
                self.record(entry);
            } else if let Some(entry) = self.entries.iter_mut().find(|e| e.matches(request.before))
            {
                entry.feedback = Feedback::Empty;
            }
        } else {
            self.record(Entry {
                dev: request.before.dev,
                ino: request.before.ino,
                observed_at_secs: now_secs,
                feedback: Feedback::NoProgress {
                    current_bytes: comparable
                        .map_or(request.before.current_bytes, |s| s.current_bytes),
                },
            });
            outcome.retry_after_secs = now_secs.saturating_add(NO_PROGRESS_BACKOFF_SECS);
        }
        outcome
    }

    fn record(&mut self, entry: Entry) {
        let slot = self
            .entries
            .iter()
            .position(|e| e.dev == entry.dev && e.ino == entry.ino)
            .or_else(|| {
                self.entries
                    .iter()
                    .position(|e| e.expired(entry.observed_at_secs))
            })
            .or_else(|| {
                self.entries
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| matches!(e.feedback, Feedback::RefaultWatch { .. }))
                    .min_by_key(|(_, e)| e.observed_at_secs)
                    .map(|(i, _)| i)
            });
        // A speculative success watch must not displace an established
        // no-progress/refault backoff when every cache slot is already used.
        let index = match slot {
            Some(index) => index,
            None if matches!(entry.feedback, Feedback::RefaultWatch { .. }) => return,
            None => self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.observed_at_secs)
                .map(|(i, _)| i)
                .unwrap_or(0),
        };
        self.entries[index] = entry;
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

    fn refault_sample() -> Sample {
        Sample {
            workingset_refault_file: Some(4096),
            ..usable()
        }
    }

    fn reclaimed(mut sample: Sample, bytes: u64, page_size: u64) -> Sample {
        sample.current_bytes -= bytes;
        sample.pgsteal_proactive = sample
            .pgsteal_proactive
            .map(|pages| pages + bytes / page_size);
        sample
    }

    #[test]
    fn rapid_refaults_pause_successful_reclaim_even_when_usage_grows() {
        let mut tracker = Tracker::new();
        let before = refault_sample();
        let request = tracker.plan(before, 64 * MIB, 4096, 0).unwrap();
        let mut after = reclaimed(before, 64 * MIB, 4096);
        let outcome = tracker.finish(request, Some(after), 2);
        assert_eq!(outcome.progress, Progress::Reclaimed);
        assert_eq!(outcome.reclaimed_bytes, Some(64 * MIB));
        assert_eq!(outcome.retry_after_secs, 0);

        after.workingset_refault_file = Some(4096 + 32 * MIB / 4096);
        assert_eq!(
            tracker.plan(after, 64 * MIB, 4096, 10),
            Err(SkipReason::RefaultBackoff)
        );
        after.current_bytes += 128 * MIB;
        for now in [11, 30, 69] {
            assert_eq!(
                tracker.plan(after, 64 * MIB, 4096, now),
                Err(SkipReason::RefaultBackoff)
            );
        }
        assert!(tracker.plan(after, 64 * MIB, 4096, 70).is_ok());
        assert!(tracker.plan(after, 64 * MIB, 4096, 71).is_ok());
    }

    #[test]
    fn refaults_during_the_worker_are_included_in_the_short_window() {
        let mut tracker = Tracker::new();
        let before = refault_sample();
        let request = tracker.plan(before, 64 * MIB, 4096, 0).unwrap();
        let mut after = reclaimed(before, 64 * MIB, 4096);
        after.workingset_refault_file = Some(4096 + 32 * MIB / 4096);
        tracker.finish(request, Some(after), 2);
        assert_eq!(
            tracker.plan(after, 64 * MIB, 4096, 10),
            Err(SkipReason::RefaultBackoff)
        );
    }

    #[test]
    fn repeated_success_keeps_the_original_refault_baseline() {
        for page_size in [4096, 65536] {
            for measured_reclaim in [true, false] {
                let mut tracker = Tracker::new();
                let before = Sample {
                    pgsteal_proactive: measured_reclaim.then_some(100),
                    ..refault_sample()
                };
                let request = tracker.plan(before, 64 * MIB, page_size, 0).unwrap();
                let mut after = reclaimed(before, 64 * MIB, page_size);
                tracker.finish(request, Some(after), 1);

                after.current_bytes = before.current_bytes;
                after.workingset_refault_file = Some(4096 + 16 * MIB / page_size);
                let request = tracker.plan(after, 64 * MIB, page_size, 10).unwrap();
                after = reclaimed(after, 64 * MIB, page_size);
                assert!(
                    tracker
                        .finish(request, Some(after), 11)
                        .progress
                        .made_progress()
                );

                // Each ten-second interval refaults less than half of a
                // request, but the first request's window reaches its limit.
                after.workingset_refault_file = Some(4096 + 32 * MIB / page_size);
                assert_eq!(
                    tracker.plan(after, 64 * MIB, page_size, 20),
                    Err(SkipReason::RefaultBackoff)
                );
                assert_eq!(
                    tracker.plan(after, 64 * MIB, page_size, 79),
                    Err(SkipReason::RefaultBackoff)
                );
                assert!(tracker.plan(after, 64 * MIB, page_size, 80).is_ok());
            }
        }
    }

    #[test]
    fn later_worker_refaults_enter_backoff_when_it_finishes() {
        let mut tracker = Tracker::new();
        let before = refault_sample();
        let request = tracker.plan(before, 64 * MIB, 4096, 0).unwrap();
        let mut after = reclaimed(before, 64 * MIB, 4096);
        tracker.finish(request, Some(after), 1);

        after.current_bytes = before.current_bytes;
        after.workingset_refault_file = Some(4096 + 16 * MIB / 4096);
        let request = tracker.plan(after, 64 * MIB, 4096, 10).unwrap();
        after = reclaimed(after, 64 * MIB, 4096);
        after.workingset_refault_file = Some(4096 + 32 * MIB / 4096);
        let outcome = tracker.finish(request, Some(after), 12);
        assert_eq!(outcome.progress, Progress::Reclaimed);
        assert_eq!(outcome.retry_after_secs, 72);
        assert_eq!(
            tracker.plan(after, 64 * MIB, 4096, 20),
            Err(SkipReason::RefaultBackoff)
        );
        assert!(tracker.plan(after, 64 * MIB, 4096, 72).is_ok());
    }

    #[test]
    fn repeated_success_does_not_extend_the_original_observation_deadline() {
        let mut tracker = Tracker::new();
        let before = refault_sample();
        let request = tracker.plan(before, 64 * MIB, 4096, 0).unwrap();
        let mut after = reclaimed(before, 64 * MIB, 4096);
        tracker.finish(request, Some(after), 1);

        after.current_bytes = before.current_bytes;
        after.workingset_refault_file = Some(4096 + 8 * MIB / 4096);
        let request = tracker.plan(after, 64 * MIB, 4096, 20).unwrap();
        after = reclaimed(after, 64 * MIB, 4096);
        tracker.finish(request, Some(after), 21);

        after.workingset_refault_file = Some(4096 + 40 * MIB / 4096);
        assert!(tracker.plan(after, 64 * MIB, 4096, 30).is_ok());
    }

    #[test]
    fn cold_pages_and_expired_windows_keep_reclaim_available() {
        let mut tracker = Tracker::new();
        let before = refault_sample();
        let request = tracker.plan(before, 64 * MIB, 4096, 0).unwrap();
        let mut after = reclaimed(before, 64 * MIB, 4096);
        tracker.finish(request, Some(after), 2);
        assert!(tracker.plan(after, 64 * MIB, 4096, 10).is_ok());
        after.workingset_refault_file = Some(4096 + 32 * MIB / 4096 - 1);
        assert!(tracker.plan(after, 64 * MIB, 4096, 20).is_ok());
        after.workingset_refault_file = Some(u64::MAX);
        assert!(tracker.plan(after, 64 * MIB, 4096, 30).is_ok());

        let request = tracker.plan(before, 64 * MIB, 4096, 40).unwrap();
        // A worker completing outside the observation window must not turn
        // unrelated, long-accumulated refaults into a fresh short-window signal.
        tracker.finish(request, Some(after), 70);
        assert!(tracker.plan(after, 64 * MIB, 4096, 71).is_ok());
    }

    #[test]
    fn refault_guard_requires_four_mib_with_either_native_page_size() {
        for page_size in [4096, 65536] {
            let mut tracker = Tracker::new();
            let before = refault_sample();
            let request = tracker.plan(before, 64 * MIB, page_size, 0).unwrap();
            let mut after = reclaimed(before, page_size, page_size);
            tracker.finish(request, Some(after), 2);
            after.workingset_refault_file = Some(4096 + 4 * MIB / page_size - 1);
            assert!(tracker.plan(after, 64 * MIB, page_size, 10).is_ok());
            after.workingset_refault_file = Some(4096 + 4 * MIB / page_size);
            assert_eq!(
                tracker.plan(after, 64 * MIB, page_size, 12),
                Err(SkipReason::RefaultBackoff)
            );
        }
    }

    #[test]
    fn net_reduction_fallback_compares_refaults_with_the_requested_amount() {
        let mut tracker = Tracker::new();
        let before = Sample {
            pgsteal_proactive: None,
            ..refault_sample()
        };
        let request = tracker.plan(before, 64 * MIB, 4096, 0).unwrap();
        let mut after = reclaimed(before, 8 * MIB, 4096);
        let outcome = tracker.finish(request, Some(after), 2);
        assert_eq!(outcome.progress, Progress::NetReduction);
        assert_eq!(outcome.reclaimed_bytes, None);
        after.workingset_refault_file = Some(4096 + 8 * MIB / 4096);
        assert!(tracker.plan(after, 64 * MIB, 4096, 10).is_ok());
        after.workingset_refault_file = Some(4096 + 32 * MIB / 4096);
        assert_eq!(
            tracker.plan(after, 64 * MIB, 4096, 12),
            Err(SkipReason::RefaultBackoff)
        );
    }

    #[test]
    fn missing_and_regressed_refault_counters_discard_the_observation() {
        for (before_pages, after_pages, next_pages) in [
            (None, Some(4096), Some(u64::MAX)),
            (Some(4096), None, Some(u64::MAX)),
            (Some(4096), Some(4095), Some(u64::MAX)),
            (Some(4096), Some(4096), None),
            (Some(4096), Some(4097), Some(4096)),
        ] {
            let mut tracker = Tracker::new();
            let before = Sample {
                workingset_refault_file: before_pages,
                ..usable()
            };
            let request = tracker.plan(before, 64 * MIB, 4096, 0).unwrap();
            let mut after = reclaimed(before, 64 * MIB, 4096);
            after.workingset_refault_file = after_pages;
            tracker.finish(request, Some(after), 2);
            after.workingset_refault_file = next_pages;
            assert!(tracker.plan(after, 64 * MIB, 4096, 10).is_ok());
            after.workingset_refault_file = Some(u64::MAX);
            assert!(tracker.plan(after, 64 * MIB, 4096, 12).is_ok());
        }
    }

    #[test]
    fn refault_feedback_is_isolated_by_cgroup_identity() {
        let mut tracker = Tracker::new();
        let before = refault_sample();
        let request = tracker.plan(before, 64 * MIB, 4096, 0).unwrap();
        let mut after = reclaimed(before, 64 * MIB, 4096);
        tracker.finish(request, Some(after), 2);
        after.workingset_refault_file = Some(u64::MAX);
        for replacement in [
            Sample {
                dev: after.dev + 1,
                ..after
            },
            Sample {
                ino: after.ino + 1,
                ..after
            },
        ] {
            assert!(tracker.plan(replacement, 64 * MIB, 4096, 10).is_ok());
        }
        assert_eq!(
            tracker.plan(after, 64 * MIB, 4096, 10),
            Err(SkipReason::RefaultBackoff)
        );
        let replacement = Sample {
            ino: after.ino + 1,
            ..after
        };
        assert!(tracker.plan(replacement, 64 * MIB, 4096, 12).is_ok());
    }

    #[test]
    fn clock_regressions_discard_watches_but_restart_existing_refault_backoff() {
        for enter_backoff in [false, true] {
            let mut tracker = Tracker::new();
            let before = refault_sample();
            let request = tracker.plan(before, 64 * MIB, 4096, 1000).unwrap();
            let mut after = reclaimed(before, 64 * MIB, 4096);
            tracker.finish(request, Some(after), 1001);
            after.workingset_refault_file = Some(u64::MAX);
            if enter_backoff {
                assert_eq!(
                    tracker.plan(after, 64 * MIB, 4096, 1002),
                    Err(SkipReason::RefaultBackoff)
                );
            }
            for now in [1, 20, 60] {
                let result = tracker.plan(after, 64 * MIB, 4096, now);
                if enter_backoff {
                    assert_eq!(result, Err(SkipReason::RefaultBackoff));
                } else {
                    assert!(result.is_ok());
                }
            }
            assert!(tracker.plan(after, 64 * MIB, 4096, 61).is_ok());
        }
    }

    #[test]
    fn feedback_capacity_remains_bounded_and_new_groups_replace_the_oldest() {
        let mut tracker = Tracker::new();
        for index in 0..=TRACKED_GROUPS {
            let before = Sample {
                ino: 1001 + index as u64,
                ..refault_sample()
            };
            let request = tracker.plan(before, 64 * MIB, 4096, 0).unwrap();
            tracker.finish(request, Some(reclaimed(before, 64 * MIB, 4096)), 1);
        }
        assert_eq!(
            tracker
                .entries
                .iter()
                .filter(|entry| !matches!(entry.feedback, Feedback::Empty))
                .count(),
            TRACKED_GROUPS
        );
        let sample = Sample {
            workingset_refault_file: Some(u64::MAX),
            ..refault_sample()
        };
        assert!(tracker.plan(sample, 64 * MIB, 4096, 2).is_ok());
        for index in 1..=TRACKED_GROUPS {
            let retained = Sample {
                ino: 1001 + index as u64,
                ..sample
            };
            assert_eq!(
                tracker.plan(retained, 64 * MIB, 4096, 2),
                Err(SkipReason::RefaultBackoff)
            );
        }
    }

    #[test]
    fn successful_groups_cannot_evict_existing_no_progress_backoffs() {
        let mut tracker = Tracker::new();
        for index in 0..TRACKED_GROUPS {
            let before = Sample {
                ino: 1001 + index as u64,
                ..refault_sample()
            };
            let request = tracker.plan(before, 64 * MIB, 4096, 0).unwrap();
            tracker.finish(request, Some(before), 1);
        }
        let before = Sample {
            ino: 1001 + TRACKED_GROUPS as u64,
            ..refault_sample()
        };
        let request = tracker.plan(before, 64 * MIB, 4096, 2).unwrap();
        tracker.finish(request, Some(reclaimed(before, 64 * MIB, 4096)), 3);
        for index in 0..TRACKED_GROUPS {
            let retained = Sample {
                ino: 1001 + index as u64,
                ..refault_sample()
            };
            assert_eq!(
                tracker.plan(retained, 64 * MIB, 4096, 4),
                Err(SkipReason::NoProgressBackoff)
            );
        }
    }

    #[test]
    fn expired_backoffs_release_slots_without_displacing_active_backoffs() {
        for refault_backoff in [false, true] {
            let mut tracker = Tracker::new();
            for index in 0..TRACKED_GROUPS {
                let started = if index < TRACKED_GROUPS / 2 { 0 } else { 40 };
                let before = Sample {
                    ino: 1001 + index as u64,
                    ..refault_sample()
                };
                let request = tracker.plan(before, 64 * MIB, 4096, started).unwrap();
                if refault_backoff {
                    let mut after = reclaimed(before, 64 * MIB, 4096);
                    after.workingset_refault_file = Some(4096 + 32 * MIB / 4096);
                    tracker.finish(request, Some(after), started + 1);
                    assert_eq!(
                        tracker.plan(after, 64 * MIB, 4096, started + 2),
                        Err(SkipReason::RefaultBackoff)
                    );
                } else {
                    tracker.finish(request, Some(before), started + 1);
                }
            }
            // The first half's backoffs have expired without those groups
            // being sampled again. A new group must be able to keep feedback.
            let now = if refault_backoff { 62 } else { 61 };
            let before = Sample {
                ino: 1001 + TRACKED_GROUPS as u64,
                ..refault_sample()
            };
            let request = tracker.plan(before, 64 * MIB, 4096, now).unwrap();
            let mut after = reclaimed(before, 64 * MIB, 4096);
            tracker.finish(request, Some(after), now + 1);
            after.workingset_refault_file = Some(4096 + 32 * MIB / 4096);
            assert_eq!(
                tracker.plan(after, 64 * MIB, 4096, now + 2),
                Err(SkipReason::RefaultBackoff)
            );
            for index in TRACKED_GROUPS / 2..TRACKED_GROUPS {
                let retained = Sample {
                    ino: 1001 + index as u64,
                    ..refault_sample()
                };
                assert_eq!(
                    tracker.plan(retained, 64 * MIB, 4096, now + 2),
                    Err(if refault_backoff {
                        SkipReason::RefaultBackoff
                    } else {
                        SkipReason::NoProgressBackoff
                    })
                );
            }
        }
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
