# Changelog

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
