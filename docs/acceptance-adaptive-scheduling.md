# Adaptive background scheduling acceptance

## Historical candidate and provenance

This is the parallel scheduling branch's acceptance record, preserved from [commit `9385e7e91f688633c1234c144919f47a0b953845`](https://github.com/LIghtJUNction/zmem/commit/9385e7e91f688633c1234c144919f47a0b953845). Its measurements and **56 passing ordinary tests** belong to the earlier package-0.1.2 candidate described below. The integrated 0.2.0 candidate has separate validation; these historical measurements do not validate its added swappiness governor, reclaim feedback, confirmed-budget transactions or capped-budget redistribution.

The original report follows. Its recorded numbers, environment and evidence limits are retained. See the [current README](../README.md) for the integrated control and status contracts.

Validated on 22 November 2026 (Asia/Taipei), using Linux 7.3.5-arch1-1, systemd 262.2 and Rust 1.100.0 on the development host. The candidate builds on `a65c48ab75b26493a0e613986ac0e3a896c2f53c`; its package version remains 0.1.2 pending a release. This report accompanies the source changes it validates.

## Problem and scope

The previous adapter divided a fixed percentage of physical RAM equally between discovered background directories. It did not account for occupied memory or distinguish empty groups from active work. Ordinary background launches also received a permanent per-job `MemoryHigh` of half RAM. That leaf restriction remained even when the parent recovered to `max`.

The candidate calculates a shared byte budget from current managed usage and available host memory, distributes it among populated groups, and enables accounting without adding that permanent leaf restriction. It retains the configured reserve, finite external limits, control ownership checks and page-size normalization. Terminal discovery now follows nested slices while sharing the existing app protection pool.

The policy retains its strictest severity hint until stable recovery. Byte budgets can nevertheless grow gradually when measured spare memory returns. This distinction matters because [`memory.high`](https://docs.kernel.org/admin-guide/cgroup-v2.html#memory-interface-files) throttles allocation through reclaim; that work can itself contribute pressure. Waiting for pressure to disappear before returning any capacity can prolong the restriction.

The status additions expose the boundary of this control: proposed aggregate/per-group budgets, actual sampled kernel limits, managed resident/Swap usage and optional process membership. A proposed limit is not a receipt for a successful kernel write. Existing allocations remain charged to their original cgroup when a process moves, as described in the kernel's [memory ownership documentation](https://docs.kernel.org/admin-guide/cgroup-v2.html#memory-ownership).

## Deterministic validation

| Check | Result |
| --- | --- |
| `cargo test --locked --offline` | 56 passed: 40 binary tests and 16 policy tests. Three kernel fixtures are excluded from the ordinary run. |
| Budget regressions | Twelve tests cover active/empty groups, startup shares, usage weighting, signed headroom boundaries, progress floors, reserve ceilings, conservation, saturated counters and bounded recovery while pressure remains elevated. |
| Recovery regressions | Cover interrupted recovery, invalid samples, clock regression and the configured number of consecutive comfortable samples. |
| Nested protection and sampling | Cover terminal ancestors, shared protection, bounded traversal, optional Swap accounting and invalid population measurements. |
| Strict lint | `cargo clippy --locked --offline --all-targets -- -D warnings` passed. |
| Portable policy | `cargo check --locked --offline --lib --no-default-features` passed. |
| Formatting and optimized build | `cargo fmt` and `cargo build --locked --offline --release` passed. |

The Cargo batch held the shared Project Z owner lock throughout. The optimized executable and test executable were copied out before releasing it. No dependencies were downloaded. The two pre-existing fixtures that require stopping the installed daemon were not run.

## Real kernel throttling and recovery

The new ignored fixture was then run explicitly in a fresh delegated user service. Its outer limits were `MemoryMax=256M`, `MemorySwapMax=0`, `TasksMax=16` and `RuntimeMaxSec=45s`. The fixture validates those limits and its unique unit name before creating children. It changes only its own delegated subtree, restores temporary controls and removes its groups afterward.

Two workers allocate and initialize 64 MiB and 16 MiB after entering their cgroups. An additional empty group checks that inactive directories receive no quota. The host-pressure inputs are controlled: total RAM 256 MiB, reserve 32 MiB, and recovered outside usage 32 MiB. Group usage, kernel throttling, allocations, data integrity and progress come from the actual running kernel.

| Stage | Observed result |
| --- | --- |
| Weighted allocation | A 184 MiB shared budget gives the larger worker the larger limit; the empty group receives no temporary limit. |
| Pressure | The aggregate tightens to 64 MiB. Each worker requests another 8 MiB; the larger worker records 17 `memory.high` events. |
| Continued pressure with spare RAM | The severity hint stays 50. Four feedback samples increase the aggregate through 87.6875, 111.375, 135.0625 and 158.75 MiB. |
| Feedback bounds | Each sample measures 89.25 MiB managed usage and computes 134.75 MiB available RAM. Every increment is 23.6875 MiB, exactly one quarter of spare memory above the 40 MiB comfortable reserve. |
| Useful progress | Both workers complete their additional allocations and verify every initialized byte while their limits are still finite. This occurs before Healthy restores `max`. |
| Resource outcome | Zero OOM and OOM-kill events; zero Swap use. systemd reports a memory peak of 112.8M and 138 ms CPU time. |
| Completion and cleanup | The test passes in about 2.15 seconds. The transient unit finishes successfully and is subsequently `not-found`/inactive. |

The aggregate exceeds the former half-RAM ceiling while pressure is still reported. This demonstrates recovery of useful capacity under those controlled inputs, including actual task progress after throttling. It does not measure desktop input latency or establish a whole-machine speedup.

To reproduce, obtain the binary-test executable from `cargo test --locked --offline --bin zmem --no-run`, then substitute its absolute path below. This command runs only the bounded fixture:

```sh
systemd-run --user --wait --pipe --collect \
  --unit=zmem-scheduler-fixture-manual.service \
  --property=Delegate=memory \
  --property=MemoryMax=256M --property=MemorySwapMax=0 \
  --property=TasksMax=16 --property=RuntimeMaxSec=45s \
  --setenv=ZMEM_SCHEDULER_FIXTURE=1 \
  /absolute/path/to/zmem-test-executable \
  linux::scheduling_kernel_tests::kernel_background_budget_throttles_and_recovers \
  --exact --ignored --nocapture --test-threads=1
```

## CLI and launch checks

The optimized candidate passed `status --live --json --processes`, human-readable live status, `plan --json` and read-only `once --json`. JSON parsed successfully, background measurements were complete and error arrays were empty. All 20 process rows included the new membership fields, and human status labeled its budget as proposed.

A real `zmem run --class background` launch entered the dedicated background subtree with leaf `memory.high=max`. Its accounting files existed, and a status sample taken inside the workload identified the populated parent and measured its current usage. A separate fresh slice with explicit `MemoryAccounting=yes` also exposed accounting without a finite leaf limit, then was stopped and collected.

The candidate observe daemon emitted three complete, error-free samples and exited successfully on SIGTERM. The installed service remained active with the same PID throughout these smoke checks. The candidate was not installed as that service during this validation.

## Evidence boundary

The measured kernel fixture uses controlled host-pressure inputs to reproduce the scheduling transition without exhausting the development host. It validates the production budget calculation and control-write path, not a full daemon takeover. Automatic classification of existing applications, CPU/IO hierarchy changes, whole-desktop latency, multi-day behavior and a new package release are outside these results.
