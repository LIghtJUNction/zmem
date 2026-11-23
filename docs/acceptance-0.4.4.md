# 0.4.4 memory efficiency and responsiveness acceptance

Observed on 2026-11-23, Asia/Taipei. The development host uses Linux 7.3.5-arch1-1, cgroup v2 and 4 KiB base pages.

## Outcome

The development host runs the tested 0.4.4 daemon, and the default `zmem` command reports the same version. The release executable, installed file and running process executable all match SHA-256 `22222087799eb545299b25ca8854cf2e329cf22b12983758fa4d3495b8c7d346`. The service was active with zero automatic restarts and about 1.65 MiB charged to its service cgroup at verification.

The final readback reported complete protection, no protection or daemon errors, and a shared protection ceiling of 2,059,587,584 bytes. The input scope retained 256 MiB of `memory.low`; every checked ancestor covered that allocation. Installation retained active swap, and the final host inventory contained 16 swap entries. The previous 0.4.3 executable and configuration were preserved for local recovery.

The [sanitized observations](observations/0.4.4-memory-efficiency.json) contain both measurement windows and the final control readback. They omit process IDs, command arguments, cgroup names, environment values, window titles and typed text.

## Changes and tradeoffs

### Retain bounded control-process demand

Terminal and remote-control scopes now estimate demand from resident plus swapped memory, capped at the existing 128 MiB per scope. A small resident footprint after eviction therefore does not erase the request for protection. Input methods retain their existing 256 MiB cap and priority within their category.

All requests still share `min(RAM / 8, 2 GiB)` and the existing user, category and ancestor allocations. A larger terminal or remote-control request can reduce another application's share. Desktop sessions and ordinary applications retain resident-only demand, so a large shared desktop group's cold swapped content does not enlarge its request. The implementation does not prefetch pages or reserve that amount of physical RAM; `memory.low` supplies a best-effort reclaim preference. See the [protection planner](../src/protection.rs), [responsiveness guide](responsiveness.md) and Linux [memory protection documentation](https://docs.kernel.org/admin-guide/cgroup-v2.html).

### Stop repeatedly reclaiming reused file pages

After successful optional reclaim, the policy observes a managed group's file refault counter for 30 seconds from the request. If refaults reach at least 4 MiB and half the observed reclaimed pages, it pauses further proactive reclaim of that group for 60 seconds. When the kernel does not expose a usable proactive-reclaim count, half the request provides the comparison threshold. Other eligible cold groups remain selectable.

The counter measures reuse across the cgroup and cannot prove which operation evicted each page. Missing or regressed counters discard the observation. Memory growth does not bypass this pause. The existing no-progress behavior remains in place, and a fixed 64-entry cache reuses expired records while preserving active backoffs over new speculative observations. `managed.reclaim_refault_skips` counts candidates skipped in the current reclaim decision, rather than a lifetime total. Source: [reclaim policy](../src/reclaim.rs) and [Linux integration](../src/linux.rs).

### Remove unnecessary idle process discovery

The initial daemon report showed dedup discovery examining 689 processes on its 10-second refresh interval despite zero populated managed background groups. A complete empty cgroup inventory now supplies a fresh, complete empty task list without scanning `/proc`. New work or uncertain inventory triggers an immediate scan. Existing KSM ownership, pause and readback behavior remain intact.

The final live report confirmed `processes_examined=0`, `registered_tasks=0`, `task_observation_age_ms=0`, complete coverage and idle dedup state. This establishes removal of that discovery work for the observed empty case; it does not measure a whole-system CPU reduction. Source: [dedup adapter](../src/dedup_adapter.rs).

## Validation

| Check | Result |
| --- | --- |
| Rust regression suite | 250 passed: 23 library, 207 binary and 20 policy tests; 3 existing privileged fixtures remain ignored |
| Protection regressions | Evicted control demand, role caps, overflow, shared-budget pressure, input priority, resident-only session/application demand and Linux discovery fallbacks passed |
| Reclaim regressions | Thresholds, observation expiry, missing/regressed counters, no-progress interaction, candidate selection, bounded capacity and expired-slot reuse passed |
| Dedup regressions | Empty inventory, first populated/uncertain transition, stale-task retirement and owned KSM pause behavior passed |
| Formatting and Clippy | `cargo fmt --all -- --check` and `cargo clippy --locked --offline --all-targets -- -D warnings` passed |
| Policy core | `cargo check --locked --offline --lib --no-default-features` passed |
| Observer and version tooling | 8 observer self-test groups and 2 version-bump tests passed |
| Release executable | Built with Rust 1.100.0 for x86_64 Linux and executed on the development host |
| Live readback | Matching installed/running executable hashes, active service, fresh daemon report, complete protection and no reported errors |

The lock-inheritance regression now runs in an isolated test process. Concurrent test children must not accidentally inherit its intentionally exec-persistent descriptor and extend its lifetime. The production lock behavior is unchanged.

Compilation and source checks ran outside the development host. No additional whole-machine memory-pressure workload was introduced. Positive live reclaim-backoff and KSM merge throughput were not measured: the observed host had zero populated managed background groups, so those behaviors are covered by source regression tests rather than this live workload.

## Natural-workload observation

Both windows used the same observer with separate output directories:

```sh
python3 scripts/measure_responsiveness.py \
  --duration 40 --desktop-probe --input-probe \
  --output artifacts/responsiveness-observation
```

| Observed metric | Before, 0.4.3 | Final, 0.4.4 |
| --- | ---: | ---: |
| Samples | 21 | 21 |
| Input query timeouts, 800 ms deadline | 0 / 21 | 0 / 21 |
| Input query P95 | 15.28 ms | 4.22 ms |
| Longest successful input query | 44.43 ms | 4.26 ms |
| Compositor query timeouts | 0 / 21 | 0 / 21 |
| Compositor query P95 | 122.09 ms | 41.05 ms |
| Median available memory | 1.54 GiB | 2.04 GiB |
| Mean memory full PSI | 4.81% | 0.17% |
| Mean IO full PSI | 1.95% | 2.47% |
| Mean direct page scanning | 291.29 MiB/s | 24.11 MiB/s |

These are natural-workload observations, not a controlled causal benchmark. Background work and cache state changed. Fcitx reported active in all baseline samples and inactive in all final samples; the observer did not switch the input method. Query times include process startup and IPC, not physical keyboard-to-screen or frame latency. The final window showed lower memory pressure and prompt service queries; IO full PSI was slightly higher than the baseline. These observations do not assign every difference to this patch or establish that all desktop stutter has been removed.

The managed background slice remained unpopulated. Existing arbitrary compiler, agent and application processes therefore remained outside zmem's explicit background admission and per-group proactive reclaim path. Future batch work can use `zmem run --class background --wait-timeout 300 -- COMMAND`; the [admission guide](background-admission.md) explains its limits. The final record supports the installed code and its observed controls, while the [0.4.3 record](acceptance-0.4.3.md) retains the earlier input-priority evidence.
