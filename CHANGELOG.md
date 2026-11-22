# Changelog

## 0.4.1 — 2026-11-22

- Use architecture-correct Linux directory and no-follow open flags. AArch64 uses different values from x86_64; the 0.4.0 formal AArch64 validation exposed the mismatch before crate or release-asset publication.
- Share the flags across admission, installation, swap helpers and I/O delegation. Preserve the existing file-identity and regular-file checks, including the distinct `O_PATH | O_NOFOLLOW` behavior for a symlink descriptor.
- Test actual directory/file/symlink opening behavior and expected kernel errors on each release runner. The admission thresholds and process-preserving policy are unchanged.

## 0.4.0 — 2026-11-22

- Check memory headroom before starting explicitly submitted background and disposable commands. Use available RAM and memory PSI, with strict handling of missing observations and a three-second stable recovery period after pressure.
- Add `--wait-timeout 0..3600`, defaulting to 60 seconds. A timeout returns 75 without starting the target; interactive commands bypass admission. Preserve inherited signal semantics and cancel eligible waiting invocations on SIGINT or SIGTERM.
- Pace same-user admission reservations through a bounded, identity-checked runtime record and a short lock. Recheck conditions before granting a reservation; do not hold the lock for the running workload's lifetime. This does not promise FIFO order or predict peak job memory.
- Preserve literal argument bytes through the normal and dedup launch paths. Detect systemd-run compatibility before disabling environment expansion on versions that support that option.
- Emit machine-readable admission reasons on stderr while keeping stdout for the target. Expose wait, timeout and coverage semantics through `zmem schema` and the background-admission guide.
- Retain existing running-task budgets, Swap state and process-preserving policy. Admission adds no permanent CPU quota, global bandwidth cap, application interception or automatic process termination.

## 0.3.1 — 2026-11-22

- Add a measured reclaim-latency guard to automatic swappiness selection. Sustained memory stalls together with direct scanning and anonymous refaults cap optimistic preferences at 60 while retaining lower actual values, existing hysteresis and confirmed disk-swap churn priority.
- Skip swappiness writes when current kernel readback is unavailable; expose a null request and `readback_unavailable` instead of writing a previously cached value.
- Prepare additive I/O delegation and accounting for user managers through a separately owned drop-in. Keep the original installation backup format readable and preserve foreign files or later external edits during upgrades and rollback.
- Publish the drop-in with create-only semantics and a durable ownership record; recover interrupted preparation and undo only configuration created by the failed installation.
- Add read-only user I/O-controller diagnostics to `doctor --json`. Distinguish an installed default from an existing manager that still needs a refresh, without automatically restarting user sessions or claiming that declared I/O weights are effective.
- Record the local background-scanner investigation, effective bandwidth limits and observed memory-pressure changes separately from policy regression tests; no mouse-latency speedup is inferred.

## 0.3.0 — 2026-11-22

- Schedule automatic maintenance of owned Swap files, at most one candidate per attempt and at least 60 seconds between attempts. Give growth and managed-background reclaim priority.
- Clean retained inactive allocations without draining Swap. Recheck file identity and inventory, use ordinary kernel-guarded unlink, and preserve active or draining files even when their canonical pathname is absent from `/proc/swaps`.
- Retire eligible owned Swap only after sustained healthy headroom, 600 seconds by default, with immediate whole-file RAM and remaining-Swap checks. Skip known used files and never infer inactivity from a missing alias or `EINVAL` alone.
- Serialize zmem Swap allocation, activation, retirement and removal with a shared lock inherited through the actual lifetime of allocation and syscall helpers, including helpers that outlive a timeout.
- Add explicit `zmem run --class background|disposable --dedup -- COMMAND` registration, with process KSM enable/readback before executing the command and an explicit error when registration is unsupported or denied.
- Manage compatible, previously stopped KSM controls for registered tasks by default. Pause owned scanning under CPU, memory or IO pressure and require 30 seconds of stable observations to resume; use 128-page batches and a 100 ms sleep rather than claiming a hard CPU quota.
- Preserve externally running KSM, external advisors and changed controls. Pause with `run=0`; never force unmerging with `run=2` or `PR_SET_MEMORY_MERGE=0`, and never apply filesystem deduplication to active Swap.
- Refresh bounded registered-task discovery every 10 seconds and report observed/target scan state, task age, coverage, ownership, write results and signed system-wide KSM profit. Keep missing metrics `null` and identify KSM's system-wide matching domain.
- Repair interactive protection after systemd reload resets an owned `memory.low` ancestor or service to its original zero baseline. Reapply only for the same cgroup identity and preserve nonzero external values or baselines; use `protect_sessions=false` for a persistent opt-out.
- Retain the last protection check's errors and `protection_sampled_at_unix` between checks so an ordinary sampling cycle cannot hide an incomplete protection operation.
- Document the new lifecycle and dedup mechanisms separately from the 0.2.0 host observations; no performance improvement is inferred from the earlier version's measurements.

## 0.2.0

- Separate memory pressure from IO congestion and select swappiness from measured file-page refaults, disk-swap reads and useful zram headroom, with hysteresis and bounded changes.
- Keep `status --processes` on the verified daemon report while adding a fresh process list. Label `--live`, `plan` and missing-cache fallbacks as independent previews with unprimed rates.
- Report observed kernel controls, sampling provenance, managed-background coverage, optional per-process cgroup membership and incomplete protection operations.
- Replace fixed half-RAM background-job limits with aggregate byte budgets based on occupied memory, available headroom and the RAM reserve; empty groups consume no share.
- Tighten background limits before relaxing them, confirm changes through kernel readback, and restore capacity gradually when spare headroom allows progress despite throttling PSI.
- Expose requested and observed background budgets and per-group limits; retain `plan.background_high_percent` as a pressure-severity hint.
- Discover recognized interactive and remote-access processes in nested cgroups and distribute a bounded protection budget through their actual hierarchy.
- Restrict proactive reclaim to populated background groups with eligible clean file pages, record observed progress separately from requests, and back off after unproductive work.
- Periodically remove recovery records for vanished or replaced cgroups while preserving live baselines and uncertain metadata errors.
- Reject oversized reads before restoring controls, retaining the complete recovery journal on failure.
- Recover a failed installation from a separate transaction snapshot of the immediately previous version; keep the first-install baseline for explicit `rollback`.
- Activate and deactivate Swap through util-linux `nsenter` in the host mount namespace, using a pinned, verified file identity for the swap syscalls. Add the service capabilities needed to access PID 1's namespace and root while retaining the other service restrictions.
- Request a full configured growth chunk, 2 GiB by default and bounded by the disk-swap cap, so minor headroom deficits do not consume a new kernel swap slot for each small file. Preserve the existing pressure, IO and cooldown gates.
- Count inactive and incomplete owned files in the disk-swap budget using the larger of logical length and allocated blocks; avoid double-counting exact canonical active paths while conservatively charging unverified aliases separately.
- Preserve typed kernel swap errors separately from setup, protocol and timeout failures. Remove a failed new activation file only after a confirmed non-busy syscall failure, unchanged identity and a fresh inventory confirming its canonical path is inactive; retain uncertain files and charge their allocation to the budget.
- Add a read-only natural-workload probe for scheduler lateness, PSI, paging, managed-background activity and optional desktop IPC round trips.
- Add regression coverage for status provenance, paging preferences, nested protection, reclaim feedback, control identity cleanup, oversized journal recovery, installation snapshots, the swap-helper protocol, configured growth batches, retained allocations and failure-cleanup decisions.

## 0.1.1

- Normalize cgroup memory limits to the running kernel's page size so the daemon can continue adjusting its own limits.
- Preserve real external changes and track both sides of pending control writes.
- Recover five-column 0.1.0 control journals during upgrades without losing the old daemon's final records.
- Add a privileged empty-cgroup regression for consecutive writes, rollback, external changes and legacy recovery.
- Verify live 0.1.0 takeover with all 13 recorded control baselines and active swap preserved.

## 0.1.0

- Introduce an allocation-free `no_std` memory policy core and a Linux control daemon with no third-party Rust dependencies.
- Combine available memory, PSI and swap activity in bounded policy decisions with cooldowns and recovery hysteresis.
- Adopt active zram and disk swap, manage bounded disk-swap capacity, and initialize zram at boot.
- Manage background memory budgets through cgroup v2 and expose status, plans and command contracts as JSON.
- Provide service installation, existing-manager replacement and configuration rollback.
- Add the Ink-style bilingual website and a manual version-bump workflow that publishes Linux x86_64 and ARM64 archives, GitHub releases and the crate.
