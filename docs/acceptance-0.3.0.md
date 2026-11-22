# zmem 0.3.0 acceptance

This report records the checks performed on 22 November 2026. It establishes a repaired protection chain and working task registration and pressure admission. It does not establish an overall desktop speedup or a measured KSM memory saving.

## Source and environment

The tested Linux x86_64 executable was built with Rust 1.100.0 from the 0.3.0 source. Its SHA-256 was `80113aa5246da8485b158b24f3d202c166a83f0bfd3b88e089193792344fc240`. The development host uses systemd, cgroup v2 and Btrfs, with 16,476,700,672 bytes of physical RAM and existing zram and disk Swap. The checks retained running workloads.

All Rust commands shared one outer Cargo lock and target directory. The source tree passed 176 Rust tests: 12 library tests, 144 executable tests and 20 policy integration tests. Three existing tests requiring an isolated privileged cgroup setup remained ignored in this run. Strict Clippy, formatting, the `no_std` library check, the release build and two version-bump tests passed.

## Interactive protection regression

The host exposed a concrete failure after systemd reset six previously owned `memory.low` controls to their original zero values. These controls included shared ancestors of the desktop and terminal scopes. Their leaf values were still present, but no complete protection chain could be verified. The status report showed zero fully protected scopes and an incomplete operation; its error messages disappeared between reconciliation cycles.

The repair recognizes a zero reset only for the same owned cgroup identity and a zero original baseline. It preserves nonzero external settings. Known reset values are checked during ordinary sampling, while complete process discovery still runs periodically. The last protection errors and attempt time remain available between discovery cycles. See the [implementation](../src/linux.rs) and [project guide](../README.md).

A real `systemctl daemon-reload` after installing 0.3.0 produced six `protection_default_reapplied` events. The same daemon PID restored complete protection for 15 critical scopes, with no reported error. The reload-and-verification procedure completed in 2.34 seconds. This elapsed time measures the repair procedure, not input latency or frame time.

## Upgrade and command execution

The installation transaction completed in 1.90 seconds. All 15 active Swap inventory entries retained their paths, types, capacities and priorities. Their used-page counts changed with the running workload. The configuration checksum was unchanged, the daemon was active with zero restarts, and its initial systemd memory-accounting sample was 2,740,224 bytes. That one sample is not a long-running memory ceiling.

The native launch checks established the following results:

| Check | Observation |
|---|---|
| Ordinary background command | `PR_GET_MEMORY_MERGE` returned 0; the scope had no dedup marker. |
| Explicit background `--dedup` command | The executed Python process read back flag 1 and `ksm_merge_any: yes` in its marked zmem scope. |
| Command boundary | Target arguments `--help --config literal-target-arg` arrived unchanged after `--`. |
| Process execution | Environment, working directory and target exit code 23 were preserved. |
| Interactive class | `--dedup` was rejected before running the command. |

An explicitly registered task then remained alive through eight daemon samples. Three samples observed it with complete coverage. Each reported `paused_pressure`, reason `memory_pressure`, and actual KSM `run=0`. This validates registration, discovery and the busy-system admission gate; it does not validate a completed merge.

## Deferred host checks

The isolated 64 MiB inactive-file cleanup fixture did not start because the host's I/O full PSI remained above its 1% admission threshold. The initial attempt and eight subsequent checks retained receipts. No fixture allocation or active Swap retirement was performed by those attempts. Pure lifecycle tests covered candidate rotation, pressure and capacity gates, guarded deletion eligibility and a real fork/exec mutation-lock lifetime.

The separate two-region KSM merge/COW fixture also did not start: its initial attempt and twelve subsequent checks found I/O full PSI at or above the fixture's 10% threshold. The fixture would have used two private 1 MiB mappings, fixed 128-page/100 ms scanning and an at-most-20-second scan window. Its preflight skips did not change KSM settings. No measured deduplication saving or positive native merge/COW result is claimed here.

These deferred checks remain distinct from the successful registration and busy-system admission checks. The production controller retains its own stricter scanning gates and the [documented maintenance policy](swap-lifecycle-and-dedup.md). Thresholds were not lowered to obtain a passing host result.

## Claim review and limits

| Question | Evidence and conclusion |
|---|---|
| What concrete fault was repaired? | Six owned zero-baseline protection resets were reproduced and automatically repaired on the running host. |
| Can requested and observed behavior be distinguished? | Status includes actual kernel readback, task coverage, observation ages, persisted protection errors and separate dedup target state. |
| Is an overall speed improvement established? | No. The workload was uncontrolled, and no matched desktop-latency comparison was completed for 0.3.0. |
| Is the evaluation complete for all new operations? | No. Registration and pressure admission passed natively; positive cleanup and KSM merge/COW host checks were deferred by pressure. |
| What prevents maintenance from increasing contention? | Existing pressure gates, bounded attempts, full-file retirement reserves, helper-lifetime mutual exclusion, and pause-only KSM pressure handling. The [kernel documentation](https://docs.kernel.org/admin-guide/mm/ksm.html) explains why `run=0` preserves merged pages. |

The earlier [0.2.0 natural-workload observations](acceptance-0.2.0.md) remain attributed to that version. Application checkpointing, preservation of arbitrary unsaved application state, and automatic recovery of computation progress are not implemented by these changes.
