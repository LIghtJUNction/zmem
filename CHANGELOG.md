# Changelog

## Unreleased

- Adapt the aggregate background budget to managed usage and available memory above a comfortable reserve, with a shared progress floor, a reserve ceiling and usage-weighted shares for populated groups. Empty groups consume no quota.
- Restore background capacity gradually when measured spare headroom allows it, even if PSI remains elevated. Keep pressure severity strict until stable recovery.
- Enable memory accounting for background launches and remove their permanent per-job half-RAM `MemoryHigh`; explicit disposable limits remain in place.
- Expose managed background usage, proposed budgets, sampled kernel limits and process cgroup classification. Incomplete background samples retain existing limits and skip background limit changes and reclaim selection.
- Protect terminal scopes below nested slices and their intermediate ancestors within the existing shared app protection pool, preserving larger external protections.
- Honor busy Swap/IO policy before zram and profile preferences when selecting swappiness.
- Periodically remove recovery records for vanished or replaced cgroups while preserving live baselines and uncertain metadata errors.
- Reject oversized reads before restoring controls, retaining the complete recovery journal on failure.
- Add regression coverage for swappiness thresholds, profile preferences, control identity cleanup and oversized journal recovery.

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
