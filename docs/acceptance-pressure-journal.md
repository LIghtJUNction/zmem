# Swap pressure and control journal acceptance

Validated on 22 November 2026 (Asia/Taipei), on the Linux development host with Rust 1.100.0, systemd and cgroup v2. The candidate is based on `df778e868b916dddb13a629c19f0f7d66f54825c` and contains the changes recorded with this note. Its package version remains 0.1.2 pending a release.

## Swap pressure reaches the control write

The policy already recognized busy Swap/IO, but the Linux adapter could override that decision when zram had free capacity. The adapter now uses the policy hint before applying zram or profile preferences. `apply_controls` calls the same selection function exercised by the regression tests.

| Input with available zram capacity | Previous effective swappiness | Candidate effective swappiness |
| --- | --- | --- |
| Swap-in at 64 MiB/s, low IO PSI, balanced profile | 120 | 20 |
| Swap-in at 64 MiB/s, low IO PSI, interactive or gaming profile | 100 | 20 |
| Swap-in at 16 MiB/s and Swap-out at 64 MiB/s, balanced profile | 120 | 20 |
| IO full PSI at 1,000 basis points (10%) | 40 | 20 |

These are deterministic policy/control selections. The two parameterized regressions cover all five profiles, all three busy triggers, three zram fill levels, values just below each busy threshold, the 20% remaining-capacity boundary, and the existing disk-swap fallback. Normal zram preferences remain 120 for balanced/throughput/ai and 100 for interactive/gaming.

The kernel interprets [swappiness](https://docs.kernel.org/admin-guide/sysctl/vm.html#swappiness) as the relative cost of swapping and filesystem paging. The patch makes the existing busy-swap cost decision effective; it does not introduce a measured application-latency claim.

## Recovery records follow live controls

Repeated terminal and user-session creation could leave old control records in memory and on disk indefinitely. Periodic pruning now removes records only when their path is missing or their device/inode identity has changed. Permission errors, interrupted metadata reads and other uncertain failures retain the record. An unchanged set causes no journal rewrite.

The bounded reader now rejects data above 1 MiB before restoration begins. The oversized-journal regression accepts exactly 1 MiB, rejects one extra byte, and verifies that the complete original file remains after failed recovery. Additional tests remove and replace real temporary files to exercise identity cleanup while preserving a live record's original and last values. These fixtures require no privileges and leave system controls untouched.

## Validation results

| Check | Observed result |
| --- | --- |
| `cargo test --locked --offline` | 37 passed: 24 binary tests and 13 policy tests. The two pre-existing privileged fixtures remain ignored in this ordinary run. |
| `cargo clippy --locked --offline --all-targets -- -D warnings` | Passed. |
| `cargo check --locked --offline --lib --no-default-features` | Passed; the portable policy library is unchanged. |
| Formatting and optimized build | `cargo fmt` and `cargo build --locked --offline --release` completed successfully. |
| Candidate CLI | `--version`, `status --live --json`, `once --json`, `doctor --json` and `schema` all exited successfully. Diagnostic checks passed and telemetry contained no errors. |
| Candidate observe daemon | Three samples at the default two-second interval, no reported errors, then a successful graceful SIGTERM exit. |
| Existing host state | The installed daemon stayed active with the same PID; configuration bytes were unchanged and every previously active Swap path remained active. |

The Rust commands ran serially under the shared Project Z Cargo owner lock and used the shared target directory. The optimized executable was copied out while that lock was held so later builds could not change the candidate used for host checks. No dependencies were downloaded on the development host.

## Evidence scope

The regression tests validate the changed decision and recovery paths. Host checks exercised the compiled candidate in read-only modes. The installed 0.1.2 service was not replaced, stopped or reconfigured during this validation. The existing privileged tests that require stopping it were not rerun. Whole-machine pressure, desktop latency, multi-day endurance and a new package release are outside this patch's validation results.
