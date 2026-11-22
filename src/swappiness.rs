//! A relative paging-cost preference, not a target for how full RAM should be.
//! Evidence of repeatedly evicted file working sets takes precedence over an
//! undifferentiated IO-busy signal. Requests are held and stepped to avoid
//! turning noisy two-second samples into alternating global kernel settings.

const MIB: u64 = 1024 * 1024;
// Conservative starting thresholds for observed reclaim contention, not
// kernel recommendations or a measurement of pointer/input latency.
const RECLAIM_GUARD_FULL_BP: u16 = 100;
const RECLAIM_GUARD_SCAN_BYTES: u64 = 64 * MIB;
const RECLAIM_GUARD_ANON_BYTES: u64 = 4 * MIB;
const RECLAIM_GUARD_CEILING: u16 = 60;

#[derive(Clone, Copy, Debug, Default)]
pub struct Input {
    pub rates_available: bool,
    pub file_refault: u64,
    pub anon_refault: u64,
    pub direct_scan: u64,
    pub memory_full_bp: u16,
    pub swap_in: u64,
    pub disk_swap_in: Option<u64>,
    pub efficient_zram_room: bool,
    pub swap_or_io_busy: bool,
    pub hint: u16,
    pub interactive: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Preference {
    pub value: u16,
    pub reason: &'static str,
    pub urgent: bool,
    /// A ceiling may withdraw a promotion but must not raise a lower read-back.
    pub ceiling_only: bool,
}

pub fn preference(i: Input) -> Preference {
    // A file refault means a page was evicted and used again. It is a working
    // set signal, not a measurement of bytes transferred from a physical disk.
    let file_dominates = i.rates_available
        && i.file_refault >= 8 * MIB
        && i.direct_scan >= 8 * MIB
        && i.file_refault >= i.anon_refault.saturating_mul(2)
        && i.file_refault >= i.swap_in.saturating_mul(2);
    let disk_churn = i.disk_swap_in.is_some_and(|rate| rate >= 32 * MIB);
    if disk_churn {
        return Preference {
            value: i.hint.min(40),
            reason: "disk_swap_read_pressure",
            urgent: true,
            ceiling_only: false,
        };
    }
    // Global refault counts do not identify foreground working sets or the
    // relative cost of file and anonymous paging. When direct reclaim and
    // anonymous refaults coincide with measured stalls, withdraw automatic
    // file/zram promotion through the ordinary governor rather than assuming
    // that keeping more file cache will improve interactive responsiveness.
    if i.rates_available
        && i.direct_scan >= RECLAIM_GUARD_SCAN_BYTES
        && i.anon_refault >= RECLAIM_GUARD_ANON_BYTES
        && i.memory_full_bp >= RECLAIM_GUARD_FULL_BP
    {
        return Preference {
            value: RECLAIM_GUARD_CEILING,
            reason: "reclaim_latency_guard",
            urgent: false,
            ceiling_only: true,
        };
    }
    if file_dominates {
        return Preference {
            value: if i.efficient_zram_room { 100 } else { 80 },
            reason: "file_working_set_refaults",
            urgent: false,
            ceiling_only: false,
        };
    }
    // PSI alone cannot establish that compressed swap is expensive. In the
    // absence of warming samples, keep the device-agnostic policy preference.
    if i.rates_available && i.efficient_zram_room && !i.swap_or_io_busy {
        return Preference {
            value: if i.interactive { 100 } else { 120 },
            reason: "efficient_zram_headroom",
            urgent: false,
            ceiling_only: false,
        };
    }
    Preference {
        // The old hint of 20 also covered unrelated filesystem IO. Without
        // measured disk swap reads, that is not evidence that anonymous pages
        // should displace a repeatedly used file working set. Fall back to the
        // device-neutral preference rather than preserving that inference.
        value: i.hint.clamp(60, 80),
        reason: if i.rates_available {
            if i.swap_or_io_busy {
                "unattributed_io_pressure"
            } else {
                "balanced_paging_cost"
            }
        } else {
            "warming_sample"
        },
        urgent: false,
        ceiling_only: false,
    }
}

const SUPPORT_REQUIRED_SECS: u64 = 10;
const SUPPORT_MAX_SECS: u64 = 20;
const SAMPLE_CREDIT_MAX_SECS: u64 = 10;
const DISK_CONFIRM_SECS: u64 = 6;
const WRITE_INTERVAL_SECS: u64 = 30;

#[derive(Clone, Copy, Debug)]
struct Evidence {
    preference: Preference,
    support_secs: u64,
}

#[derive(Debug, Default)]
pub struct Governor {
    // Only actual read-back values enter current. A proposed write may fail.
    current: Option<u16>,
    changed_at: Option<u64>,
    pending: Option<(u16, u64)>,
    candidate: Option<Evidence>,
    last_observed: Option<u64>,
    urgent_support_secs: u64,
    last_urgent: bool,
}

impl Governor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe(
        &mut self,
        mut desired: Preference,
        actual: Option<u16>,
        now: u64,
    ) -> Preference {
        let elapsed = self
            .last_observed
            .map_or(0, |previous| now.saturating_sub(previous))
            .min(SAMPLE_CREDIT_MAX_SECS);
        if self.last_observed.is_some_and(|previous| now < previous) {
            self.changed_at = Some(now);
            self.pending = None;
            self.candidate = None;
            self.urgent_support_secs = 0;
            self.last_urgent = false;
        }
        self.last_observed = Some(now);

        let Some(current) = actual.map(|value| value.min(200)) else {
            self.pending = None;
            self.candidate = None;
            self.urgent_support_secs = 0;
            self.last_urgent = false;
            return Preference {
                value: self.current.unwrap_or(desired.value).min(200),
                reason: "readback_unavailable",
                urgent: false,
                ceiling_only: false,
            };
        };
        if desired.ceiling_only {
            desired.value = desired.value.min(current);
        }
        if self.current.is_some_and(|previous| previous != current) {
            // A matching read-back confirms the preceding proposal. External
            // changes are also a new baseline and restart the normal cooldown.
            self.changed_at = Some(
                self.pending
                    .filter(|(value, when)| *value == current && *when <= now)
                    .map_or(now, |(_, when)| when),
            );
            self.pending = None;
        } else {
            // The preceding proposal did not change the observed value. A
            // later matching external value must not confirm that old attempt.
            self.pending = None;
        }
        self.current = Some(current);

        if desired.reason == "warming_sample" {
            self.candidate = None;
            self.urgent_support_secs = 0;
            self.last_urgent = false;
            return hold(current, "warming_sample");
        }
        self.urgent_support_secs = if desired.urgent && self.last_urgent {
            self.urgent_support_secs
                .saturating_add(elapsed)
                .min(DISK_CONFIRM_SECS)
        } else {
            0
        };
        self.last_urgent = desired.urgent;
        if desired.urgent
            && desired.value < current
            && self.urgent_support_secs >= DISK_CONFIRM_SECS
        {
            self.candidate = None;
            self.pending = Some((desired.value, now));
            return desired;
        }

        self.accumulate(desired, current, elapsed);
        let Some(candidate) = self.candidate else {
            return hold(current, desired.reason);
        };
        // Opposing samples subtract bounded support instead of erasing it.
        // A short disk burst therefore cannot indefinitely veto a dominant
        // file-working-set signal. Do not raise during the burst itself, and
        // require continued disk pressure before making an urgent reduction.
        if desired.urgent
            || candidate.support_secs < SUPPORT_REQUIRED_SECS
            || !same_direction(candidate.preference.value, desired.value, current)
            || self
                .changed_at
                .is_some_and(|changed| now.saturating_sub(changed) < WRITE_INTERVAL_SECS)
        {
            return hold(current, "hysteresis_hold");
        }
        let target = candidate.preference.value;
        let next = if target > current {
            target.min(current.saturating_add(20))
        } else {
            target.max(current.saturating_sub(20))
        };
        self.pending = Some((next, now));
        Preference {
            value: next,
            reason: candidate.preference.reason,
            urgent: false,
            ceiling_only: candidate.preference.ceiling_only,
        }
    }

    fn accumulate(&mut self, desired: Preference, current: u16, elapsed: u64) {
        let previous = self
            .candidate
            .filter(|candidate| candidate.preference.value != current);
        self.candidate = match previous {
            Some(mut candidate)
                if same_direction(candidate.preference.value, desired.value, current) =>
            {
                // Targets on the same side support the same conservative
                // movement. Use the nearer target, so alternating 80/100 does
                // not repeatedly discard evidence for moving from 40 to 80.
                if desired.value.abs_diff(current) < candidate.preference.value.abs_diff(current) {
                    candidate.preference = desired;
                }
                candidate.support_secs = candidate
                    .support_secs
                    .saturating_add(elapsed)
                    .min(SUPPORT_MAX_SECS);
                Some(candidate)
            }
            Some(mut candidate) if candidate.support_secs > elapsed => {
                candidate.support_secs -= elapsed;
                Some(candidate)
            }
            Some(candidate) if desired.value != current => Some(Evidence {
                preference: desired,
                support_secs: elapsed.saturating_sub(candidate.support_secs),
            }),
            None if desired.value != current => Some(Evidence {
                preference: desired,
                support_secs: 0,
            }),
            _ => None,
        };
    }
}

fn same_direction(a: u16, b: u16, current: u16) -> bool {
    (a > current && b > current) || (a < current && b < current)
}

fn hold(value: u16, reason: &'static str) -> Preference {
    Preference {
        value,
        reason,
        urgent: false,
        ceiling_only: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reclaim_contention() -> Input {
        Input {
            rates_available: true,
            file_refault: 79 * MIB,
            anon_refault: 57 * MIB / 10,
            direct_scan: 156 * MIB,
            memory_full_bp: 123,
            hint: 60,
            ..Input::default()
        }
    }

    #[test]
    fn observed_reclaim_combination_caps_file_and_zram_promotions() {
        for efficient_zram_room in [false, true] {
            for file_refault in [0, 79 * MIB] {
                let desired = preference(Input {
                    file_refault,
                    efficient_zram_room,
                    ..reclaim_contention()
                });
                assert_eq!(desired.value, 60);
                assert_eq!(desired.reason, "reclaim_latency_guard");
                assert!(desired.ceiling_only);
                assert!(!desired.urgent);
            }
        }
    }

    #[test]
    fn reclaim_guard_requires_all_three_thresholds_and_warmed_rates() {
        let boundary = Input {
            direct_scan: 64 * MIB,
            anon_refault: 4 * MIB,
            memory_full_bp: 100,
            ..reclaim_contention()
        };
        assert!(preference(boundary).ceiling_only);
        for below in [
            Input {
                direct_scan: 64 * MIB - 1,
                ..boundary
            },
            Input {
                anon_refault: 4 * MIB - 1,
                ..boundary
            },
            Input {
                memory_full_bp: 99,
                ..boundary
            },
            Input {
                rates_available: false,
                ..boundary
            },
        ] {
            assert!(!preference(below).ceiling_only, "{below:?}");
        }
    }

    #[test]
    fn sustained_reclaim_guard_withdraws_eighty_after_ordinary_support() {
        let mut governor = Governor::new();
        let desired = preference(reclaim_contention());
        let mut actual = 80;
        let mut writes = Vec::new();
        for now in (0..=60).step_by(2) {
            let proposal = governor.observe(desired, Some(actual), now);
            assert!(!proposal.urgent);
            assert!(proposal.value <= actual);
            if proposal.value != actual {
                writes.push((now, proposal.value));
                actual = proposal.value;
            }
        }
        assert_eq!(actual, 60);
        assert_eq!(writes, [(10, 60)]);
    }

    #[test]
    fn reclaim_ceiling_never_raises_a_lower_actual_or_a_failed_write_baseline() {
        let desired = preference(reclaim_contention());
        for actual in [0, 20, 40, 59, 60] {
            let mut governor = Governor::new();
            // Leave an unsupported/failed upward proposal in the old history.
            for now in (0..=20).step_by(2) {
                governor.observe(hold(120, "efficient_zram_headroom"), Some(actual), now);
            }
            for now in (22..=90).step_by(2) {
                assert_eq!(governor.observe(desired, Some(actual), now).value, actual);
            }
        }
        let mut governor = Governor::new();
        assert_eq!(governor.observe(desired, Some(120), 0).value, 120);
        assert_eq!(governor.observe(desired, Some(120), 10).value, 100);
        // A failed reduction must not be mistaken for a confirmed new value.
        assert_eq!(governor.observe(desired, Some(120), 40).value, 100);
        let unknown = governor.observe(desired, None, 42);
        assert_eq!(unknown.value, 120);
        assert_eq!(unknown.reason, "readback_unavailable");
    }

    #[test]
    fn short_or_alternating_reclaim_guard_samples_do_not_lower_the_setting() {
        let guard = preference(reclaim_contention());
        let normal = preference(Input {
            memory_full_bp: 99,
            ..reclaim_contention()
        });
        assert_eq!(normal.value, 80);
        for alternating in [false, true] {
            let mut governor = Governor::new();
            for now in (0..=60).step_by(2) {
                let desired = if (alternating && now % 4 == 2) || now == 2 {
                    guard
                } else {
                    normal
                };
                assert_eq!(governor.observe(desired, Some(80), now).value, 80);
            }
        }
    }

    #[test]
    fn reclaim_guard_and_later_promotion_obey_the_existing_write_interval() {
        let mut governor = Governor::new();
        let normal = hold(80, "file_working_set_refaults");
        let guard = preference(reclaim_contention());
        assert_eq!(governor.observe(normal, Some(60), 0).value, 60);
        assert_eq!(governor.observe(normal, Some(60), 10).value, 80);
        for now in (12..40).step_by(2) {
            assert_eq!(governor.observe(guard, Some(80), now).value, 80);
        }
        assert_eq!(governor.observe(guard, Some(80), 40).value, 60);
        for now in (42..70).step_by(2) {
            assert_eq!(governor.observe(normal, Some(60), now).value, 60);
        }
        assert_eq!(governor.observe(normal, Some(60), 70).value, 80);
    }

    #[test]
    fn confirmed_disk_churn_keeps_priority_over_the_reclaim_ceiling() {
        let desired = preference(Input {
            disk_swap_in: Some(32 * MIB),
            efficient_zram_room: true,
            hint: 20,
            ..reclaim_contention()
        });
        assert!(desired.urgent);
        assert!(!desired.ceiling_only);
        assert_eq!(desired.reason, "disk_swap_read_pressure");
        let mut governor = Governor::new();
        for now in [0, 2, 4] {
            assert_eq!(governor.observe(desired, Some(80), now).value, 80);
        }
        assert_eq!(governor.observe(desired, Some(80), 6).value, 20);
        let guard = preference(reclaim_contention());
        for now in (8..=60).step_by(2) {
            assert_eq!(governor.observe(guard, Some(20), now).value, 20);
        }
    }

    #[test]
    fn file_working_set_can_win_even_when_disk_is_busy_and_zram_is_full() {
        let i = Input {
            rates_available: true,
            file_refault: 44 * MIB,
            anon_refault: 5 * MIB,
            direct_scan: 170 * MIB,
            swap_in: 5 * MIB,
            disk_swap_in: Some(MIB),
            swap_or_io_busy: true,
            hint: 20,
            ..Input::default()
        };
        assert_eq!(preference(i).value, 80);
        assert_eq!(preference(i).reason, "file_working_set_refaults");
        assert_eq!(
            preference(Input {
                efficient_zram_room: true,
                ..i
            })
            .value,
            100
        );
    }

    #[test]
    fn only_measured_disk_swap_churn_requests_a_low_swap_preference() {
        let i = Input {
            rates_available: true,
            file_refault: 96 * MIB,
            anon_refault: 40 * MIB,
            direct_scan: 128 * MIB,
            swap_in: 40 * MIB,
            disk_swap_in: Some(40 * MIB),
            swap_or_io_busy: true,
            efficient_zram_room: true,
            hint: 20,
            ..Input::default()
        };
        assert_eq!(preference(i).value, 20);
        assert!(preference(i).urgent);
        assert_eq!(
            preference(Input {
                file_refault: 4 * MIB,
                efficient_zram_room: false,
                disk_swap_in: None,
                ..i
            })
            .value,
            60
        );
    }

    #[test]
    fn zram_promotion_requires_measured_room_and_warmed_rates() {
        let i = Input {
            efficient_zram_room: true,
            hint: 60,
            ..Input::default()
        };
        assert_eq!(preference(i).value, 60);
        assert_eq!(
            preference(Input {
                rates_available: true,
                ..i
            })
            .value,
            120
        );
        assert_eq!(
            preference(Input {
                rates_available: true,
                efficient_zram_room: false,
                ..i
            })
            .value,
            60
        );
    }

    #[test]
    fn alternating_samples_do_not_flip_the_kernel_setting() {
        let mut g = Governor::new();
        for now in (0..100).step_by(2) {
            let desired = Preference {
                value: if now % 4 == 0 { 120 } else { 20 },
                reason: "test",
                urgent: false,
                ceiling_only: false,
            };
            assert_eq!(g.observe(desired, Some(60), now).value, 60);
        }
    }

    #[test]
    fn sustained_preference_steps_and_confirmed_disk_churn_can_lower_it() {
        let mut g = Governor::new();
        let p = Preference {
            value: 120,
            reason: "test",
            urgent: false,
            ceiling_only: false,
        };
        assert_eq!(g.observe(p, Some(40), 0).value, 40);
        assert_eq!(g.observe(p, Some(40), 10).value, 60);
        assert_eq!(g.observe(p, Some(60), 20).value, 60);
        assert_eq!(g.observe(p, Some(60), 40).value, 80);
        let disk = Preference {
            value: 20,
            urgent: true,
            ..p
        };
        for now in [41, 43, 45] {
            assert_eq!(g.observe(disk, Some(80), now).value, 80);
        }
        assert_eq!(g.observe(disk, Some(80), 47).value, 20);
    }

    #[test]
    fn backward_clock_restarts_hysteresis() {
        let mut g = Governor::new();
        let p = Preference {
            value: 120,
            reason: "test",
            urgent: false,
            ceiling_only: false,
        };
        g.observe(p, Some(40), 100);
        assert_eq!(g.observe(p, Some(40), 50).value, 40);
        assert_eq!(g.observe(p, Some(40), 60).value, 40);
        assert_eq!(g.observe(p, Some(40), 80).value, 60);
    }

    #[test]
    fn unrelated_io_cannot_turn_the_legacy_hint_into_a_cost_claim() {
        for efficient_zram_room in [false, true] {
            let p = preference(Input {
                rates_available: true,
                disk_swap_in: Some(0),
                efficient_zram_room,
                swap_or_io_busy: true,
                hint: 20,
                ..Input::default()
            });
            assert_eq!(p.value, 60);
            assert_eq!(p.reason, "unattributed_io_pressure");
            assert!(!p.urgent);
        }
    }

    #[test]
    fn field_like_replay_reaches_file_target_despite_two_second_disk_bursts() {
        let mut governor = Governor::new();
        let mut actual = 40u16;
        let mut ordinary_writes = Vec::new();
        let mut file_samples = 0;
        let mut disk_samples = 0;
        // 80% of samples have 6.625 MiB/s disk reads; every fifth has
        // 40 MiB/s. The mean is 13.3 MiB/s, near the field trace's mean,
        // with 90.95 MiB/s file refaults and 20.16 MiB/s anonymous refaults.
        for (index, now) in (0..180).step_by(2).enumerate() {
            let disk_rate = if index % 5 == 4 {
                40 * MIB
            } else {
                53 * MIB / 8
            };
            let desired = preference(Input {
                rates_available: true,
                file_refault: 9095 * MIB / 100,
                anon_refault: 2016 * MIB / 100,
                direct_scan: 156 * MIB,
                swap_in: disk_rate + 644 * MIB / 100,
                disk_swap_in: Some(disk_rate),
                swap_or_io_busy: true,
                hint: 20,
                ..Input::default()
            });
            if desired.urgent {
                disk_samples += 1;
            } else {
                assert_eq!(desired.value, 80);
                file_samples += 1;
            }
            let proposal = governor.observe(desired, Some(actual), now);
            assert!(!proposal.urgent, "a two-second spike is not sustained");
            assert!(proposal.value >= actual);
            if proposal.value != actual {
                assert!(proposal.value.abs_diff(actual) <= 20);
                if let Some(previous) = ordinary_writes.last() {
                    assert!(now - previous >= WRITE_INTERVAL_SECS);
                }
                ordinary_writes.push(now);
                actual = proposal.value;
            }
        }
        assert_eq!((file_samples, disk_samples), (72, 18));
        assert_eq!(actual, 80);
        assert_eq!(ordinary_writes, [14, 44]);
    }

    #[test]
    fn failed_writes_never_advance_the_observed_step_baseline() {
        let mut g = Governor::new();
        let desired = hold(120, "efficient_zram_headroom");
        assert_eq!(g.observe(desired, Some(40), 0).value, 40);
        assert_eq!(g.observe(desired, Some(40), 10).value, 60);
        // The first proposal failed. Even after a whole ordinary cooldown,
        // the next proposal must still be only twenty above actual 40.
        assert_eq!(g.observe(desired, Some(40), 40).value, 60);
        assert_eq!(g.observe(desired, Some(60), 42).value, 60);
        assert_eq!(g.observe(desired, Some(60), 68).value, 60);
        assert_eq!(g.observe(desired, Some(60), 70).value, 80);
    }

    #[test]
    fn external_readback_is_the_new_baseline_and_restarts_the_cooldown() {
        let mut g = Governor::new();
        let desired = hold(120, "efficient_zram_headroom");
        g.observe(desired, Some(40), 0);
        assert_eq!(g.observe(desired, Some(40), 10).value, 60);
        // An unrelated controller selected 100, not our requested 60.
        assert_eq!(g.observe(desired, Some(100), 12).value, 100);
        assert_eq!(g.observe(desired, Some(100), 40).value, 100);
        assert_eq!(g.observe(desired, Some(100), 42).value, 120);
    }

    #[test]
    fn sustained_opposing_evidence_retires_the_old_target() {
        let mut g = Governor::new();
        let up = hold(120, "efficient_zram_headroom");
        let down = hold(20, "test_opposing_evidence");
        // Accumulate the maximum history while simulating failed writes.
        for now in (0..=20).step_by(2) {
            g.observe(up, Some(60), now);
        }
        let mut actual = 60u16;
        let mut writes = Vec::new();
        for now in (22..=90).step_by(2) {
            let proposal = g.observe(down, Some(actual), now);
            assert!(proposal.value <= actual, "stale evidence cannot raise it");
            if proposal.value != actual {
                writes.push(now);
                actual = proposal.value;
            }
        }
        assert_eq!(actual, 20);
        assert_eq!(writes, [50, 80]);
    }

    #[test]
    fn neutral_samples_and_nearby_targets_preserve_a_dominant_direction() {
        let mut g = Governor::new();
        let mut actual = 40u16;
        for (index, now) in (0..80).step_by(2).enumerate() {
            let desired = hold(
                if index % 5 == 4 {
                    actual
                } else if index % 2 == 0 {
                    80
                } else {
                    100
                },
                "file_working_set_refaults",
            );
            actual = g.observe(desired, Some(actual), now).value;
        }
        assert!(actual >= 80);
    }

    #[test]
    fn unavailable_readback_does_not_confirm_a_proposal() {
        let mut g = Governor::new();
        let desired = hold(120, "efficient_zram_headroom");
        g.observe(desired, Some(40), 0);
        assert_eq!(g.observe(desired, Some(40), 10).value, 60);
        let unknown = g.observe(desired, None, 40);
        assert_eq!(unknown.value, 40);
        assert_eq!(unknown.reason, "readback_unavailable");
        assert_eq!(g.observe(desired, Some(40), 42).value, 40);
        assert_eq!(g.observe(desired, Some(40), 52).value, 60);
    }

    #[test]
    fn ordinary_recovery_waits_thirty_seconds_after_a_confirmed_urgent_write() {
        let mut g = Governor::new();
        let disk = Preference {
            value: 20,
            reason: "disk_swap_read_pressure",
            urgent: true,
            ceiling_only: false,
        };
        for now in [0, 2, 4] {
            assert_eq!(g.observe(disk, Some(80), now).value, 80);
        }
        assert_eq!(g.observe(disk, Some(80), 6).value, 20);
        let file = hold(80, "file_working_set_refaults");
        for now in (8..36).step_by(2) {
            assert_eq!(g.observe(file, Some(20), now).value, 20);
        }
        assert_eq!(g.observe(file, Some(20), 36).value, 40);
    }
}
