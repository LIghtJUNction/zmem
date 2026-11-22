# Swap lifecycle and explicit memory deduplication

zmem 0.3.0 adds automatic maintenance of its own Swap files and scanning control for explicitly registered KSM workloads. These mechanisms address different resources: Swap maintenance returns eligible disk allocations, while KSM asks the kernel to share identical anonymous memory pages. Filesystem deduplication of active Swap is outside this design.

This guide describes the current implementation and its limits. The [0.2.0 host acceptance record](acceptance-0.2.0.md) remains evidence for that earlier version and its recorded observation windows; it is not a performance test of these additions.

## Automatic Swap maintenance

### Growth, cleanup and retirement

| Operation | When it may run | What it does |
| --- | --- | --- |
| Growth | The existing memory/Swap policy requests headroom and its pressure, IO, cooldown, disk-space and capacity gates permit it. | Requests a full configured chunk, 2 GiB by default, capped by remaining capacity and filesystem reserve. |
| Cleanup | The controller is not critical, IO is uncongested and the immediate worker check has sufficient RAM reserve. | Attempts to remove one valid owned file without calling `swapoff` or migrating its pages. The kernel preserves a file that is still active or draining. |
| Active retirement | The policy has sustained healthy headroom, and a fresh worker sample passes whole-file RAM and Swap checks. | Deactivates one eligible owned file, verifies its identity and inventory again, then attempts ordinary unlink. |

Maintenance runs only when the daemon has no other worker. Growth takes priority, followed by eligible managed-background reclaim. There are at least 60 seconds between maintenance attempts, including failed or deferred attempts; each attempt selects at most one file. Candidate selection rotates, so a deferred candidate need not monopolize later attempts. A busy machine can defer maintenance indefinitely. This is a maximum action rate, not a promise to free disk space every minute.

The maintenance scope is the configured `swap_dir`, under `/var/lib/zmem`. Candidates must have zmem's exact `zmem-NNNNNN.swap` name, be root-owned private regular files with one link, and retain their observed file identity. Symlinks, unrelated files and arbitrary adopted swap devices are not maintenance candidates. Directory traversal is bounded; exceeding the scan limit preserves the files and reports an error. See [`maintain_swap` and its file checks](../src/linux.rs) and the [maintenance scheduler](../src/swap_lifecycle.rs).

### Cleanup without draining Swap

Cleanup handles retained allocations, including files whose preparation or activation was incomplete. It skips an exact canonical path already listed in the fresh Swap inventory. For the remaining candidates, an absent path does **not** establish inactivity: an active file created in an older service mount namespace may appear under a different alias.

The worker therefore verifies the pinned file identity and performs only an ordinary unlink. Linux's active-swap inode guard makes that final operation refuse an active or draining file. A busy or protected result is recorded as a deferral. Cleanup does not truncate the file, punch holes in it, rewrite its contents, rename it away or apply reflinks or filesystem deduplication.

Let `R` be the effective available-memory reserve. The cleanup gate requires a valid memory sample, available RAM of at least `R / 2`, memory pressure below the critical threshold, IO full PSI at or below 1%, and no other detected IO congestion. The controller also requires its memory state to be noncritical. This deliberately differs from active retirement: cleanup itself does not bring swapped pages back into RAM.

### Retirement reserves the entire file

The default policy requires 600 seconds of continuously healthy, comfortable conditions before considering active retirement, and at least 600 seconds since the last growth decision. The worker then samples again; earlier permission does not bypass current pressure checks.

For a candidate whose charged size is `B = max(logical length, allocated blocks)`, the immediate checks include:

| Check | Required condition |
| --- | --- |
| Memory PSI | `some ≤ 2%` and `full ≤ 1%` |
| IO PSI | `full ≤ 1%`, with the policy's IO-congestion gate also satisfied |
| Available RAM | At least `2 × R + B` |
| Free Swap | After subtracting `B`, at least `swap_free_target_mib`, 4 GiB by default |
| Known current usage | A candidate listed under its exact path with nonzero use is skipped. |

Subtraction or addition that cannot be represented safely defers the operation. The full `B` bound matters even for a currently empty file: new pages can be assigned before `swapoff`, and a legacy alias may hide reliable per-file usage. zmem does not infer a safe migration cost from a matching basename or file size.

After a successful deactivation, or a confirmed kernel `EINVAL` result, the worker may proceed to the same identity, inventory and kernel-guarded unlink checks. `EINVAL` is not itself proof of inactivity: a concurrent deactivation can still be draining pages. An unconfirmed helper result or another failure preserves the allocation. The host helper uses the pinned file identity in PID 1's mount namespace; [the implementation](../src/linux.rs) distinguishes syscall errno from setup, protocol and timeout failures.

### Serialization and accounting

All zmem Swap mutations share one protected lock, including allocation, activation, retirement and removal. The descriptor is inherited by allocation commands and syscall helpers. If a helper outlives a worker timeout, it retains exclusion until its actual exit; closing the parent's descriptor does not release the child's lock. This serializes zmem operations. Unrelated administrative tools are not assumed to participate, so identity rechecks and kernel guards still matter.

Retained inactive or incomplete owned files remain part of the disk-Swap budget. Each is charged the larger of logical and allocated size. Exact canonical active entries are counted once; uncertain aliases can be charged separately until identity is established. This conservative accounting can temporarily leave less growth room than a pathname-only estimate.

Read decisions and outcomes with:

```sh
zmem status --json
journalctl -u zmem.service
```

The journal reports `swap_maintenance_idle`, `swap_maintenance_deferred` or `swap_file_removed`. A removal event identifies the mode, logical bytes, allocated bytes and the kernel-guarded unlink result. Those allocated bytes describe the removed file; they are not a measured net `df` gain or a responsiveness improvement. Concurrent disk activity and filesystem accounting can change the observed free-space total.

## Explicit memory deduplication

### Register at launch

Choose trusted workloads with repeated anonymous data, then register them explicitly:

```sh
zmem run --class background --dedup -- YOUR_TRUSTED_COMMAND
zmem run --class disposable --max-memory 2048 --dedup -- YOUR_TRUSTED_BATCH_COMMAND
```

`background` uses the existing managed memory-budget policy. `disposable` also accepts an explicit hard limit, where exceeding the limit can terminate the job. `--dedup` is rejected for the `interactive` class. Default build routing through `zmem run --class background` does not add `--dedup`.

The [launch helper](../src/dedup.rs) enables `PR_SET_MEMORY_MERGE` with value 1, verifies it with `PR_GET_MEMORY_MERGE`, and then executes the original command. It does not silently run an unregistered command if registration is unsupported, denied or cannot be verified. The kernel registration survives the invocation's fork/exec path; the [Linux launcher](../src/linux.rs) places it in a marked managed scope so the daemon can discover it.

The configuration default is:

```ini
dedup_enabled = true
```

This permits scanning control for registered tasks. It does not register other applications. Set it to `false` in the service configuration and restart `zmem.service` to disable zmem's scanning policy. Changing this setting does not force an unmerge, remove the process registration of an existing task, or disable an externally owned KSM controller.

### A shared matching domain

KSM's matching domain is system-wide, subject to the kernel's existing settings. UID and cgroup boundaries do not create separate deduplication trust domains. When zmem starts an otherwise unowned scanner, it can also scan eligible memory registered by other software. The `--dedup` marker identifies the jobs zmem registered and observes; it does not restrict the kernel's global matching to those jobs.

The kernel merges eligible identical anonymous pages and copies a shared page when a process writes to it. This can save memory, but scanning and later writes have costs. It does not merge file-cache pages or deduplicate the contents of active swap files on disk. Choose registration based on whether the workloads may share this matching domain and whether their observed benefit justifies scanning. See the [kernel KSM documentation](https://docs.kernel.org/admin-guide/mm/ksm.html).

### Pause first; resume after stable observations

The daemon takes ownership only of stopped, compatible KSM controls. Existing unowned scanning, an external advisor or a detected external control change produces `externally_managed`; zmem does not overwrite that controller's settings. Controls are journaled, compared before writing and read back afterward.

For controls it owns, the policy pauses scanning when CPU, memory or IO is busy; required observations are missing; task coverage is incomplete or stale; no registered tasks remain; or dedup control is disabled. Pause means `run=0`, which preserves existing merged pages. zmem never uses `run=2`, `PR_SET_MEMORY_MERGE=0` or a process-unmerge call as its response to pressure. Restoring owned scan controls also pauses scanning before changing their batch settings.

The current busy gates use PSI `avg10`: CPU `some ≥ 5%`, memory `some ≥ 1%` or `full ≥ 0.2%`, and IO `full ≥ 1%`. Memory must additionally satisfy the daemon's healthy policy and available-RAM reserve; the IO gate includes detected paging congestion. PSI is waiting time, not CPU utilization. Missing observations are not treated as a quiet machine.

Starting or resuming scanning requires 30 seconds of stable, complete observations. A sampling gap beyond the governor's allowance, a clock reversal or a failed write resets that stability history. The fixed settings are `pages_to_scan=128` and `sleep_millisecs=100`, applied while stopped before `run=1`. These settings bound a scan batch and its sleep interval; they are not a hard CPU quota or a measured maximum CPU percentage. See the [governor](../src/dedup.rs) and [adapter](../src/dedup_adapter.rs).

### Observe coverage and actual state

The daemon refreshes registered-task discovery every 10 seconds. Discovery checks managed scope membership and stable PID identity around each process's KSM observation. It is bounded by process and task limits and does not read command lines, environments or memory contents. Enumeration limits, permission failures or malformed observations make coverage incomplete. `status --processes` adds the ordinary fresh process list; it does not replace the daemon's cached dedup discovery with a new scan.

| JSON field | Interpretation |
| --- | --- |
| `dedup.state`, `dedup.target_state`, `dedup.reason` | Observed control state, desired next state and the decision reason. A requested pause is not reported as completed if the kernel still reports scanning. |
| `dedup.control_owned`, `dedup.write_succeeded` | Current validated ownership and the result of a write attempted in this sample. `null` means no write result is available. |
| `dedup.registered_tasks`, `dedup.tasks` | Observed registered process leaders and their PID/start-time identities, cgroups and available KSM fields. |
| `dedup.task_observation_age_ms`, `dedup.task_refresh_interval_ms` | Age and scheduled refresh interval of dedup discovery, separate from the report's sampling age. |
| `dedup.coverage_complete`, `dedup.truncated`, `dedup.errors` | Whether discovery completed within its limits, and why a partial result cannot establish complete coverage. |
| `ksm.run`, `ksm.pages_to_scan`, `ksm.sleep_millisecs`, `ksm.advisor` | Observed global kernel settings, rather than assumed policy values. |
| `ksm.general_profit_bytes` | A signed kernel estimate for system-wide KSM memory benefit, including accounting overhead. It may be negative and is not attributable solely to zmem. |
| `ksm.pages_sharing`, `ksm.full_scans`, other optional counters | Available kernel counters. `null` means unknown or unsupported; it is not an observed zero. |

`idle` with `no_registered_tasks` is expected until a compatible task is launched and discovered. `warming` indicates that the stability interval has not yet permitted scanning. `paused_pressure` includes a reason explaining pressure or incomplete observations. `unavailable` indicates incomplete or unsupported KSM control observation. Read actual controls, coverage and errors with the state: a plan alone cannot establish that a write succeeded.

`status --live` and `plan` remain independent previews. They have no daemon task-discovery history and do not establish that scanning has started or that registered-task coverage is complete.

## Evidence and limits

The source supports automatic bounded maintenance and explicit KSM registration with observable pause/resume decisions. It does not guarantee a particular amount of freed disk space, memory savings, reduced SSD wear, desktop latency or application throughput. Repeated data may be absent, merged pages can later change, external KSM work contributes to global counters, and higher-priority memory work can defer maintenance.

The existing [read-only measurement procedure](../README.md#build-and-validate) can record comparable natural-workload windows. A future result must identify the tested version, executable, kernel, workload, KSM ownership and actual task coverage. It should assess latency and useful task progress alongside memory profit, scanning activity and paging. The 0.2.0 measurements do not supply these results for 0.3.0, and this guide claims no new performance measurement.
