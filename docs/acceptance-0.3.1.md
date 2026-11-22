# zmem 0.3.1 responsiveness acceptance

This record separates a host investigation performed while zmem 0.3.0 was running from the source and installation checks for 0.3.1. It does not report measured mouse input latency or claim that all desktop stutter has been removed.

## Observed problem

On 22 November 2026, the user reported a stuttering mouse. The system had 16,476,700,672 bytes of physical RAM and approximately 2.95 GB available during the initial diagnostic sample. CPU pressure was close to zero, but memory full PSI was about 1.23%, direct scanning was about 156 MB/s, file refaults about 79 MB/s and anonymous refaults about 5.7 MB/s.

The 0.3.0 daemon classified overall memory capacity as healthy and requested swappiness 80 for `file_working_set_refaults`. A background filesystem deduplication service, `project-bees.service`, was reading approximately 82 MB/s. The compositor used approximately one CPU core's worth of time. These observations did not establish a single cause of the reported mouse behavior.

## Bounded host intervention

The filesystem scanner was frozen for a bounded comparison, retaining its PID and progress. An independent 90-second systemd timer was installed before freezing it, so it would resume without depending on the diagnostic client remaining connected. The service subsequently returned to `frozen=0` with the same PID.

During a ten-second sample within that pause, memory full PSI declined from approximately 0.06% to 0.03%; I/O full PSI was 0.18% to 0.51%. The compositor continued to consume approximately 84% to 92% of one CPU core. Desktop IPC requests took roughly 25 to 62 ms. Those timings include command launch and an IPC round trip; they are not cursor-to-display latency.

The service already declared `IOWeight=10`, but `user@1000.service` only delegated `cpu`, `memory` and `pids`. The service's `io.stat`, `io.weight` and `io.max` files were absent. A low requested weight therefore did not establish an applied I/O control.

Adding I/O delegation and reloading the system manager exposed the requested property, but that alone did not establish kernel controls for the existing session. Enabling I/O accounting activated the ancestors; refreshing the user manager then made the controller available to its existing units. The user-manager PID and filesystem-scanner PID remained unchanged.

The scanner then received a per-device read cap of 8,388,608 bytes/s and write cap of 2,097,152 bytes/s. Both values were read back from its actual `io.max` entry. Its process remained active, and the configuration was persisted using a stable device identifier.

| Observation | Result |
| --- | --- |
| Scanner reads, initial two-second observation | Approximately 82 MB/s. |
| Scanner reads, ten-second observation with effective limit | 8,187,844 bytes/s on the capped physical device. |
| Memory full PSI in the two endpoint samples of that limited window | Reported 0.00%. |
| Direct scanning in those endpoint samples | 0 and 77,823 bytes/s. |
| Foreground compositor I/O full PSI in a subsequent sample | Reported 0.00% for avg10. |
| Process continuity | User manager and scanner kept their PIDs; the scanner was not left frozen. |

These samples were taken during a changing, real workload. Later observations included memory full PSI of 0.61% and higher direct scanning; the low values are not sustained ceilings. The global I/O stall number also increased during bandwidth limiting. Background tasks waiting on a limit can contribute to PSI, so that number must be interpreted alongside per-group pressure and actual device throughput. No percentage desktop speedup is inferred.

## Policy regression addressed in 0.3.1

The original file-refault preference had no task attribution and could remain optimistic while direct reclaim and anonymous reactivation were already producing memory stalls. The new `reclaim_latency_guard` requires all of memory full PSI at least 1%, direct scan at least 64 MiB/s and anonymous refault at least 4 MiB/s. It caps that automatic preference at 60 while preserving any lower actual value.

The guard retains ordinary evidence accumulation and write hysteresis. It does not convert this observation into an emergency swappiness value of 20. Confirmed disk-swap churn retains its earlier urgent handling. Current-readback failure now prevents a sysctl write and reports a null request.

Focused replay covered 25 swappiness and control-wiring cases, including the observed combination, exact thresholds, isolated and alternating observations, lower actual values, write/read failures, recovery and the existing disk-churn priority. These are regression tests using measured inputs, not a recreation of full desktop behavior.

## Installation and release checks

The final 0.3.1 source was checked with Rust 1.100.0. `cargo test --locked --offline` passed 193 tests: 12 library, 161 binary and 20 policy integration tests. Three existing privileged host tests remained ignored. Strict Clippy across all targets, formatting, the `no_std` library check, the release build, two version-bump tests and the seven-group probe self-test also passed.

The eight I/O-installation regressions cover append-only delegation, foreign files and symlinks, changed contents/identity/permissions, failed upgrades, racing publication, interrupted preparation, damaged receipts and the unchanged 17-file backup format. The I/O addition has its own ownership record; these tests do not require replacing the legacy baseline schema.

The locally built candidate was transferred as a verified 92,759-byte binary delta and installed on the development host. Its SHA-256 was `d5f25d1bb782f04d82bddeaea45789ededc4f5dfe34f88c990fefc6161514b4f`. The installation checks found:

- The service and executable reported 0.3.1; the service was active with zero automatic restarts.
- The existing configuration hash and all 15 active Swap entries' paths, types, sizes and priorities were preserved. Used-page counts were not treated as fixed values.
- The persistent I/O drop-in and its separate ownership receipt were created. After removing this investigation's two temporary runtime overrides and reloading systemd, the user manager still exposed `cpu io memory pids` and I/O accounting.
- `doctor --json` reported `controller_available` from the calling user's kernel cgroup files. It left `io_weight_effective` unknown, rather than inferring scheduler behavior from a declared weight.
- The background scanner remained populated and unfrozen, with the same 8 MiB/s read and 2 MiB/s write limits read back from `io.max`.
- All 17 discovered interactive protection scopes reported complete protection with no errors after the reload. The daemon's observed swappiness was 20 for the existing `disk_swap_read_pressure` path at that time; this was not a forced guard threshold or a new latency measurement.

These checks identify the locally built installation candidate. The formal release publishes its own build provenance and checksums; independently built executables need not share this candidate's binary hash.

## Scope and limits

- The scanner's bandwidth limits are a measured, host-specific configuration change. They are not imposed on every application by the zmem default policy.
- Preparing user I/O delegation enables a capability. An actual scheduler backend and kernel readback still determine what relative I/O weights achieve.
- The compositor remained busy during the pause. GPU, compositor rendering, input-device behavior and cursor-to-display latency were not benchmarked or attributed to a single fault.
- The [0.3.0 acceptance record](acceptance-0.3.0.md) remains the source for its protection-reset, registration and pressure-admission results. Its deferred native cleanup and positive KSM merge/COW checks are not relabeled as passed.
- Arbitrary application checkpointing and recovery of unsaved application state remain outside these changes.

See [the responsiveness guide](responsiveness.md), [the policy source](../src/swappiness.rs), [Linux PSI](https://docs.kernel.org/accounting/psi.html), [cgroup v2](https://docs.kernel.org/admin-guide/cgroup-v2.html) and [systemd resource controls](https://www.freedesktop.org/software/systemd/man/latest/systemd.resource-control.html) for mechanism and interpretation.

