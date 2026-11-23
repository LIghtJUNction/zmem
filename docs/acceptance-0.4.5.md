# 0.4.5 memory policy and throughput acceptance

Observed on 2026-11-23, Asia/Taipei, with Linux 7.3.5-arch1-1 and 4 KiB base pages.

## Installed outcome

The development host runs zmem 0.4.5. The release executable, installed file and running daemon executable match SHA-256 `76df9ad73e444464fd3559a3d44af7fae6f2aa595efbe926dc4bbc2e0cb8872f`. The default `zmem` command resolves to that installed version. The daemon was active, its status was fresh, and all 21 post-installation samples reported version 0.4.5 with zero diagnostics.

Installation preserved the existing configuration bytes and all 17 active swap entries' paths, types, sizes and priorities. The live readback reported complete protection for 17 critical scopes within the existing 2,059,587,584-byte shared budget. At that readback the service cgroup used 901,120 bytes and had zero automatic restarts. Memory usage is a point observation.

The [sanitized evidence](observations/0.4.5-memory-throughput.json) contains the measurement summaries, build identity and installed controls. It omits process IDs, command arguments, cgroup paths, environment values, window content and input text.

## Changes and their evidence

### Preserve working-set feedback across repeated reclaim

The controller can request reclaim every ten seconds, while its file-refault observation lasts thirty seconds. A later successful request previously replaced that observation, losing reuse accumulated before the new request. Successful requests now preserve an unexpired observation's original counter baseline, threshold and deadline. Finishing a later worker also checks cumulative reuse immediately and reports the actual backoff deadline.

The regression sequence reclaims 64 MiB, observes 16 MiB of file refaults before a second successful request, then reaches 32 MiB within the first observation window. Further reclaim is paused for the existing sixty seconds. Tests cover both 4 KiB and 64 KiB pages, the fallback without a proactive-reclaim counter, immediate worker-completion feedback and the original window's expiry. The cache remains fixed at 64 entries. Source: [reclaim policy](../src/reclaim.rs).

### Avoid maintenance workers that already fail the pressure gate

The persistent sampler uses PSI counter deltas for its current interval. A newly started swap-maintenance worker initially sees the kernel's ten-second average. Under intermittent I/O contention, the parent could launch in a short quiet interval while the worker immediately deferred on the longer average.

The parent now retains `psi_avg10` and checks the more conservative value from both windows before scheduling optional maintenance. Blocked maintenance does not consume the scheduling interval. The child still reads the current state before operating on a file; swap shrinking retains its stricter memory and swap reserves. Malformed, missing, repeated, non-finite or out-of-range PSI fields produce a sampling error.

The regression uses zero short-window I/O pressure and a 7.67% ten-second average, checks that no maintenance is scheduled, and verifies resumption after both windows permit it. It also covers immediate short-window pressure, missing averages and shrink-to-cleanup fallback. Source: [Linux integration](../src/linux.rs); interface semantics: [Linux PSI](https://docs.kernel.org/accounting/psi.html).

### Attribute compressed headroom to the writable swap tier

A low-priority zram device with spare capacity previously could justify a compressed-swap preference even if a higher-priority disk would receive the next allocation. Missing statistics for another active zram could also hide its physical memory cost.

The preference now requires complete observations of every active zram, counts all of their physical residency, and examines the highest-priority tier with available swap slots. Every writable device in that tier must be eligible zram. The existing half-RAM physical ceiling, compression benefit, logical headroom and device memory-limit thresholds remain in effect.

Eight device fixtures cover ordinary single-zram operation, writable disk at equal or higher priority, full tiers, incomplete statistics, aggregate residency and strict capacity boundaries. The live host has one high-priority zram, so this general multi-device repair is validated through fixtures. Its later 83% logical use was consistent with the existing headroom threshold. Source: [device-policy tests](../src/swappiness_device_tests.rs) and [Linux swap priority rules](https://man7.org/linux/man-pages/man2/swapon.2.html).

## Validation

| Check | Result |
| --- | --- |
| Rust regressions | 263 passed: 26 library, 217 binary and 20 policy tests; 3 existing privileged fixtures remained ignored |
| Formatting and Clippy | Formatting completed and Clippy passed with all targets and warnings denied |
| Policy core | Offline, locked `--lib --no-default-features` check passed |
| Observer/version tooling | 8 observer self-test groups and 2 version-bump tests passed |
| Native release build | Rust 1.100.0; 14.09 seconds; peak child RSS 390.95 MiB |
| Build placement | Actual release build entered the existing zmem managed background hierarchy before allocating compiler memory |
| Source identity | Tested sandbox sources and native build sources matched digest `6a06d148eb2473e447696323fd3eb6281523f10a214be00f4cd5bb9d2766d95e` |
| Installed readback | Matching executable identities; current configuration and swap inventory preserved; fresh status and complete protection |

The ordinary regression suite and source checks ran in the sandbox. The native release build used one Cargo job, the existing shared build lock and zmem background admission. It was a successful real build, not a before/after build-throughput comparison. The three ignored kernel fixtures require a stopped daemon and controlled cgroup setup.

## Natural-workload observations

Both windows used the existing observer for forty seconds with desktop and input query probes, collecting 21 samples each. The observer added no synthetic memory-pressure workload.

| Metric | Before: 0.4.4 | After: 0.4.5 |
| --- | ---: | ---: |
| Median available RAM | 2.28 GiB | 2.45 GiB |
| Mean memory full PSI | 0.095% | 0.045% |
| Mean I/O full PSI | 0.95% | 0.51% |
| Mean direct page scanning | 11.57 MiB/s | 4.92 MiB/s |
| Mean swap-in | 1.10 MiB/s | 0.28 MiB/s |
| Input query P50 / P95 | 2.72 / 4.13 ms | 2.45 / 2.85 ms |
| Compositor query P50 / P95 | 12.03 / 98.24 ms | 32.61 / 54.58 ms |
| Input / compositor query timeouts | 0 / 0 | 0 / 0 |

Workload, cache state and input-method state changed between windows. The later window showed lower memory and I/O stalls and a lower compositor P95, while compositor P50 increased. The query probes measure helper startup plus IPC, not physical keyboard-to-screen or rendered frame latency. These observations support the installed service's healthy behavior during the measured windows; they do not assign each difference to this update or establish a whole-machine throughput percentage.

Outside the short release build, the managed background hierarchy was unpopulated in these observations. The reclaim regression therefore has deterministic policy-test evidence rather than a positive live pressure test. Existing arbitrary build processes retain their current grouping. New heavy tasks can enter the existing policy with `zmem run --class background --wait-timeout 300 -- COMMAND`.

## Claim and evidence review

| Claim | Evidence | Scope |
| --- | --- | --- |
| Repeated successful reclaim retains the original reuse signal | Cumulative-refault and expiry regressions | Policy behavior |
| Known average pressure prevents a pointless maintenance launch | Both-window scheduling regression and live `psi_avg10` readback | Parent preflight; child revalidation remains |
| Compressed preference accounts for swap priority and missing devices | Eight device fixtures | General device layouts |
| 0.4.5 is active on the host | Executable hashes, fresh 21-sample status and control readback | Installed host |
| Whole-machine throughput improved by a fixed percentage | No matched throughput experiment | No percentage claim |

The review checked contribution, explanation, experimental support, coverage and control semantics. Reported benefits are limited to the validated behaviors and the recorded observation windows.
