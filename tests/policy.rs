use zmem::{Config, GIB, MIB, Policy, Pressure, Reason, Snapshot, io_congested};

fn healthy() -> Snapshot {
    Snapshot {
        total_bytes: 16 * GIB,
        available_bytes: 8 * GIB,
        swap_total_bytes: 16 * GIB,
        swap_free_bytes: 10 * GIB,
        ..Snapshot::default()
    }
}

#[test]
fn low_memory_with_existing_swap_headroom_does_not_allocate_more_disk() {
    let mut policy = Policy::new(Config::default());
    let mut sample = healthy();
    sample.available_bytes = 512 * MIB;
    let decision = policy.observe(sample, 0);
    assert_eq!(decision.pressure, Pressure::Elevated);
    assert_eq!(decision.grow_swap_bytes, 0);
    assert!(!decision.shrink_allowed);
    assert!(decision.background_high_percent < 100);
}

#[test]
fn zero_swap_grows_before_exhaustion_but_not_on_an_idle_machine() {
    let mut policy = Policy::new(Config::default());
    let mut sample = healthy();
    sample.swap_total_bytes = 0;
    sample.swap_free_bytes = 0;
    assert_eq!(policy.observe(sample, 0).grow_swap_bytes, 0);
    sample.available_bytes = 2 * GIB;
    let decision = policy.observe(sample, 1);
    assert_eq!(decision.grow_swap_bytes, 2 * GIB);
    assert_eq!(decision.pressure, Pressure::Healthy);
    assert!(!decision.shrink_allowed);
}

#[test]
fn growth_uses_full_batches_for_spare_headroom_and_rate_limits_failures() {
    let mut policy = Policy::new(Config::default());
    let mut sample = healthy();
    sample.available_bytes = 512 * MIB;
    sample.swap_free_bytes = GIB;
    assert_eq!(policy.observe(sample, 10).grow_swap_bytes, 2 * GIB);
    assert_eq!(policy.observe(sample, 39).grow_swap_bytes, 0);
    assert_eq!(policy.observe(sample, 40).grow_swap_bytes, 2 * GIB);
    sample.swap_free_bytes = 3 * GIB;
    // A 1 GiB deficit still requests the configured 2 GiB batch, leaving
    // spare headroom instead of using a new swap slot for every small deficit.
    assert_eq!(policy.observe(sample, 70).grow_swap_bytes, 2 * GIB);
    sample.swap_free_bytes = 4 * GIB;
    assert_eq!(policy.observe(sample, 100).grow_swap_bytes, 0);
}

#[test]
fn tiny_swap_deficit_requests_the_configured_batch_without_repeating_the_worker() {
    for chunk_bytes in [Config::default().chunk_bytes, 64 * MIB] {
        let mut policy = Policy::new(Config {
            chunk_bytes,
            ..Config::default()
        });
        let mut sample = healthy();
        sample.available_bytes = 512 * MIB;
        sample.swap_free_bytes = 4 * GIB - 4096;
        assert_eq!(policy.observe(sample, 10).grow_swap_bytes, chunk_bytes);
        assert_eq!(policy.observe(sample, 39).grow_swap_bytes, 0);
        sample.swap_total_bytes += chunk_bytes;
        sample.swap_free_bytes += chunk_bytes;
        assert_eq!(policy.observe(sample, 40).grow_swap_bytes, 0);
    }
}

#[test]
fn recovery_requires_consecutive_good_samples_and_restarts_on_pressure() {
    let mut policy = Policy::new(Config::default());
    let mut bad = healthy();
    bad.available_bytes = 128 * MIB;
    assert_eq!(policy.observe(bad, 0).pressure, Pressure::Critical);
    for second in 1..=3 {
        assert_eq!(
            policy.observe(healthy(), second).pressure,
            Pressure::Recovering
        );
    }
    bad.available_bytes = 512 * MIB;
    assert_eq!(policy.observe(bad, 4).pressure, Pressure::Elevated);
    for second in 5..=8 {
        assert_eq!(
            policy.observe(healthy(), second).pressure,
            Pressure::Recovering
        );
    }
    assert_eq!(policy.observe(healthy(), 9).pressure, Pressure::Healthy);
}

#[test]
fn a_sample_just_above_the_low_memory_boundary_does_not_clear_pressure() {
    let mut policy = Policy::new(Config::default());
    let mut sample = healthy();
    sample.available_bytes = 512 * MIB;
    policy.observe(sample, 0);
    sample.available_bytes = Config::default().effective_reserve_bytes(sample.total_bytes);
    for second in 1..=10 {
        let decision = policy.observe(sample, second);
        assert_eq!(decision.pressure, Pressure::Recovering);
        assert!(!decision.shrink_allowed);
    }
}

#[test]
fn stalled_swap_io_suppresses_extra_reclaim_and_never_shrinks() {
    let mut policy = Policy::new(Config::default());
    let mut sample = healthy();
    sample.available_bytes = 128 * MIB;
    sample.memory_full_bp = 3500;
    sample.io_full_bp = 2500;
    let decision = policy.observe(sample, 0);
    assert_eq!(decision.pressure, Pressure::Critical);
    assert_eq!(decision.reason, Reason::SwapIoPressure);
    assert_eq!(decision.reclaim_bytes, 0);
    assert!(!decision.shrink_allowed);
    sample.io_full_bp = 0;
    sample.swap_in_bytes_per_sec = 64 * MIB;
    assert_eq!(policy.observe(sample, 10).reclaim_bytes, 0);
}

#[test]
fn bounded_background_reclaim_has_its_own_cooldown() {
    let mut policy = Policy::new(Config::default());
    let mut sample = healthy();
    sample.memory_some_bp = 1500;
    let first = policy.observe(sample, 5);
    assert!(first.reclaim_bytes > 0 && first.reclaim_bytes <= 128 * MIB);
    assert_eq!(policy.observe(sample, 14).reclaim_bytes, 0);
    assert!(policy.observe(sample, 15).reclaim_bytes > 0);
}

#[test]
fn shrink_waits_for_long_healthy_period_and_is_revoked_immediately() {
    let mut policy = Policy::new(Config::default());
    assert!(!policy.observe(healthy(), 0).shrink_allowed);
    assert!(!policy.observe(healthy(), 599).shrink_allowed);
    assert!(policy.observe(healthy(), 600).shrink_allowed);
    let mut bad = healthy();
    bad.memory_some_bp = 1200;
    assert!(!policy.observe(bad, 601).shrink_allowed);
    for second in 602..=606 {
        assert!(!policy.observe(healthy(), second).shrink_allowed);
    }
    assert!(!policy.observe(healthy(), 1205).shrink_allowed);
    assert!(policy.observe(healthy(), 1206).shrink_allowed);
}

#[test]
fn clock_regression_cannot_bypass_action_cooldowns_or_allow_shrink() {
    let mut policy = Policy::new(Config::default());
    let mut sample = healthy();
    sample.available_bytes = 512 * MIB;
    sample.swap_free_bytes = 0;
    assert!(policy.observe(sample, 1000).grow_swap_bytes > 0);
    let rewound = policy.observe(sample, 1);
    assert_eq!(rewound.grow_swap_bytes, 0);
    assert_eq!(rewound.reclaim_bytes, 0);
    assert!(!rewound.shrink_allowed);
    assert_eq!(policy.observe(sample, 30).grow_swap_bytes, 0);
    assert!(policy.observe(sample, 31).grow_swap_bytes > 0);
}

#[test]
fn malformed_samples_cannot_trigger_system_changes_or_fake_stable_recovery() {
    let mut policy = Policy::new(Config::default());
    policy.observe(healthy(), 0);
    let invalid = policy.observe(Snapshot::default(), 600);
    assert_eq!(invalid.reason, Reason::InvalidSnapshot);
    assert_eq!(invalid.grow_swap_bytes, 0);
    assert_eq!(invalid.reclaim_bytes, 0);
    assert!(!invalid.shrink_allowed);
    assert!(!policy.observe(healthy(), 601).shrink_allowed);
    let mut impossible = healthy();
    impossible.swap_free_bytes = impossible.swap_total_bytes + 1;
    assert_eq!(
        policy.observe(impossible, 602).reason,
        Reason::InvalidSnapshot
    );
}

#[test]
fn huge_counters_and_thresholds_use_saturating_arithmetic() {
    let config = Config {
        reserve_bytes: u64::MAX,
        grow_free_bytes: u64::MAX,
        chunk_bytes: u64::MAX,
        max_disk_swap_bytes: 7 * GIB,
        grow_cooldown_secs: u64::MAX,
        shrink_cooldown_secs: u64::MAX,
        ..Config::default()
    };
    let mut policy = Policy::new(config);
    let sample = Snapshot {
        total_bytes: u64::MAX,
        available_bytes: u64::MAX / 2,
        swap_total_bytes: u64::MAX,
        swap_free_bytes: 0,
        memory_some_bp: u16::MAX,
        memory_full_bp: u16::MAX,
        ..Snapshot::default()
    };
    let first = policy.observe(sample, 0);
    assert_eq!(first.grow_swap_bytes, 7 * GIB);
    assert_eq!(first.pressure, Pressure::Critical);
    assert!(!first.shrink_allowed);
    assert_eq!(policy.observe(sample, u64::MAX - 1).grow_swap_bytes, 0);
    assert_eq!(policy.observe(sample, u64::MAX).grow_swap_bytes, 7 * GIB);
}

#[test]
fn explicit_zero_growth_limits_disable_allocation_without_disabling_protection() {
    for config in [
        Config {
            chunk_bytes: 0,
            ..Config::default()
        },
        Config {
            max_disk_swap_bytes: 0,
            ..Config::default()
        },
    ] {
        let mut policy = Policy::new(config);
        let mut sample = healthy();
        sample.available_bytes = 128 * MIB;
        sample.swap_free_bytes = 0;
        let decision = policy.observe(sample, 0);
        assert_eq!(decision.pressure, Pressure::Critical);
        assert_eq!(decision.grow_swap_bytes, 0);
        assert_eq!(decision.background_high_percent, 50);
    }
}

#[test]
fn kernel_memory_stalls_trigger_protection_even_when_memavailable_is_high() {
    let mut policy = Policy::new(Config::default());
    let mut sample = healthy();
    sample.memory_full_bp = 3000;
    let decision = policy.observe(sample, 0);
    assert_eq!(decision.pressure, Pressure::Critical);
    assert_eq!(decision.reason, Reason::MemoryStall);
    assert_eq!(decision.grow_swap_bytes, 0);
    assert!(!decision.shrink_allowed);
}

#[test]
fn observed_io_only_stall_is_visible_without_creating_memory_pressure() {
    let mut policy = Policy::new(Config::default());
    let sample = Snapshot {
        total_bytes: 16_476_700_672,
        available_bytes: 3_140_000_000,
        swap_total_bytes: 32 * GIB,
        swap_free_bytes: 8 * GIB,
        memory_some_bp: 0,
        memory_full_bp: 0,
        io_full_bp: 1026,
        swap_in_bytes_per_sec: 5_200_000,
        swap_out_bytes_per_sec: 0,
    };
    for second in [0, 10, 60, 600] {
        let decision = policy.observe(sample, second);
        assert!(decision.io_congested);
        assert_eq!(decision.pressure, Pressure::Healthy);
        assert_eq!(decision.reason, Reason::SwapIoPressure);
        assert_eq!(decision.background_high_percent, 100);
        assert_eq!(decision.reclaim_bytes, 0);
        assert_eq!(decision.grow_swap_bytes, 0);
        assert!(!decision.shrink_allowed);
    }
}

#[test]
fn io_congestion_defers_preventive_swap_allocation_but_not_a_real_shortage() {
    let mut policy = Policy::new(Config::default());
    let mut sample = healthy();
    sample.available_bytes = 2 * GIB;
    sample.swap_free_bytes = GIB;
    sample.io_full_bp = 1000;
    let deferred = policy.observe(sample, 0);
    assert_eq!(deferred.pressure, Pressure::Healthy);
    assert!(deferred.io_congested);
    assert_eq!(deferred.grow_swap_bytes, 0);

    // A skipped preventive allocation must not consume the growth cooldown.
    sample.available_bytes = 512 * MIB;
    let urgent = policy.observe(sample, 1);
    assert_eq!(urgent.pressure, Pressure::Elevated);
    assert_eq!(urgent.grow_swap_bytes, 2 * GIB);
    assert_eq!(urgent.reclaim_bytes, 0);
}

#[test]
fn preventive_swap_growth_resumes_after_io_recovers() {
    let mut policy = Policy::new(Config::default());
    let mut sample = healthy();
    sample.available_bytes = 2 * GIB;
    sample.swap_free_bytes = GIB;
    sample.io_full_bp = 1000;
    assert_eq!(policy.observe(sample, 20).grow_swap_bytes, 0);
    sample.io_full_bp = 499;
    let resumed = policy.observe(sample, 21);
    assert!(!resumed.io_congested);
    assert_eq!(resumed.grow_swap_bytes, 2 * GIB);
}

#[test]
fn reserve_boundary_keeps_disk_work_optional_until_memory_is_below_reserve() {
    let config = Config::default();
    let mut sample = healthy();
    sample.swap_free_bytes = 0;
    sample.io_full_bp = 1000;
    sample.available_bytes = config.effective_reserve_bytes(sample.total_bytes);
    let mut policy = Policy::new(config);
    assert_eq!(policy.observe(sample, 0).grow_swap_bytes, 0);
    sample.available_bytes -= 1;
    assert!(policy.observe(sample, 1).grow_swap_bytes > 0);
}

#[test]
fn io_signal_uses_a_five_percent_gate_and_preserves_swap_rate_boundaries() {
    let mut sample = healthy();
    sample.io_full_bp = 499;
    assert!(!io_congested(sample));
    sample.io_full_bp = 500;
    assert!(io_congested(sample));
    sample.io_full_bp = 0;
    sample.swap_in_bytes_per_sec = 64 * MIB - 1;
    assert!(!io_congested(sample));
    sample.swap_in_bytes_per_sec += 1;
    assert!(io_congested(sample));
    sample.swap_in_bytes_per_sec = 16 * MIB;
    sample.swap_out_bytes_per_sec = 64 * MIB;
    assert!(io_congested(sample));
    sample.swap_in_bytes_per_sec -= 1;
    assert!(!io_congested(sample));
}

#[test]
fn the_lower_io_gate_does_not_lower_the_existing_swappiness_hint() {
    let mut policy = Policy::new(Config::default());
    let mut sample = healthy();
    sample.io_full_bp = 506;
    let decision = policy.observe(sample, 0);
    assert!(decision.io_congested);
    assert_eq!(decision.pressure, Pressure::Healthy);
    assert_eq!(decision.background_high_percent, 100);
    assert_eq!(decision.recommended_swappiness, 60);
    assert_eq!(decision.reclaim_bytes, 0);
}
