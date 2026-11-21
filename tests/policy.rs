use zmem::{Config, GIB, MIB, Policy, Pressure, Reason, Snapshot};

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
fn growth_failure_is_rate_limited_and_remaining_headroom_is_bounded() {
    let mut policy = Policy::new(Config::default());
    let mut sample = healthy();
    sample.available_bytes = 512 * MIB;
    sample.swap_free_bytes = GIB;
    assert_eq!(policy.observe(sample, 10).grow_swap_bytes, 2 * GIB);
    assert_eq!(policy.observe(sample, 39).grow_swap_bytes, 0);
    assert_eq!(policy.observe(sample, 40).grow_swap_bytes, 2 * GIB);
    sample.swap_free_bytes = 3 * GIB;
    assert_eq!(policy.observe(sample, 70).grow_swap_bytes, GIB);
    sample.swap_free_bytes = 4 * GIB;
    assert_eq!(policy.observe(sample, 100).grow_swap_bytes, 0);
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
