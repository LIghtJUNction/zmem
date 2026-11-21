# Changelog

## 0.1.0

- Introduce an allocation-free `no_std` memory policy core and a Linux control daemon with no third-party Rust dependencies.
- Combine available memory, PSI and swap activity in bounded policy decisions with cooldowns and recovery hysteresis.
- Adopt active zram and disk swap, manage bounded disk-swap capacity, and initialize zram at boot.
- Manage background memory budgets through cgroup v2 and expose status, plans and command contracts as JSON.
- Provide service installation, existing-manager replacement and configuration rollback.
- Add the Ink-style bilingual website and a manual version-bump workflow that publishes Linux x86_64 and ARM64 archives, GitHub releases and the crate.
