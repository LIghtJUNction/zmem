# zmem 0.1.0 acceptance

Validated on 2026-11-22 (Asia/Taipei), Linux 7.3.5-arch1-1, x86_64, Rust 1.100.0, 16,476,700,672 bytes of physical memory. These measurements describe one active development workstation and the workloads below.

## Correctness and host behavior

| Check | Observed result |
| --- | --- |
| Rust tests | 27 passed: 13 policy scenarios and 14 configuration, parser, JSON, installer and control-boundary tests. |
| Portable policy core | cargo check --no-default-features --lib passed. The policy has no allocator and no unsafe code. |
| Compiler checks | cargo fmt --check and cargo clippy --all-targets -- -D warnings passed. |
| Live takeover | The existing zram device and all three used Swapspace files remained active. |
| Controller ownership | zmem became enabled and active; Swapspace, nohang-desktop and systemd-oomd became inactive and masked. |
| Background throttling | A managed background scope was given a 64 MiB high watermark. A 112 MiB allocation completed its data-integrity check; the cgroup recorded 619 high-watermark events. |
| Disposable budget | A disposable scope received 96 MiB RAM and 96 MiB Swap maxima, with memory.oom.group=1. For the hard-limit test, the soft high watermark was explicitly removed; a 300 MiB allocation was terminated within this scope. |
| Btrfs Swap growth | A 64 MiB owned swap file was created in a dedicated Btrfs subvolume and activated. Existing Swap remained active. |
| Conservative shrink | The busy host's memory PSI caused the shrink request to be deferred. Used or pressure-sensitive Swap was preserved. |
| Recovery and reinstallation | zmem rollback restored the three original active controllers without draining Swap. Installing zmem again succeeded and retained the original devices. |
| Unprivileged inspection | zmem status --json read the daemon's current status successfully. |
| Website | Chinese and English at 1440, 390 and 320 px; copy, language persistence, three synthetic scenarios and horizontal-overflow checks passed. Screenshots were visually reviewed. |

The IO controller was not delegated to the user's test scope on this host, so the IO weight file was absent. Memory limits and CPU weight were present. The background test exercised kernel throttling and data integrity; it did not measure whole-desktop input latency.

## Observed overhead

The installed optimized binary was **750,424 bytes**. The daemon used one thread, with **2,884 KiB RSS** and a 4,016 KiB virtual size during the recorded sample. Across a 10.028-second window, its CPU tick count did not advance at the host's accounting resolution. This is a measurement below the sample's resolution, not a zero-CPU guarantee.

Twenty cached JSON status invocations took a median **1.272 ms** and a maximum **1.528 ms** on this host. These are CLI invocation timings. They do not quantify speedup over another manager or latency under every workload.

## Operational scope

The active zram layout was preserved during migration. The configured 50%-of-RAM zram layout will be used when zmem initializes an unused device after a normal boot; this acceptance did not reboot the working machine. Kernel paging, compression and the final OOM mechanism remain responsible for the underlying memory operations.

The daemon applies memory.low protection through the user/session hierarchy and to SSH, and controls explicitly managed background scopes. Unknown applications do not receive arbitrary hard limits or automatic termination. Application checkpoint/restore, general anonymous-memory deduplication, energy optimization and multi-day endurance measurements are outside this release's validated behavior.

The reported zram savings describe compression already present in the active device. They are not incremental savings attributed to installing zmem. A comparative benchmark needs the same workload, storage, kernel and starting memory state.

## Reproduction

    cargo test --locked
    cargo check --locked --lib --no-default-features
    cargo clippy --locked --all-targets -- -D warnings
    cargo build --release --locked
    zmem doctor --json
    zmem status --json --processes
    zmem run --class background -- YOUR_BUILD
    zmem run --class disposable --max-memory 96 -- YOUR_DISPOSABLE_TEST

The lifecycle acceptance used the installed manager's recorded recovery baseline and returned the machine to zmem. Raw host logs and screenshots are kept outside Git; source history contains this textual record.
