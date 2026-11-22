# Responsiveness, reclaim and background I/O

Available RAM and responsiveness describe different things. A machine can have spare memory while a task repeatedly faults pages back in, waits for storage, or enters direct reclaim. zmem uses measured paging and pressure alongside capacity; the pressure label alone is not a desktop-latency measurement.

## Read a current observation

```sh
zmem status --json
zmem status --json --processes
```

Check `observation.source`, `daemon_active`, `instance_verification`, `stale` and the sample age first. The active daemon report contains measured rates and policy history. A one-shot preview starts without those rates.

| Field | What it establishes |
| --- | --- |
| `memory.available_bytes` | The kernel's estimate of available RAM, including reclaimable capacity. |
| `psi.memory_full_bp` | Observed memory stall time, in basis points; 100 bp is 1%. |
| `paging.direct_scan_bytes_per_sec` | The rate of direct page scanning; this is work performed, not bytes successfully freed. |
| `paging.anon_refault_bytes_per_sec` | Anonymous pages becoming active again after eviction, expressed as a byte rate. |
| `paging.file_refault_bytes_per_sec` | File pages returning after eviction; it does not identify a foreground application. |
| `controls.swappiness_actual` and `swappiness_requested` | Kernel readback and the controller's current request. A missing request is not a successful write. |

Pressure is also available for individual cgroups. A background service waiting on its own bandwidth limit can contribute I/O stalls while the desktop's I/O pressure remains low. Read the relevant groups before interpreting a global I/O number as a saturated physical drive. Linux documents [PSI](https://docs.kernel.org/accounting/psi.html) and the [cgroup v2 pressure and I/O interfaces](https://docs.kernel.org/admin-guide/cgroup-v2.html).

## Avoid an unsupported cache preference

File refaults establish that pages were evicted and accessed again. They do not establish that those pages belong to interactive work. A background filesystem scanner can generate refaults while causing anonymous memory to be recycled.

Starting with 0.3.1, the `reclaim_latency_guard` prevents the file-cache and spare-zram paths from increasing swappiness when all three measured conditions hold:

- Memory full PSI is at least 1%.
- Direct scanning is at least 64 MiB/s.
- Anonymous refaults are at least 4 MiB/s.

The resulting preference is a ceiling of 60. If the actual kernel value is already lower, the guard retains that lower value. It uses the normal 10-second evidence period, 30-second write interval and maximum change of 20 per ordinary write. Confirmed disk-swap churn retains its existing urgent path. These thresholds are a conservative starting point based on a host observation, not universal kernel recommendations or measurements of mouse latency.

If current swappiness cannot be read, zmem skips the write and reports `readback_unavailable` with a null request. A previously cached value cannot substitute for current readback. The implementation is in [swappiness.rs](../src/swappiness.rs); the [kernel's swappiness documentation](https://docs.kernel.org/admin-guide/sysctl/vm.html#swappiness) describes the underlying cost preference.

## Wait before adding another working set

A new compiler or model loader can worsen paging even while disk Swap still has space. `zmem run --class background` and `--class disposable` check available RAM and memory PSI before executing the target. The default wait is bounded at 60 seconds, with exit code 75 if conditions do not recover. Interactive commands bypass admission.

Use `--wait-timeout 300` for a longer batch window or `--wait-timeout 0` for a single immediate attempt. The launcher emits JSON events on stderr while waiting, rechecks conditions before admission, and spaces same-user starts. The [admission guide](background-admission.md) explains the thresholds and recovery rule. This gate controls new explicitly submitted work; running-task budgets remain the daemon's responsibility.

Keep a persistent Cargo target directory for repeated checks. A fresh temporary target directory discards that directory's compiled-artifact reuse, while excessive concurrent compiler working sets can turn CPU work into paging stalls. Admission and cache reuse address these separate costs. Neither requires a permanent one-core limit on ordinary builds.

## Protect desktop applications

The daemon recognizes ChatGPT, Chrome/Chromium, Firefox and WeChat in independent user application scopes, including nested graphical slices. It also recognizes the explicit `zmem-interactive-<pid>.scope` created by `zmem run --class interactive`. Generic `node`, `python` and `electron` processes do not qualify by name. A browser inherited by a terminal or remote-control scope does not enlarge that hosting scope's protection budget. This classification does not inspect the focused window or window titles.

The planner requests up to 512 MiB of observed memory for each selected interactive scope and propagates its allocation through the cgroup ancestors. The total ceiling remains `min(RAM / 8, 2 GiB)`. Each user initially divides its share 3:1 between session and application scopes; genuinely unrequested capacity can be borrowed by selected scopes in that same user's other category. Both categories retain the original split when both need their full shares. Root's explicitly marked interactive scopes share the existing system ceiling, at most one eighth of the total budget, with selected system services.

The interactive launcher makes an initial 512 MiB `memory.low` request on its scope. Complete ancestor planning requires a running daemon with `protect_sessions=true`; normal reconciliation runs every 15 sampling cycles, about 30 seconds with the default interval. The daemon retains nonzero external settings and applies the shared ancestor ceilings. `memory.low` is a best-effort reclaim preference, not preallocated RAM, a hard reservation or a promise to prevent all swapping. Inspect the leaf and every ancestor, together with `controls.protection_complete` and `protection_errors`, when checking whether protection was applied.

## Make user-session I/O controls available

`zmem run --class background -- COMMAND` requests a lower CPU and I/O weight. For a user scope, its I/O controller must be available through the user manager and its ancestors. A requested `IOWeight` value alone does not prove that a kernel control exists or that the storage scheduler uses weights.

The installer prepares a separately owned `user@.service` drop-in that adds I/O delegation and accounting. The addition preserves the existing delegated controller list. Its ownership record is separate from the original installation backup, so an upgrade does not change the old backup format. Existing foreign files and later external edits remain preserved during installation and rollback.

An already running user manager may retain its earlier controller availability. Follow the install diagnostic; a user-manager refresh can retain running application processes:

```sh
systemctl --user daemon-reexec
```

Confirm the relevant scope's actual `cgroup.controllers`, `io.stat`, `io.weight`, and any configured `io.max` afterwards. The installer does not automatically refresh every user's manager. The [systemd resource-control manual](https://www.freedesktop.org/software/systemd/man/latest/systemd.resource-control.html) explains delegation and hierarchical activation.

## Bound heavy maintenance separately

Filesystem deduplication has a storage workload and its own I/O budget. KSM has a memory-scanning workload and zmem's separate registration and pressure gates. Neither operation should be treated as free background work.

Per-service `IOReadBandwidthMax` and `IOWriteBandwidthMax` can bound an observed storage scanner while retaining its process and progress. Select every physical backing device that the service accesses and read back each actual `io.max` entry. A limit on the root device leaves a separate home-data device unrestricted. Compare device-specific `io.stat` deltas with `lsblk` and stable device identifiers before configuring the service. Unlike a relative weight, a bandwidth cap also limits an otherwise idle device; that is a throughput tradeoff, so zmem does not install one arbitrary cap on all applications.

The [0.4.2 acceptance record](acceptance-0.4.2.md) records desktop application protection, full ancestor-chain readbacks and the scanner's two-device I/O limit repair. It includes the final installed binary's identity and a 30-second natural-workload observation. The earlier [0.3.1 acceptance record](acceptance-0.3.1.md) separates the initial filesystem-scanner mitigation from the zmem policy regression tests. These are memory and I/O observations, not input-latency benchmarks or a guarantee that every source of desktop stutter has been removed.

