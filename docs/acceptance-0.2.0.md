# zmem 0.2.0 host acceptance

Recorded on 22 November 2026, Asia/Taipei, on the existing Linux x86_64 workstation with kernel `7.3.5-arch1-1` and 4 KiB pages.

The final executable passed its host swap-helper checks and installed successfully while preserving the active Swap areas and existing configuration. A real background fixture was visible to the daemon and completed its data check. An earlier isolated kernel fixture demonstrated throttling and recovery under bounded synthetic pressure. Three natural-workload windows showed mixed results: the final window had lower memory/IO stall percentages than the original baseline, but higher desktop IPC latency and paging activity. These observations carry no causal performance-improvement claim and do not establish stall-free desktop operation.

## Candidate and evidence identity

The initial baseline observed the existing 0.1.2 daemon. The intermediate after window and bounded kernel fixture used the earlier 0.2.0 candidate. The final 0.2.0 executable has its own follow-up observation window, host helper, installation and workload-routing evidence below. The version string alone does not distinguish the two 0.2.0 source revisions.

| Item | Recorded identity |
| --- | --- |
| Measured intermediate native executable SHA-256 | `14142bff15ffb170907d74af440bb1328c53cc4373db4696f568d8d9e96de487` |
| Final installed release executable SHA-256 | `1c561b2bf4fef07217524c8fae344b26040ff82dcaa90aa511af69f23202322f` |
| Final executable size | 984,576 bytes |
| Native kernel-test executable SHA-256 | `df1d400d80e2ff4c2cb1ec143b7bb93435e0b3aabca6e1c9ebbb3ad871808718` |
| Observer script SHA-256, identical in all three windows | `569b75829e2df770f27b83c3ff19cbd08e8a59641bc79f4aff390f3db5f99c24` |
| Observer runtime | Python `3.14.10`, x86_64 |
| Host memory reported by the installed candidate | 16,476,700,672 bytes, approximately 15.35 GiB |
| Source of the observations | `scripts/measure_responsiveness.py`, kernel counters and explicit service/status checks |

Installation recovery, host-namespace activation, swap-growth batching and allocation accounting received further work after the intermediate candidate was measured. Runtime measurements remain attached to the executable that produced them. The final frozen source passed its build gates, and the helper/installation evidence identifies that final executable by its digest.

The session retained `responsiveness-before/{metadata,summary}.json`, `responsiveness-after/{metadata,summary}.json`, `responsiveness-final/{metadata,summary}.json`, `kernel-budget-acceptance.json`, `installed-status-0.2.0.json`, `installation-0.2.0.json`, `swapspace-retirement.json`, `retirement-headroom.json`, `mount-namespace-probe.json`, `remaining-lag-attribution.json`, `remaining-lag-controls.json`, `project-bees-pressure-probe.json`, `swap-syscall-probe.json`, `swap-slot-consolidation.json`, `final-release-gates.json`, `final-release-binary.json`, `host-swap-helper-acceptance.json`, `final-installation-0.2.0.json`, `final-installed-status-summary.json` and `managed-build-host-acceptance.json` as artifacts outside Git. This record includes their relevant aggregate measurements and identifies additional host readbacks separately.

## Mechanisms covered

The implementation separates requested policy from observed kernel state. Its current contracts are described in the [usage guide](../README.md); the following mechanisms were the focus of this validation.

| Mechanism | Implementation and evidence boundary |
| --- | --- |
| Daemon status and independent previews | [Status handling](../src/main.rs) validates the active daemon's identity and preserves its history, rates, worker state and errors. Requested process details are sampled separately. `--live` and `plan` produce labelled independent previews with unprimed rates. The explicit installed-system status capture exercised the verified-cache path. |
| Paging-cost selection | [Swappiness selection](../src/swappiness.rs) distinguishes file-page refaults, useful compression headroom and disk-swap read pressure. IO congestion alone does not justify lowering the preference. Requested values and kernel readbacks are reported separately. |
| Interactive protection | [Protection planning](../src/protection.rs) and Linux discovery follow recognized processes into actual nested cgroups. A bounded shared protection budget is distributed through the hierarchy; successful scope counts and incomplete operations are visible. |
| Dynamic background budget | [Budget planning](../src/budget.rs) uses occupied memory, available RAM and reserve, and assigns shares only to populated groups. [Application](../src/background.rs) tightens before relaxing, holds controls on incomplete observations and commits read-back limits as the next baseline. Spare headroom permits bounded recovery during continued throttling pressure. |
| Reclaim eligibility and feedback | [Reclaim tracking](../src/reclaim.rs) selects populated managed groups with eligible clean file pages, records requests separately from observed progress and backs off after unproductive work. The saved installed status had no populated background groups and no reclaim candidates. |
| Swap ownership and migration | The installer adopts existing active Swap. Retirement of the final occupied Swapspace file was a separate, deliberate maintenance operation with additional owned capacity prepared first. |

`plan.background_high_percent` remains a severity hint in the policy contract. Actual byte limits are represented by `background.requested_budget_bytes`, `actual_budget_bytes` and the per-group fields. A `null` requested budget with `plan_available=true` means no temporary ceiling. A `null` observed limit with a complete readback means unlimited; missing observations are distinguished by their availability flags.

## Validation gates and candidate boundary

The measured intermediate candidate completed these gates:

| Check | Observed result |
| --- | --- |
| Ordinary Rust tests | 127 passed; 3 privileged tests remained ignored in the ordinary run. |
| Strict Clippy, formatting and `no_std` library check | Passed during candidate validation. |
| `linux::scheduling_kernel_tests::kernel_background_budget_throttles_and_recovers` | One matched test passed; exit code 0; 2.13 seconds. |
| Existing page-rounding/restore and managed-slice-reset kernel fixtures | Not rerun in this session. They require global service or system-slice preconditions and retain their historical evidence in [0.1.2 acceptance](acceptance-0.1.2.md). |

The 127-test count predates the later installation, namespace and growth-request changes. The separate kernel result is credited only for the one case actually selected and executed.

The final frozen source completed the following gates as ordinary UID 1000. Sources: `final-release-gates.json` and `final-release-binary.json`.

| Check | Observed result |
| --- | --- |
| `cargo test --locked --offline --quiet` | 143 passed: 12 library, 111 binary and 20 policy tests; 3 privileged tests remained ignored. |
| `cargo fmt --check` | Passed. |
| `cargo clippy --locked --offline --all-targets -- -D warnings` | Passed. |
| `cargo check --locked --offline --lib --no-default-features` | Passed. |
| `cargo build --release --locked --offline` | Passed; produced the 984,576-byte final executable identified above. |

An intervening source revision had passed 139 ordinary tests before the slot-capacity and allocation-accounting changes; the full-chunk policy follow-up separately passed its 20 policy tests. Those staged results are superseded for final-source coverage by the 143-test gate and are not added to its count. The measurement script's 7 pure-function check groups and the 2 version-bump tests had also passed in the preceding validation, and those scripts were unchanged by the final adapter work.

The final source gate includes allocation-budget arithmetic, typed syscall-error handling, identity changes and failure-cleanup decisions. It leaves the three privileged fixtures ignored in the ordinary run. The one separately executed kernel fixture remains evidence for its measured intermediate executable. Final host helper, installation and background-routing checks are recorded separately below and are not added to the ordinary Rust test count.

### Real kernel throttling and recovery

Source: `kernel-budget-acceptance.json` and the [bounded scheduling fixture](../src/scheduling_kernel_tests.rs).

The test ran inside a fresh delegated service with a 256 MiB memory limit, Swap disabled and a 16-task limit. It created two workers with 64 MiB and 16 MiB payloads, plus an empty group. Cgroup population, memory usage, limit readbacks, allocation progress, event counters and payload verification came from the real kernel and workers. Host-wide pressure inputs were synthetic: the planner received a 256 MiB total and a 32 MiB reserve. This bounds the test's effect on the workstation while exercising real cgroup throttling.

| Stage | Recorded result |
| --- | --- |
| Initial weighted allocation | Requested aggregate 184 MiB; the larger populated group received the larger share. The empty group received no share. |
| Tightening | Requested aggregate 64 MiB; both populated groups read back a 32 MiB `memory.high`. |
| Throttling evidence | `memory.events:high` increased by 17 for the larger worker and 0 for the other worker. |
| Recovery with severity hint still at 50 | Four bounded steps raised the requested aggregate from 64 MiB to 158.75 MiB using refreshed group usage and spare-headroom inputs. |
| Final finite kernel readback | The two limits summed to 166,457,344 bytes, or 158.746 MiB. The one-page difference from the requested aggregate reflects page normalization. |
| Task progress and data | Both workers completed their additional allocation and verified their payloads before the healthy-policy step. |
| OOM handling | `oom` and `oom_kill` counter deltas were both zero. |
| Healthy-policy step | The fixture removed the temporary restrictions and both workers exited successfully. |

This fixture covers real allocation and `memory.high` behavior for the planner's bounded requests. Its controlled pressure inputs do not reproduce the workstation's full application mix. Write-failure ordering and confirmation of partial application also have ordinary regression coverage in the background adapter; the fixture is not a measurement of general desktop or build throughput.

## Intermediate installed daemon and effective controls

Source: `installed-status-0.2.0.json`, captured separately from the timed observation runs.

The explicit system executable at `/usr/local/bin/zmem` returned `observation.source=daemon_cache`, `instance_verification=proc_identity`, `daemon_active=true` and `stale=false`. The report belonged to PID 1845192. Its sample age was 1,190 ms and cache age 402 ms; the requested process list came from a fresh procfs read. Measured paging rates were available.

| Field | Saved installed-status result |
| --- | --- |
| Memory pressure / IO congestion | `healthy` / `true`, reported separately |
| Successful critical-scope protection | 15 scopes; `protection_complete=true` |
| Protection budget | 2,059,587,584 bytes; this is the shared budget, not an application-memory saving |
| Managed background coverage | 1 observed aggregate, 0 populated groups, 0 tasks; `coverage_complete=true` |
| Aggregate background budget | No temporary requested ceiling; actual active-group total 0; sampling, readback and application complete |
| Reclaim candidates / requested reclaim | 0 / 0 bytes |
| Worker / errors | No worker; empty error list in this capture |
| Swappiness request / readback | 130 / 130, with reason `hysteresis_hold` |
| Page-cluster readback | 0 |

Later host readbacks in the same maintenance session recorded swappiness 80 with `file_working_set_refaults`, followed by 20 with `disk_swap_read_pressure`. These are observations of adaptation to different samples. They do not establish a fixed preferred value for this machine. A service memory display was approximately 3.2 MB at one check; this is a point observation, separate from the observer process's resource usage below.

The empty background aggregate is a material scope limit. This installed-status capture validates that an empty managed group received no allocation share or reclaim request. It supplies no real background-work throughput measurement. Existing applications outside the managed background class retained their placement. Protection of recognized interactive scopes is a separate operation; an individual child's zero `memory.low` also needs to be read in the context of ancestor and recursive protection.

## Three 60-second natural-workload observations

Sources: the before, intermediate after and final `metadata.json` and `summary.json` files. All three windows used the same observer script digest, kernel, Python version, page size and sampling intervals. The observer sampled kernel state every two seconds and measured a Python timed wait every 100 ms. The optional `hyprctl -j monitors` request was enabled, with a bounded 0.8-second command timeout.

| Collection property | Before (0.1.2) | Intermediate after (0.2.0) | Final installed (0.2.0) |
| --- | --- | --- | --- |
| Start, Asia/Taipei | 2026-11-22 14:32:16 | 2026-11-22 15:16:37 | 2026-11-22 15:52:16 |
| Observed duration | 60.018 s | 60.025 s | 60.039 s |
| Kernel snapshots / counter windows | 31 / 30 | 31 / 30 | 31 / 30 |
| Valid duration for the reported PSI/paging means | 59.998555 s | 59.998993 s | 59.999043 s |
| Timed-wait observations / skipped deadlines | 600 / 0 | 600 / 0 | 600 / 0 |
| Successful desktop requests / timeouts | 31 / 0 | 31 / 0 | 31 / 0 |
| Daemon version reported through status | 0.1.2 | 0.2.0 | 0.2.0 |
| Cache-source metadata in the collected reports | Unknown for all 31 reports | Unknown for all 31 reports | `daemon_cache` for all 31 reports |

The intermediate after run's `zmem` command still resolved through the installed oh-my-ztty 0.2.8 bundle, whose CLI was version 0.1.2. That older CLI read the new daemon's 0.2.0 cache without adding the new process-identity provenance fields. The daemon version in that summary therefore identifies the report producer, while the explicit installed-system capture above supplies its separate verified-cache evidence. The intermediate collection is not relabelled as an identity-verified CLI run.

The final collection followed installation of the frozen executable and the bounded background fixture. Its command path explicitly selected `/usr/local/bin/zmem`, so it used the new CLI's process-identity verification. All 31 status requests returned `daemon_cache` reports for version 0.2.0, with no diagnostic occurrences. This is a distinct final-version observation, not a replacement for the intermediate run.

The intermediate window began approximately 44 minutes after the baseline; the final window began approximately 80 minutes after the baseline. Ordinary workstation use and other project activity continued. Workload composition, file-cache state and surrounding maintenance were not held constant. The observer did not create a benchmark workload or change memory controls. For a fresh collection using the installed system CLI, the reproducible invocation is:

```sh
PATH=/usr/local/bin:$PATH python3 scripts/measure_responsiveness.py --duration 60 --desktop-probe \
  --output artifacts/responsiveness-new-run
```

Use a fresh output directory and record the CLI identity, daemon identity and installed binary alongside the observer artifacts.

### Latency observations

The scheduler metric is Python timed-wait lateness, which includes timer, scheduling and interpreter effects. The desktop metric includes starting `hyprctl`, IPC and receipt of its response. Input-to-display latency and frame delivery were not measured. Percentiles use linear interpolation at `(count - 1) × quantile`.

| Metric, milliseconds | Before (0.1.2) | Intermediate after (0.2.0) | Final installed (0.2.0) |
| --- | ---: | ---: | ---: |
| Scheduler p50 | 0.115 | 0.075 | 0.101 |
| Scheduler p95 | 0.199 | 0.187 | 0.178 |
| Scheduler p99 | 0.263 | 0.310 | 0.219 |
| Scheduler maximum | 0.857 | 0.742 | 5.159 |
| Hyprland process + IPC p50 | 19.222 | 49.388 | 50.097 |
| Hyprland process + IPC p95 | 69.021 | 110.295 | 130.963 |
| Hyprland process + IPC p99 | 75.071 | 170.861 | 196.247 |
| Hyprland process + IPC maximum | 77.298 | 195.854 | 207.962 |

The final window had lower scheduler p95/p99 values than the baseline, but a higher maximum timed-wait lateness. Every listed desktop-request latency statistic was higher in both later windows than in the baseline: final Hyprland request p95 was 130.963 ms versus 69.021 ms before. There were no desktop-request timeouts in any window. These observations provide no basis for a general responsiveness-improvement or stall-free-desktop claim.

### Memory pressure and paging observations

PSI values below are derived from cumulative stall-time deltas over the valid observation intervals. Rate means are weighted by interval duration. One MiB is 1,048,576 bytes. Swap, refault and scan counters use the recorded base-page size; refault and scan byte equivalents describe page activity rather than physical-disk throughput.

| Metric | Before (0.1.2) | Intermediate after (0.2.0) | Final installed (0.2.0) |
| --- | ---: | ---: | ---: |
| MemAvailable minimum, MiB | 1,077.969 | 809.191 | 905.934 |
| MemAvailable median, MiB | 1,357.898 | 1,192.305 | 1,111.379 |
| MemAvailable maximum, MiB | 1,813.211 | 1,467.613 | 1,246.055 |
| Memory some PSI, mean % | 10.064 | 4.398 | 4.046 |
| Memory full PSI, mean % | 9.279 | 4.318 | 3.867 |
| IO some PSI, mean % | 93.941 | 9.832 | 28.955 |
| IO full PSI, mean % | 86.525 | 9.195 | 26.058 |
| CPU some PSI, mean % | 0.477 | 1.076 | 0.788 |
| Swap-in, mean MiB/s | 19.744 | 56.844 | 32.434 |
| Swap-out, mean MiB/s | 14.920 | 76.970 | 36.447 |
| File refault, mean MiB/s of page equivalents | 90.948 | 218.128 | 105.616 |
| Anonymous refault, mean MiB/s of page equivalents | 20.157 | 57.533 | 32.784 |
| Direct scan, mean MiB/s of page equivalents | 155.979 | 488.478 | 232.379 |
| zram reads, mean MiB/s | 6.442 | 31.335 | 14.422 |
| zram writes, mean MiB/s | 6.961 | 33.135 | 15.979 |

Both later windows combined lower memory/IO stall percentages than the baseline with less available RAM and more paging activity. Final IO full PSI was higher than in the intermediate window, while final memory full PSI was slightly lower. The observations alone do not isolate which workload, cache, storage or policy change produced those differences. The timed runs also provide no useful-work throughput comparison: one managed aggregate was visible in each window, its CPU counter did not advance, and its IO counters were unavailable. The bounded background fixture described below was a separate check.

Each summary retained 30 undefined system-level CPU-full values and 60 unavailable background-IO interval values. These remain unavailable values, not zeros. The page size was consistent throughout all three runs; no summary recorded a counter-reset reason. Kernel snapshot reads were sequential rather than atomic, with observed maximum read spans of 4.099 ms before, 4.962 ms in the intermediate run and 11.318 ms in the final run.

### Status diagnostics and observation cost

All 31 status requests were available in each window. The baseline report had no separate memory-pressure classification. The intermediate reports classified memory pressure as `critical` in all 31 samples and contained one diagnostic occurrence: sample 19 reported `grow worker failed (exit status: 1)`. The final reports classified memory pressure as `elevated` in all 31 samples and contained zero diagnostic occurrences. The successful standalone status captures and final window do not erase the earlier timed-run diagnostic.

| Observer resource | Before (0.1.2) | Intermediate after (0.2.0) | Final installed (0.2.0) |
| --- | ---: | ---: | ---: |
| Observer CPU time | 0.152154 s | 0.152081 s | 0.123224 s |
| Peak observer RSS | 40.438 MiB | 37.129 MiB | 40.105 MiB |
| Probe-child CPU time | 0.316061 s | 0.476414 s | 0.288106 s |

These figures measure the Python observer and its child commands. They are separate from zmem daemon memory or CPU consumption. The collector retained aggregate counters and status fields; its command probes did not retain process command lines, environment contents, window titles or monitor-response payloads.

## Remaining pressure and workload placement

Sources: `remaining-lag-attribution.json`, `remaining-lag-controls.json` and `project-bees-pressure-probe.json`. These short diagnostic windows followed the formal 60-second collection and have their own observation boundaries.

A 4.999-second read-only trace found `cargo` and `rustc` in the same session cgroup as SSMCP's remote-access processes. That group was outside zmem's managed background subtree. Its saved controls had `memory.high=max` and a 128 MiB `memory.low`; zmem's background budget therefore did not isolate the compiler from the remote session in this capture.

| Observation in the combined SSMCP/compiler group | Recorded value |
| --- | ---: |
| Group current memory | 1,390.7 MiB |
| Group Swap | 3,148.8 MiB |
| Group file refault rate | 68.63 MiB/s of page equivalents |
| Group direct scan rate | 176.35 MiB/s of page equivalents |
| Group memory full PSI | 3.131% |
| `rustc` RSS | 1,246.0 MiB |
| `rustc` Swap | 2,898.4 MiB |

The group and process rows are overlapping views and must not be summed. The trace identified a concrete classification gap and substantial compiler paging activity; it did not measure frames or assign all desktop delay to that compiler. The process scan also lacked permission for many per-process IO counters, so unavailable IO observations cannot be interpreted as zero traffic.

`project-bees.service` is a separate Btrfs disk-deduplication service with its own `MemoryHigh=256M` unit setting. It is not a zmem memory-deduplication feature. The first trace observed file scans and IO activity in that service. A later, targeted 2.001-second probe recorded current memory of 95.84 MiB, no increase in `memory.events:high`, no memory or IO PSI, and no direct-scan/refault progress during that window. Its cumulative high-event total was unchanged at 2,360,343. Those observations do not establish that the 256 MiB limit was a current bottleneck; the old cumulative event count is not a current throttling rate.

The consumer build entry points were updated to classify future builds at launch. oh-my-ztty's bundle compiler and Project Z's `run` entry point for build/check/test and related compilation commands prepend `zmem run --class background --` when a version-0.2-or-newer CLI and an identity-verified active daemon are available. Linux/systemd capability checks are bounded; an unavailable manager or `ZMEM_BUILD_BACKGROUND=0` leaves the original invocation in place with a diagnostic. The local focused gates passed 25 tests for the suite and 10 for the aggregate, including the existing build/runner behavior and a real child-process fixture for IO, workdir, environment, outer Cargo lock and exit-status preservation.

This routing applies to newly launched work through those entry points. The already-running compiler and remote session from the trace were left in place. No claim is made that this source change had already improved the recorded desktop latency or reclassified arbitrary existing applications.

## Installation outcome and follow-up findings

Source: `installation-0.2.0.json`, `mount-namespace-probe.json` and the host's recovery/readback record.

The recorded successful install returned exit code 0 and enabled the 0.2.0 service. All 19 Swap entries recorded before this successful invocation were present afterward with the same path, size and priority. Used-page counts changed during normal operation. At that point the final occupied Swapspace file was still active; its retirement happened separately.

An earlier install attempt exposed a recovery defect while Swap path aliases were changing. The preservation check rejected that attempt, and failure recovery restored the original first-migration baseline, removing the then-working zmem executable instead of restoring the immediately preceding installation. The maintainer restored 0.1.2 promptly; a subsequent 0.2.0 install succeeded after the aliases stabilized. This event is evidence for the need to distinguish an upgrade-attempt snapshot from the original migration baseline. The successful later invocation does not validate failure recovery for the measured executable.

A bounded namespace probe also found a concrete capability requirement. The minimum-capability variant of `nsenter` failed to open `/proc/1/root` with a permission error; the variant including `CAP_SYS_PTRACE` succeeded. This establishes namespace-entry behavior for those probe profiles. It does not by itself exercise future swap-file activation under the revised service sandbox.

The timed-run growth diagnostic came from a reserve-sized allocation too small for the swap worker despite ample disk space. A later investigation also exposed exhaustion of kernel swap slots by many small files, described below. The resulting policy requests a full configured chunk, 2 GiB by default, bounded by the disk-swap cap. The pressure, IO and cooldown gates remain unchanged. This both avoids tiny deficit-sized requests and creates headroom without consuming a new slot for every small change in demand.

The updated installer records a separate attempt snapshot and retains the first-install baseline for explicit rollback. The swap helper enters the host mount namespace through util-linux `nsenter`, checks a pinned file identity and calls the activation/deactivation syscall there; its service capabilities include `CAP_SYS_CHROOT` and `CAP_SYS_PTRACE` for access to PID 1's namespace and root. The other service restrictions remain in the unit. Existing aliases created by older service namespaces can remain visible until reactivation or a normal boot. The final executable's host tests below exercise the helper and a successful upgrade; deliberate failure injection of that final upgrade remains outside the host results.

### Kernel swap slots and consolidation

Sources: `swap-syscall-probe.json`, `swap-slot-consolidation.json` and the host's kernel-configuration and post-consolidation readback record. These checks were performed after the formal natural-workload observations.

The host had reached 24 active Swap areas. Inspection of this kernel's configuration accounted for the limit as 32 encoded slots minus 3 migration entries, 1 memory-failure entry and 4 device-private entries; `CONFIG_PTE_MARKER` was not enabled. The resulting 24-area limit is specific to this kernel configuration. Available disk bytes alone therefore could not establish that another swap file could be activated.

At that limit, repeat activation probes against an already-active file returned `EPERM` (errno 1) for both its canonical path and pinned descriptor. The result reproduced in a root context with full capabilities and no seccomp filter, and in the private service unit. The inspected kernel path allocates a swap slot before reaching its already-active-file check. This explains why slot exhaustion could surface as a permission-looking error rather than the expected busy-file result.

The maintainer then retired 16 old alias files one at a time. Each held roughly 4–20 MiB of pages; the operation required at least 4 GiB of free capacity in other Swap and at least 768 MiB of available RAM before proceeding. These were maintenance guards, not new default policy settings.

| Consolidation observation | Recorded result |
| --- | --- |
| Active Swap areas during this consolidation stage | 24 before; 8 after |
| Old alias files retired | 16 |
| File allocation removed | 3,660,529,664 bytes, approximately 3.409 GiB |
| Repeat activation probe using the same executable | Changed from `EPERM` (1) to `EBUSY` (16) after slots were released |

The unchanged executable and changed slot occupancy support slot exhaustion as the cause of this activation failure. `EBUSY` is the expected result for the already-active probe target; it is not a successful new-file activation test. The removed allocation belongs to these old zmem alias files and is separate from the Swapspace-package retirement below. No net filesystem-free-space gain is inferred from the allocation total.

The count of 8 is a stage observation. The outgoing candidate subsequently grew more files, bringing the count to 12 before the final upgrade. The final install retained those 12 areas; a later status sample recorded 13 after the new daemon added one full 2 GiB file.

### Final allocation budget and failure cleanup

Source: [the final Linux adapter](../src/linux.rs), specifically `owned_swap_allocations`, `disk_swap_budget`, `SwapSyscallError` and `can_remove_failed_swap`, with their ordinary regression cases included in the 143-test gate.

The budget now charges valid owned files in the configured swap directory even when they are inactive or left by an incomplete allocation/activation attempt. The charge for each file is `max(logical_length, allocated_blocks × 512)`. The adapter also charges active non-zram Swap outside that owned set. Exact canonical paths are reconciled so an active owned file is not charged twice; a different alias, even with the same basename or size, remains a separate conservative charge. This can reduce permitted growth until uncertain identity is resolved, while keeping retained disk allocations inside the calculation.

The helper returns a numeric errno in a validated protocol, and the caller represents that result with a distinct syscall-error type. Namespace setup, unreadable paths, malformed replies and timeouts do not become kernel activation failures merely because their text or generic IO error contains the same number. A busy result preserves an existing file and reports `active_identity_confirmed=false`; it does not establish that an orphan alias names that file.

Cleanup of a newly attempted activation requires all of the following: a confirmed nonzero syscall error other than `EBUSY`, an unchanged device/inode identity while the original file remains pinned, and a successful fresh inventory with no active entry at the canonical path. Missing observations, changed identity, busy status and unconfirmed failure preserve the file. Retained files are charged on the next growth calculation. Ordinary regressions cover these failure-cleanup decisions and capacity arithmetic. The host helper fixture below covers activation, identity rejection, busy handling and deliberate retirement; it does not inject every failed-activation cleanup case.

## Final host helper, installation and routing

Sources: `host-swap-helper-acceptance.json`, `final-installation-0.2.0.json`, `final-installed-status-summary.json` and `managed-build-host-acceptance.json`.

### Host swap-helper fixture

The host fixture used the final executable with SHA-256 `1c561b2bf4fef07217524c8fae344b26040ff82dcaa90aa511af69f23202322f`. Its temporary allocation was nominally 64 MiB to bound this test. The ordinary growth policy's configured default remains 2 GiB.

| Helper check | Observed result |
| --- | --- |
| Already-active target | Valid helper response `zmem-swap-v1` with errno 16; helper exit 0. The protocol transports the kernel result separately from setup failure. |
| Wrong expected inode | Rejected with exit 1 and an identity-change error before activation. |
| Fresh growth from the private unit | Exit 0. |
| After the private unit exited | The file remained active at its canonical host path, with 65,532 KiB usable capacity, zero used pages and priority 10. |
| Deliberate retirement | Valid helper response with errno 0; exit 0. |
| Fixture after cleanup | `fixture_swap_removed=true`; no test Swap remained active. |

The stable canonical path after the private unit exited directly exercises the mount-lifetime issue that the earlier namespace-entry probe could not resolve by itself.

### Final installation and state

The final install returned exit 0 in **1.6156 seconds**. The installed executable changed from the measured intermediate digest to the final digest above. The configuration SHA-256 remained `5b9ca0a31dc035bbaee2bca5baeb5ea859cdeb834354dd764fa759077e8ca3c4` before and after the operation. This is the duration of the install invocation, not a measured service-interruption interval.

All 12 pre-existing Swap areas remained active with their recorded sizes and priorities. Nine path spellings changed from canonical paths to namespace aliases. The installer's preservation check used its held `O_PATH` mount/inode witnesses for those changes and completed successfully; basename or size similarity alone was not the identity proof. Used-page counts changed during normal operation.

| Final host readback | Observed result |
| --- | --- |
| Service | `active`, `running`, PID 1976372, `NRestarts=0` at the saved check |
| Service memory counter | `MemoryCurrent=4,108,288` bytes at the install readback |
| Status provenance | `daemon_cache`, `proc_identity`, active and non-stale; sample age 2,073 ms |
| Memory pressure / IO congestion | `elevated` / `true` in the saved status |
| Swappiness request / actual | 20 / 20, with `hysteresis_hold` in that sample |
| Protected critical scopes | 15, with complete protection |
| Background aggregate before the routing fixture | 0 populated groups and 0 tasks; no reclaim candidates |
| Worker / errors | No worker; empty error list in the saved status |
| Active Swap areas in the later status | 13, including one newly created full-chunk file |

The additional file had 2,147,479,552 usable bytes, nominally 2 GiB minus the Swap header, and retained its canonical host path. It demonstrates a successful automatic full-chunk growth by the installed final daemon. The elevated memory pressure and IO congestion in the same report remain material observations; service readiness and successful growth do not establish that all desktop stalls disappeared.

### Managed background work on the host

Both the Project Z and oh-my-ztty build-routing modules selected `/usr/local/bin/zmem run --class background -- cargo build` on the host. A separate bounded fixture then allocated and maintained an 8 MiB data payload in the real `zmem-background` scope. The daemon observed one populated background group and one task; aggregate `memory.current` was 14,032,896 bytes, including the fixture's runtime overhead.

The requested and actual aggregate `memory.high` both read back as 268,435,456 bytes. Sampling, readback, coverage and application were complete, with an empty background error list. After approximately eight seconds the fixture verified its data and exited 0. Its status capture came from a process-identity-verified daemon cache.

This validates host classification, daemon coverage, confirmed limits and completion of the bounded payload. The two prefix checks are not two full Cargo build benchmarks. The existing compiler and remote-session tasks from the earlier trace were not migrated, and this small fixture supplies no general build-throughput or desktop-latency claim.

## Swapspace retirement and storage accounting

Sources: `retirement-headroom.json`, `swapspace-retirement.json` and the final legacy-file/package audit from the maintenance session.

The first two legacy Swapspace files were removed by a separate concurrent maintenance actor, which also uninstalled the Swapspace package. The final file, `/var/lib/swapspace/3`, remained occupied and was retired deliberately after zmem-owned capacity was added. This distinguishes the package/file cleanup from the installer's adoption of active Swap.

| Final-file retirement check | Recorded result |
| --- | --- |
| Occupied data before retirement | 458,674,176 bytes, or 437.426 MiB |
| Available RAM before retirement | 2,575,486,976 bytes, or 2,456.176 MiB |
| Free capacity in other Swap before retirement | 7,348,432,896 bytes, or 7,008.012 MiB |
| Additional owned capacity | A new nominal 2 GiB zmem file; kernel-visible Swap size 2,147,479,552 bytes after its header |
| Final legacy `swapoff` | Exit code 0 |
| Final legacy file afterward | No longer active; deleted |
| Replacement file in the saved final Swap list | Active, priority 10, with zero used bytes at that sample |

The final audit found all three old raw Swapspace files gone and the package absent. Their previously allocated file storage totalled **16,785,948,672 bytes, approximately 15.63 GiB**. This is the allocation removed from those three files. The new zmem file provided nominal 2 GiB capacity for the last retirement, and concurrent filesystem work continued during the session.

The filesystem free-space samples therefore receive separate treatment. The last recorded counter decreased from 661,461,966,848 to 658,855,944,192 bytes across the surrounding work. Concurrent activity and Btrfs transaction/accounting timing were uncontrolled. This record claims the measured removal of legacy-file allocations and the verified final Swap state, without a net `df` free-space gain.

## Verification limits

This record establishes one real bounded allocation/throttling/recovery fixture, successful intermediate and final installation checks, verified daemon-state captures, final host helper operation, completion of a managed background payload and deliberate retirement of the legacy Swapspace files. It also records the installation recovery defect, namespace-entry requirements, growth-worker diagnostics, the kernel-slot reproduction and consolidation, and the compiler/session placement gap that informed subsequent changes.

The natural-workload windows establish observations for their stated times and executable identities. They do not measure application completion throughput, frame/input latency, lossless application checkpoints, cold-boot behavior or multi-day stability. The earlier empty managed-background captures and the later bounded payload have different scopes; neither measures the full application mix under sustained managed load. Historical privileged fixtures retain their original scope. The final frozen source, installed identity and host helper/routing checks have separate recorded evidence; broad performance and long-duration outcomes remain unmeasured.
