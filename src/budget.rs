//! Demand-aware limits for explicitly managed background cgroups.
//!
//! This module only calculates limits. The Linux adapter owns cgroup discovery,
//! external-limit protection, page rounding, and kernel writes.

const TOTAL_PROGRESS_BYTES: u64 = 256 * 1024 * 1024;
const GROUP_PROGRESS_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GroupUsage {
    pub current_bytes: u64,
    pub populated: bool,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct BudgetPlan {
    /// None clears the temporary aggregate budget and its pressure episode.
    pub total_bytes: Option<u64>,
    /// One entry per input group. None requests no temporary high restriction;
    /// the caller must still preserve finite external limits.
    pub limits: Vec<Option<u64>>,
}

/// Allocate a pressure budget using occupied memory and spare host headroom.
///
/// A healthy cap (100 or greater), an invalid zero physical-memory size, or no
/// populated groups produces no temporary limits. Otherwise, the aggregate is
/// current managed usage plus signed headroom above reserve + reserve / 4.
/// The cap percentage distinguishes healthy policy from a pressure episode;
/// measured capacity, rather than that fixed percentage, bounds the byte budget.
/// The reserve limits the aggregate, subject to a shared progress floor of at
/// most 256 MiB and one quarter of physical RAM (at least one byte). Each active
/// group receives a base of at most
/// 64 MiB, then a share proportional to current usage; all-zero usage shares the
/// remainder equally. Empty groups consume neither usage nor quota.
///
/// Pass the preceding result's total_bytes back as previous_budget. A pressure
/// episode tightens its budget immediately. Positive measured spare headroom
/// permits an increase of at most min(256 MiB, spare / 4) per sample. This gives
/// back capacity even when the controller's own memory.high throttling keeps PSI
/// elevated. Without spare headroom, the preceding budget cannot increase.
/// A preceding budget outside the current physical bounds is clamped to those
/// bounds. Changing group membership redistributes the resulting aggregate.
/// Limits sum exactly to total_bytes before the adapter rounds them to pages.
/// max_increase_bytes is a caller-supplied time allowance, so faster sampling
/// cannot accelerate recovery beyond the configured observation cadence.
pub(crate) fn plan_background(
    total: u64,
    available: u64,
    reserve: u64,
    cap_percent: u8,
    groups: &[GroupUsage],
    previous_budget: Option<u64>,
    max_increase_bytes: u64,
) -> BudgetPlan {
    let active = groups.iter().filter(|group| group.populated).count() as u64;
    if total == 0 || cap_percent >= 100 || active == 0 {
        return BudgetPlan {
            total_bytes: None,
            limits: vec![None; groups.len()],
        };
    }

    let available = available.min(total);
    let reserve = reserve.min(total);
    let comfortable = reserve.saturating_add(reserve / 4).min(total);
    let used: u128 = groups
        .iter()
        .filter(|group| group.populated)
        .map(|group| u128::from(group.current_bytes))
        .sum();
    let candidate = if available >= comfortable {
        used.saturating_add(u128::from(available - comfortable))
    } else {
        used.saturating_sub(u128::from(comfortable - available))
    };
    let floor = TOTAL_PROGRESS_BYTES.min((total / 4).max(1));
    let ceiling = (total - reserve).max(floor);
    let mut budget = candidate.clamp(u128::from(floor), u128::from(ceiling)) as u64;
    if let Some(previous) = previous_budget {
        let spare = available.saturating_sub(comfortable);
        let increase = TOTAL_PROGRESS_BYTES.min(spare / 4).min(max_increase_bytes);
        budget = budget.min(previous.clamp(floor, ceiling).saturating_add(increase));
    }

    let base = GROUP_PROGRESS_BYTES.min(budget / active);
    let remainder = budget - base * active;
    let mut cumulative_weight = 0u128;
    let mut cumulative_share = 0u64;
    let weight_total = if used == 0 { u128::from(active) } else { used };
    let limits = groups
        .iter()
        .map(|group| {
            if !group.populated {
                return None;
            }
            cumulative_weight += if used == 0 {
                1
            } else {
                u128::from(group.current_bytes)
            };
            let share = proportional_share(remainder, cumulative_weight, weight_total);
            let limit = base + (share - cumulative_share);
            cumulative_share = share;
            Some(limit)
        })
        .collect();
    BudgetPlan {
        total_bytes: Some(budget),
        limits,
    }
}

/// floor(pool * numerator / denominator), with numerator <= denominator.
fn proportional_share(pool: u64, numerator: u128, denominator: u128) -> u64 {
    if numerator == denominator {
        return pool;
    }
    if let Some(product) = u128::from(pool).checked_mul(numerator) {
        return (product / denominator) as u64;
    }
    // Normal physical-memory measurements fit the fast path. Keep malformed or
    // saturated counters safe too, without truncating their relative weights.
    let mut quotient = 0u128;
    let mut remainder = 0u128;
    for bit in (0..64).rev() {
        quotient *= 2;
        if remainder >= denominator - remainder {
            remainder -= denominator - remainder;
            quotient += 1;
        } else {
            remainder *= 2;
        }
        if pool & (1u64 << bit) != 0 {
            if remainder >= denominator - numerator {
                remainder -= denominator - numerator;
                quotient += 1;
            } else {
                remainder += numerator;
            }
        }
    }
    quotient as u64
}

/// Preserve an aggregate when finite external caps prevent some groups from
/// using their initial share. Limits, caps, and total must use the same unit;
/// the adapter uses pages so rounding cannot lose quota on every sample.
pub(crate) fn cap_and_redistribute(
    plan: &mut BudgetPlan,
    groups: &[GroupUsage],
    caps: &[Option<u64>],
) {
    let Some(total) = plan.total_bytes else {
        return;
    };
    assert_eq!(plan.limits.len(), groups.len());
    assert_eq!(caps.len(), groups.len());
    for (limit, cap) in plan.limits.iter_mut().zip(caps) {
        if let (Some(limit), Some(cap)) = (limit, cap) {
            *limit = (*limit).min(*cap);
        }
    }
    let assigned: u64 = plan.limits.iter().flatten().copied().sum();
    let mut remaining = total.saturating_sub(assigned);
    for _ in 0..=groups.len() {
        if remaining == 0 {
            break;
        }
        let eligible: Vec<_> = groups
            .iter()
            .enumerate()
            .filter_map(|(index, group)| {
                let limit = plan.limits[index]?;
                (group.populated && caps[index].is_none_or(|cap| limit < cap)).then_some(index)
            })
            .collect();
        if eligible.is_empty() {
            break;
        }
        let used: u128 = eligible
            .iter()
            .map(|&index| u128::from(groups[index].current_bytes))
            .sum();
        let weight_total = if used == 0 {
            eligible.len() as u128
        } else {
            used
        };
        let mut cumulative_weight = 0u128;
        let mut cumulative_share = 0u64;
        let mut assigned = 0u64;
        for index in eligible {
            cumulative_weight += if used == 0 {
                1
            } else {
                u128::from(groups[index].current_bytes)
            };
            let share = proportional_share(remaining, cumulative_weight, weight_total);
            let limit = plan.limits[index]
                .as_mut()
                .expect("eligible populated group");
            let grant =
                (share - cumulative_share).min(caps[index].map_or(u64::MAX, |cap| cap - *limit));
            *limit += grant;
            assigned += grant;
            cumulative_share = share;
        }
        if assigned == 0 {
            break;
        }
        remaining -= assigned;
    }
    plan.total_bytes = Some(total - remaining);
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;

    fn plan_background(
        total: u64,
        available: u64,
        reserve: u64,
        cap_percent: u8,
        groups: &[GroupUsage],
        previous_budget: Option<u64>,
    ) -> BudgetPlan {
        super::plan_background(
            total,
            available,
            reserve,
            cap_percent,
            groups,
            previous_budget,
            TOTAL_PROGRESS_BYTES,
        )
    }

    #[test]
    fn exhausted_time_allowance_holds_growth_but_still_allows_tightening() {
        let groups = [group(4 * GIB)];
        let held = super::plan_background(16 * GIB, 8 * GIB, 2 * GIB, 50, &groups, Some(GIB), 0);
        assert_eq!(held.total_bytes, Some(GIB));
        let tighter = super::plan_background(16 * GIB, 0, 2 * GIB, 50, &groups, Some(6 * GIB), 0);
        assert_eq!(tighter.total_bytes, Some(GIB + 512 * MIB));
    }

    #[test]
    fn finite_external_cap_does_not_consume_an_unusable_share_each_cycle() {
        let groups = [group(8 * MIB), group(GIB)];
        let caps = [Some(16 * MIB), None];
        let mut previous = 1040 * MIB;
        for cycle in 0..32 {
            let allowance = if cycle == 0 { 32 * MIB } else { 0 };
            let mut plan = super::plan_background(
                16 * GIB,
                2 * GIB + 512 * MIB + 128 * MIB,
                2 * GIB,
                50,
                &groups,
                Some(previous),
                allowance,
            );
            cap_and_redistribute(&mut plan, &groups, &caps);
            assert_eq!(plan.total_bytes, Some(1072 * MIB));
            assert_eq!(plan.limits, [Some(16 * MIB), Some(1056 * MIB)]);
            assert_conserved(&plan, &groups);
            previous = plan.total_bytes.unwrap();
        }
    }

    #[test]
    fn capped_redistribution_stops_at_total_capacity_and_conserves_page_units() {
        let groups = [group(MIB), group(2 * MIB), group(0)];
        let mut plan = BudgetPlan {
            total_bytes: Some(100),
            limits: vec![Some(30), Some(60), Some(10)],
        };
        cap_and_redistribute(&mut plan, &groups, &[Some(2), Some(20), Some(3)]);
        assert_eq!(plan.total_bytes, Some(25));
        assert_eq!(plan.limits, [Some(2), Some(20), Some(3)]);
        let mut pages = BudgetPlan {
            total_bytes: Some(101),
            limits: vec![Some(32), Some(65), Some(1)],
        };
        cap_and_redistribute(&mut pages, &groups, &[None; 3]);
        assert_eq!(pages.limits.iter().flatten().sum::<u64>(), 101);
        assert_eq!(pages.total_bytes, Some(101));
    }

    fn group(current_bytes: u64) -> GroupUsage {
        GroupUsage {
            current_bytes,
            populated: true,
        }
    }

    fn assert_conserved(plan: &BudgetPlan, groups: &[GroupUsage]) {
        assert_eq!(plan.limits.len(), groups.len());
        assert_eq!(
            plan.limits
                .iter()
                .flatten()
                .map(|v| u128::from(*v))
                .sum::<u128>(),
            u128::from(plan.total_bytes.unwrap_or(0))
        );
        for (limit, usage) in plan.limits.iter().zip(groups) {
            if !usage.populated || plan.total_bytes.is_none() {
                assert!(limit.is_none());
            } else {
                assert!(limit.is_some());
            }
        }
    }

    #[test]
    fn nearly_empty_managed_work_does_not_receive_most_of_physical_ram() {
        let groups = [group(40 * 1024)];
        let plan = plan_background(16 * GIB, GIB, 2 * GIB, 70, &groups, None);
        assert_eq!(plan.total_bytes, Some(256 * MIB));
        assert_eq!(plan.limits, vec![Some(256 * MIB)]);
    }

    #[test]
    fn headroom_deficit_tightens_and_healthy_policy_clears_the_budget() {
        let groups = [group(8 * GIB)];
        let pressured = plan_background(16 * GIB, 512 * MIB, 2 * GIB, 70, &groups, None);
        assert_eq!(pressured.total_bytes, Some(6 * GIB));
        let recovering = plan_background(
            16 * GIB,
            8 * GIB,
            2 * GIB,
            85,
            &groups,
            pressured.total_bytes,
        );
        assert_eq!(recovering.total_bytes, Some(6 * GIB + 256 * MIB));
        let healthy = plan_background(
            16 * GIB,
            8 * GIB,
            2 * GIB,
            100,
            &groups,
            recovering.total_bytes,
        );
        assert_eq!(healthy.total_bytes, None);
        assert_eq!(healthy.limits, vec![None]);
        let next_episode =
            plan_background(16 * GIB, 8 * GIB, 2 * GIB, 85, &groups, healthy.total_bytes);
        assert!(next_episode.total_bytes.unwrap() > 6 * GIB);
    }

    #[test]
    fn ongoing_pressure_can_recover_capacity_from_measured_spare_headroom() {
        let total = 16 * GIB;
        let reserve = 2 * GIB;
        let comfortable = reserve + reserve / 4;
        let mut previous = 256 * MIB;
        for _ in 0..64 {
            // The workload grows into its granted capacity while unrelated
            // allocations remain bounded. Keep the pressure hint at 50: its
            // own high-limit stalls must not trap it at the old small budget.
            let current = previous - 64 * MIB;
            let available = total - current - 512 * MIB;
            let groups = [group(current)];
            let plan = plan_background(total, available, reserve, 50, &groups, Some(previous));
            let budget = plan.total_bytes.unwrap();
            let allowed = (256 * MIB).min(available.saturating_sub(comfortable) / 4);
            assert!(budget >= previous);
            assert!(budget - previous <= allowed);
            assert!(budget <= total - reserve);
            assert_conserved(&plan, &groups);
            previous = budget;
        }
        assert!(
            previous > total / 2,
            "the legacy pressure percentage is not a byte ceiling"
        );
    }

    #[test]
    fn no_spare_headroom_cannot_relax_a_pressure_budget() {
        let groups = [group(4 * GIB)];
        let comfortable = 2 * GIB + 512 * MIB;
        let held = plan_background(16 * GIB, comfortable, 2 * GIB, 50, &groups, Some(GIB));
        assert_eq!(held.total_bytes, Some(GIB));
        let growing = plan_background(
            16 * GIB,
            comfortable + 128 * MIB,
            2 * GIB,
            50,
            &groups,
            Some(GIB),
        );
        assert_eq!(growing.total_bytes, Some(GIB + 32 * MIB));
        let tightening = plan_background(16 * GIB, 512 * MIB, 2 * GIB, 50, &groups, Some(6 * GIB));
        assert_eq!(tightening.total_bytes, Some(2 * GIB));
    }

    #[test]
    fn pressure_hints_do_not_impose_fixed_ram_percentage_ceilings() {
        let groups = [group(12 * GIB)];
        for hint in [0, 50, 70, 85, 99] {
            let plan = plan_background(16 * GIB, 2 * GIB, GIB, hint, &groups, None);
            assert_eq!(plan.total_bytes, Some(12 * GIB + 768 * MIB));
            assert_conserved(&plan, &groups);
        }
    }

    #[test]
    fn larger_workloads_receive_more_and_empty_groups_receive_no_share() {
        let groups = [
            group(8 * GIB),
            group(GIB),
            GroupUsage {
                current_bytes: u64::MAX,
                populated: false,
            },
        ];
        let plan = plan_background(16 * GIB, GIB, 2 * GIB, 70, &groups, None);
        assert_eq!(plan.total_bytes, Some(7 * GIB + 512 * MIB));
        assert!(plan.limits[0].unwrap() > 6 * GIB);
        assert!(plan.limits[1].unwrap() < GIB);
        assert_eq!(plan.limits[2], None);
        assert_conserved(&plan, &groups);
    }

    #[test]
    fn newly_populated_zero_usage_group_receives_startup_room() {
        let groups = [group(4 * GIB), group(0)];
        let plan = plan_background(16 * GIB, 2 * GIB, 2 * GIB, 70, &groups, None);
        assert_eq!(plan.total_bytes, Some(3 * GIB + 512 * MIB));
        assert_eq!(plan.limits[1], Some(64 * MIB));
        assert_conserved(&plan, &groups);
        let groups = [group(0), group(0), group(0)];
        let plan = plan_background(16 * GIB, GIB, 2 * GIB, 70, &groups, None);
        let smallest = plan.limits.iter().flatten().min().unwrap();
        let largest = plan.limits.iter().flatten().max().unwrap();
        assert!(*largest - *smallest <= 1);
        assert_conserved(&plan, &groups);
    }

    #[test]
    fn comfortable_headroom_boundary_uses_signed_bytes_without_a_discontinuity() {
        let groups = [group(4 * GIB)];
        let comfortable = 2 * GIB + 512 * MIB;
        for available in [comfortable - 1, comfortable, comfortable + 1] {
            let plan = plan_background(16 * GIB, available, 2 * GIB, 70, &groups, None);
            assert_eq!(plan.total_bytes, Some(4 * GIB + available - comfortable));
        }
    }

    #[test]
    fn many_groups_share_one_floor_even_on_tiny_machines() {
        let groups = vec![group(0); 1024];
        let plan = plan_background(1000, 0, 900, 50, &groups, None);
        assert_eq!(plan.total_bytes, Some(250));
        assert!(plan.limits.iter().flatten().all(|limit| *limit <= 1));
        assert_conserved(&plan, &groups);
        let tight = plan_background(
            96 * MIB,
            0,
            48 * MIB,
            50,
            &[group(64 * MIB), group(16 * MIB)],
            None,
        );
        assert_eq!(tight.total_bytes, Some(24 * MIB));
        let ceiling = plan_background(
            96 * MIB,
            48 * MIB,
            48 * MIB,
            50,
            &[group(64 * MIB), group(16 * MIB)],
            None,
        );
        assert_eq!(ceiling.total_bytes, Some(48 * MIB));
    }

    #[test]
    fn empty_population_clears_a_previous_pressure_episode() {
        let groups = [GroupUsage {
            current_bytes: GIB,
            populated: false,
        }];
        let plan = plan_background(16 * GIB, GIB, 2 * GIB, 50, &groups, Some(GIB));
        assert_eq!(plan.total_bytes, None);
        assert_eq!(plan.limits, vec![None]);
        assert_eq!(
            plan_background(16 * GIB, 0, GIB, 50, &[], Some(GIB)).total_bytes,
            None
        );
        assert_eq!(
            plan_background(0, 0, 0, 50, &[group(1)], Some(GIB)).total_bytes,
            None
        );
    }

    #[test]
    fn saturated_counters_keep_exact_proportions_without_overflow() {
        let groups = [group(u64::MAX), group(u64::MAX), group(u64::MAX), group(0)];
        let plan = plan_background(u64::MAX, u64::MAX, 0, 85, &groups, None);
        assert_eq!(plan.total_bytes, Some(u64::MAX));
        assert_eq!(plan.limits[3], Some(64 * MIB));
        let first_three: Vec<_> = plan.limits[..3].iter().flatten().copied().collect();
        assert!(first_three.iter().max().unwrap() - first_three.iter().min().unwrap() <= 1);
        assert_conserved(&plan, &groups);
        assert_eq!(
            proportional_share(u64::MAX, u128::from(u64::MAX) * 2, u128::from(u64::MAX) * 3),
            (u128::from(u64::MAX) * 2 / 3) as u64
        );
        let reserved = plan_background(u64::MAX, u64::MAX, u64::MAX, 50, &groups, Some(u64::MAX));
        assert_eq!(reserved.total_bytes, Some(256 * MIB));
        assert_conserved(&reserved, &groups);
    }

    #[test]
    fn plans_conserve_capacity_across_group_counts_and_memory_scales() {
        for total in [1, 4096, MIB, 256 * MIB, 16 * GIB, u64::MAX] {
            for count in 1..=12 {
                let groups: Vec<_> = (0..count)
                    .map(|index| GroupUsage {
                        current_bytes: if index % 3 == 0 {
                            u64::MAX
                        } else {
                            total / count
                        },
                        populated: index % 4 != 0,
                    })
                    .collect();
                for cap in [0, 50, 70, 85, 99, 100] {
                    let plan = plan_background(total, total / 8, total / 10, cap, &groups, None);
                    assert_conserved(&plan, &groups);
                    if let Some(budget) = plan.total_bytes {
                        let reserve = total / 10;
                        let floor = (256 * MIB).min((total / 4).max(1));
                        let ceiling = (total - reserve).max(floor);
                        assert!(budget <= ceiling);
                        assert!(budget <= total);
                        let held = plan_background(total, 0, reserve, cap, &groups, Some(budget));
                        assert!(held.total_bytes.unwrap() <= budget);
                        assert_conserved(&held, &groups);
                        let relaxed =
                            plan_background(total, total, reserve, cap, &groups, Some(budget));
                        let comfortable = reserve.saturating_add(reserve / 4).min(total);
                        let allowed = (256 * MIB).min(total.saturating_sub(comfortable) / 4);
                        assert!(relaxed.total_bytes.unwrap() <= budget.saturating_add(allowed));
                        assert!(relaxed.total_bytes.unwrap() <= ceiling);
                        assert_conserved(&relaxed, &groups);
                    }
                }
            }
        }
    }
}
