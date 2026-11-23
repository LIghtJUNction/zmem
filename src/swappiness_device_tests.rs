use super::*;

fn compressed(name: &str, priority: i32, used: u64, resident: u64) -> (Swap, Zram) {
    (
        Swap {
            path: format!("/dev/{name}"),
            kind: "partition".into(),
            size: 10 * GIB,
            used,
            priority,
        },
        Zram {
            name: name.into(),
            original: used,
            compressed: resident,
            resident,
            same_pages: 0,
            logical_size: 10 * GIB,
            logical_used: used,
            memory_limit: 0,
            algorithm: "zstd".into(),
        },
    )
}

fn disk(priority: i32, used: u64) -> Swap {
    Swap {
        path: "/swap/background.swap".into(),
        kind: "file".into(),
        size: 8 * GIB,
        used,
        priority,
    }
}

#[test]
fn single_preferred_zram_keeps_its_measured_headroom() {
    let (swap, zram) = compressed("zram0", 100, 5 * GIB, 2 * GIB);
    assert!(efficient_zram_room(
        16 * GIB,
        &[swap.clone(), disk(10, 4 * GIB)],
        std::slice::from_ref(&zram),
    ));
    assert!(!efficient_zram_room(16 * GIB, &[], &[]));
    assert!(!efficient_zram_room(16 * GIB, &[swap], &[]));
}

#[test]
fn writable_disk_at_or_above_zram_priority_blocks_promotion() {
    for priority in [100, 200] {
        let (swap, zram) = compressed("zram0", 100, 5 * GIB, 2 * GIB);
        assert!(!efficient_zram_room(
            16 * GIB,
            &[swap, disk(priority, 4 * GIB)],
            &[zram],
        ));
    }
}

#[test]
fn full_higher_priority_disk_does_not_hide_available_zram() {
    let (swap, zram) = compressed("zram0", 100, 5 * GIB, 2 * GIB);
    assert!(efficient_zram_room(
        16 * GIB,
        &[disk(200, 8 * GIB), swap],
        &[zram],
    ));
}

#[test]
fn full_zram_tiers_are_skipped_but_nearly_full_tiers_are_not() {
    let (lower, lower_data) = compressed("zram1", 100, 5 * GIB, 2 * GIB);
    for (used, expected) in [(10 * GIB, true), (9 * GIB, false)] {
        let (higher, higher_data) = compressed("zram0", 200, used, 4 * GIB);
        assert_eq!(
            efficient_zram_room(
                16 * GIB,
                &[higher, lower.clone()],
                &[higher_data, lower_data.clone()],
            ),
            expected,
        );
    }
}

#[test]
fn missing_full_or_lower_priority_zram_observations_block_promotion() {
    let (preferred, preferred_data) = compressed("zram0", 100, 5 * GIB, 2 * GIB);
    for (priority, used) in [(10, 5 * GIB), (200, 10 * GIB)] {
        let (other, mut other_data) = compressed("zram1", priority, used, 2 * GIB);
        let swaps = [preferred.clone(), other];
        assert!(!efficient_zram_room(
            16 * GIB,
            &swaps,
            std::slice::from_ref(&preferred_data),
        ));
        other_data.name = "zram2".into();
        assert!(!efficient_zram_room(
            16 * GIB,
            &swaps,
            &[preferred_data.clone(), other_data],
        ));
    }
}

#[test]
fn all_active_devices_contribute_to_the_physical_memory_ceiling() {
    let (preferred, preferred_data) = compressed("zram0", 100, 5 * GIB, 2 * GIB);
    for (resident, expected) in [(5 * GIB, true), (6 * GIB, false)] {
        let (full, full_data) = compressed("zram1", 10, 10 * GIB, resident);
        assert_eq!(
            efficient_zram_room(
                16 * GIB,
                &[preferred.clone(), full],
                &[preferred_data.clone(), full_data],
            ),
            expected,
        );
    }
}

#[test]
fn every_writable_device_in_a_shared_tier_needs_measured_savings() {
    let (first, first_data) = compressed("zram0", 100, 5 * GIB, 2 * GIB);
    for (resident, expected) in [(2 * GIB, true), (4 * GIB, false)] {
        let (second, second_data) = compressed("zram1", 100, 5 * GIB, resident);
        assert_eq!(
            efficient_zram_room(
                16 * GIB,
                &[first.clone(), second],
                &[first_data.clone(), second_data],
            ),
            expected,
        );
    }
}

#[test]
fn device_memory_limits_and_logical_headroom_remain_strict() {
    let (swap, mut zram) = compressed("zram0", 100, 5 * GIB, 900 * MIB);
    zram.memory_limit = 1000 * MIB;
    assert!(!efficient_zram_room(
        16 * GIB,
        std::slice::from_ref(&swap),
        std::slice::from_ref(&zram),
    ));
    zram.resident -= MIB;
    assert!(efficient_zram_room(16 * GIB, &[swap], &[zram]));
    for (used, expected) in [(9 * GIB, false), (9 * GIB - 4096, true)] {
        let (swap, zram) = compressed("zram0", 100, used, 2 * GIB);
        assert_eq!(efficient_zram_room(16 * GIB, &[swap], &[zram]), expected);
    }
}
