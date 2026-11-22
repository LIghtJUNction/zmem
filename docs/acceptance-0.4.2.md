# zmem 0.4.2 acceptance record

Version 0.4.2 extends bounded memory protection to recognized desktop applications and explicitly classified interactive workloads. It also allows a user's selected applications and session to share otherwise unrequested protection budget. The source checks below passed, and native cgroup readbacks confirm that protection reaches the tested applications through their ancestor hierarchy. The final x86_64 candidate was installed and its identity, service state, background launch path and 30-second observation were verified on the development host.

## Problem and implemented behavior

The investigation found both memory pressure and competing background I/O. The initial host snapshot had approximately **854 MiB of available memory**, with **7.76 GiB of physical RAM occupied by zram**. Memory `full` PSI `avg10` was **2.34%**, and I/O `full` PSI `avg10` was **65.6%**. A nearby daemon interval reported **42.9 MiB/s of swap-in** and **78.9 MiB/s of swap-out**. These readings use different observation windows and describe the same investigation period rather than one synchronized benchmark sample.

Desktop applications had zero leaf `memory.low` despite the daemon protecting terminals, remote access and the desktop session. The [Linux adapter](../src/linux.rs) now recognizes supported GUI process names only in independent application units. It preserves the smaller terminal or remote classification for mixed scopes, handles systemd-escaped terminal unit names, and recognizes explicit `zmem-interactive-<pid>.scope` units under both user managers and the system manager. Generic interpreters and inherited browser processes do not by themselves promote a terminal or remote workload to the GUI class.

The [protection planner](../src/protection.rs) maintains a shared budget of `min(physical RAM / 8, 2 GiB)`, rounded down to the kernel page size. An automatically managed interactive leaf requests at most its observed usage and 512 MiB. A user's session and application categories retain their nominal three-quarter and one-quarter shares, while either category may borrow the other's unrequested share. Borrowing remains within that user's ceiling; proportional-rounding leftovers are not lent twice. Ancestors carry the sum of their selected descendants' allocations without counting that sum again as additional protection.

`zmem run --class interactive` requests `MemoryLow=512M` through systemd at launch. The daemon subsequently accounts for the scope and its complete ancestor chain; its default discovery interval is approximately 30 seconds. A launcher-set nonzero leaf remains a systemd-owned request, and the effective distribution is constrained by the ancestor hierarchy. Linux [`memory.low`](https://docs.kernel.org/admin-guide/cgroup-v2.html#memory-interface-files) is best-effort reclaim protection: it does not reserve or lock 512 MiB, and it does not guarantee a frame rate or prevent every reclaim. With the daemon inactive or automatic protection disabled, the launcher leaf request still depends on the existing ancestor controls.

The final [admission implementation](../src/admission.rs) retains its memory-based startup decision. Global I/O PSI alone does not veto a new background command. The regression fixture permits initial admission with healthy memory and I/O `full` at 65%, then rejects admission with the same I/O reading and memory `full` at 2%. This preserves useful startup behavior when I/O waits include deliberate scanner throttling, while retaining the original memory-pressure gate.

## Live scanner mitigation

The existing project-bees I/O limit covered the device containing the root filesystem, while its scan also accessed the separate device containing home data. The missing device entry was added to the running scanner's cgroup. Both devices now request a read limit of **8 MiB/s** and a write limit of **2 MiB/s per device**. The scanner stayed running throughout the change; no process restart or termination was used for this mitigation.

This is an explicit host configuration repair, separate from automatic GUI protection. Linux [`io.max`](https://docs.kernel.org/admin-guide/cgroup-v2.html#io-interface-files) applies limits per device and permits temporary bursts. The configured limits therefore describe requested pacing, rather than an instantaneous throughput guarantee. Continued I/O PSI remains relevant because paced I/O can still produce waiting time.

## Source validation

The final source gate completed with **224 tests passed, zero failed and three privileged kernel integration tests ignored**. The ignored tests are excluded from the pass count. Native scope exercises below provide additional host evidence; they do not relabel the ignored test cases as successful.

| Gate | Recorded result |
| --- | --- |
| Rust test suite | 224 passed, 0 failed, 3 privileged tests ignored |
| `cargo clippy --all-targets -- -D warnings` | Passed |
| `cargo fmt --check` | Passed |
| Release build | Passed |
| `no_std` library check | Previously passed in this iteration; the policy core remained unchanged afterward |

The protection regressions cover shared-budget conservation, borrowing within one user, rounding, ancestor totals, explicit root and user scope classification, independent GUI discovery, escaped terminal units and preservation of mixed terminal/remote scopes. The admission regression covers the separation between high I/O waiting and the existing memory gate. These checks validate classification and decisions; desktop timing is evaluated separately.

## Native protection checks

Kernel readbacks during this iteration showed nonzero leaf protection for the previously unprotected application scopes, with every required ancestor also nonzero. These hierarchy exercises used the intermediate candidate; its protection and interactive-launch implementation is unchanged in the final candidate. The values below are the observed `memory.low` requests at that check, expressed in MiB. They are not measured resident-memory savings.

| Application scope | Earlier leaf `memory.low` | Observed leaf `memory.low` | Ancestor readback |
| --- | ---: | ---: | --- |
| WeChat | 0 | 41.02 MiB | All required ancestors nonzero |
| ChatGPT main | 0 | 48.47 MiB | All required ancestors nonzero |
| ChatGPT renderer | 0 | 188.21 MiB | All required ancestors nonzero |
| Chrome scope A | 0 | 33.45 MiB | All required ancestors nonzero |
| Chrome scope B | 0 | 33.04 MiB | All required ancestors nonzero |

Separate user and root invocations of `zmem run --class interactive` each ran an approximately 8 MiB test workload. After 35 seconds, both had a 512 MiB leaf request and nonzero protection through their complete ancestor chain. Both commands exited with status 0. These small workloads validate launch routing and the daemon's subsequent hierarchy setup without introducing a large artificial memory load.

## Observation method

The [responsiveness observer](../scripts/measure_responsiveness.py) records natural host activity with a two-second sampling interval. Each reported window below requested 30 seconds and produced 16 samples. Available-memory distributions use the sampled `MemAvailable` values. PSI and swap-rate summaries use valid counter deltas and elapsed monotonic time; the reported means are weighted by observation duration. The initial `avg10` snapshot above is kept separate from those interval-based results. Linux [PSI documentation](https://docs.kernel.org/accounting/psi.html) describes both rolling averages and cumulative stall counters.

The desktop probe times a bounded `hyprctl -j monitors` invocation. Its result includes process creation, IPC and receipt of the response; it does not measure frame presentation, keyboard latency or mouse-to-pixel latency. The observer discards the monitor payload. Probe percentiles below describe these IPC round trips and use milliseconds.

### Earlier and intermediate observation windows

The first column was collected after the two-device scanner repair while zmem 0.4.1 was still running. The second belongs to an **intermediate protection build**, collected before the final memory-only admission path was restored. Its protection behavior informs this investigation, while final-candidate timing belongs in the next section.

| Metric | 0.4.1 after scanner repair | Intermediate protection build |
| --- | ---: | ---: |
| Requested duration | 30 s | 30 s |
| Samples | 16 | 16 |
| Available memory, minimum | 1.590 GiB | 2.094 GiB |
| Available memory, median | 1.746 GiB | 2.296 GiB |
| Available memory, maximum | 1.885 GiB | 2.374 GiB |
| Memory `full` PSI, time-weighted mean | 2.8158% | 0.9867% |
| I/O `full` PSI, time-weighted mean | 16.5877% | 21.1608% |
| Swap-in, time-weighted mean | 4.169 MiB/s | 5.359 MiB/s |
| Swap-out, time-weighted mean | 5.039 MiB/s | 3.821 MiB/s |
| Desktop probe, median | 26.292 ms | 16.174 ms |
| Desktop probe, p95 | 141.520 ms | 101.768 ms |
| Desktop probe timeouts | 0 | 0 |

The intermediate window had 16 successful probes out of 16 and a maximum round trip of 205.335 ms. Its available memory and IPC percentiles improved relative to the preceding window, while I/O PSI and swap-in increased. A memory-heavy `rustc` process also finished naturally during the investigation. Workload changes, scanner pacing and the protection update therefore remain confounded; these windows establish observed values rather than a causal speedup attributable to zmem.

### Final installed candidate

The final x86_64 executable reports `zmem 0.4.2`, is **1,162,632 bytes**, and has SHA-256 `db2fb36c6ac411d3f454a288893f0a743365e2b178167d3c552d18e000b97b39`. It was built with Rust 1.100.0 and installed on Linux `7.3.5-arch1-1`. The installed file's digest matched the validated local release build; the zmem system service was active after installation. Active Swap was retained by the installer.

A fresh report from the active daemon identified version 0.4.2, healthy memory pressure, **19 protected critical scopes**, a complete protection pass and no diagnostic or protection errors. The shared protection budget was 2,059,587,584 bytes (1.918 GiB), within the existing one-eighth-RAM ceiling.

After the final window, `zmem run --class background --wait-timeout 0 -- /usr/bin/true` was admitted immediately and exited with status 0. Its admission event recorded 2548043776 available bytes and memory `full` PSI of 0.29%. The nearby daemon sample still showed I/O `full` at 20.87%; it is a separate sample, not an I/O value recorded in the admission event. This check exercised the final memory-based startup path under the live workload. Kernel readback also reconfirmed both scanner device entries at 8 MiB/s reads and 2 MiB/s writes.

### Final 30-second observation

The final installed candidate completed a 30.025-second observation with **16 samples**, using the same observer and scanner limits. All 16 daemon reports identified version 0.4.2 and healthy memory pressure, with zero diagnostic occurrences.

| Metric | Final installed 0.4.2 |
| --- | ---: |
| Available memory, minimum | 2.254 GiB |
| Available memory, median | 2.370 GiB |
| Available memory, maximum | 2.409 GiB |
| Memory `full` PSI, time-weighted mean | 0.4180% |
| I/O `full` PSI, time-weighted mean | 21.5388% |
| Swap-in, time-weighted mean | 1.265 MiB/s |
| Swap-out, time-weighted mean | 0.189 MiB/s |
| Direct reclaim scan, time-weighted mean | 39.473 MiB/s |
| Desktop probe, median | 16.053 ms |
| Desktop probe, p95 | 52.350 ms |
| Desktop probe, maximum | 66.382 ms |
| Successful desktop probes | 16 / 16 |
| Desktop probe timeouts | 0 |

These are observations of the running desktop, not a replayed workload. The earlier heavy compiler was already absent before this final window; there was no attempt to hold all other host activity constant. The immediate background-launch check ran after the measurement. I/O waiting remained substantial even though memory stalls and swap traffic were lower in this window.

The [machine-readable final observation](observations/0.4.2-final.json) retains the observer's summary and verified binary identity without process command lines or personal paths.

## Evidence review and remaining scope

| Review question | Evidence and conclusion |
| --- | --- |
| What behavior is established? | Source regressions and native cgroup readbacks establish application selection, bounded planning and protection through the tested ancestor chains. |
| Are the controls and units clear? | `memory.low` requests are distinguished from resident memory; per-device I/O limits, MiB/GiB, PSI windows and IPC milliseconds are labelled. |
| Is a performance gain isolated? | No controlled causal comparison was performed. Natural workload changes, including the compiler finishing, affect the observation windows. |
| Is final acceptance complete? | Source gates, native hierarchy checks, final installed-binary identity, healthy daemon reports, immediate background admission and the final observation are recorded. Three privileged tests remain ignored. |
| Does the design preserve its boundaries? | Protection remains best effort and shared, explicit launcher requests retain their systemd ownership, and unrelated I/O waiting does not become an independent memory-admission veto. |

The remaining I/O waiting warrants further observation under comparable workloads. This record establishes the completed control changes and the measurements shown; it makes no claim that all desktop stutter is resolved, that applications have a fixed memory reservation, or that the IPC figures are display-frame latency. Public release artifacts require their own build identity and release-workflow results.
