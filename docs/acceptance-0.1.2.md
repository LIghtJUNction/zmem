# zmem 0.1.2 acceptance

Validated on 22 November 2026 (Asia/Taipei), on the existing Arch Linux x86_64 workstation with kernel 7.3.5-arch1-1.

The host candidate was built before the version-only release bump and therefore reported 0.1.1. Its production Rust code is the code committed with this note. The bump changes package and website versions.

## Reproduced systemd interaction

A fresh, empty user slice was created by running `/bin/true` in a scope. After writing `268435456` to the parent's `memory.high`, starting a second scope reset that value to `max`. The cgroup device and inode were unchanged, and systemd's configured `MemoryHigh` was unlimited. The temporary slice was then stopped and removed.

That reset made 0.1.1 preserve the change as an external write and stop adjusting the aggregate background limit. The fix gives the dedicated `zmem.slice/zmem-background.slice` an explicit ownership rule: when its original limit was `max`, an unlimited reset of the same cgroup may be followed by another policy write. A successful write emits `managed_background_limit_reapplied`. Finite external values, finite original baselines, other cgroups and `memory.low` keep external-change protection.

## Checks performed

| Check | Observed result |
| --- | --- |
| Rust tests in the release profile | 32 passed; the 2 privileged fixtures remained ignored in the ordinary run. |
| `kernel_cgroup_rounding_and_restore`, run as root | Passed: actual page rounding, consecutive writes, six-column journal agreement, restoration, finite external-write preservation and legacy five-column recovery. |
| `kernel_managed_background_reset_and_restore`, run as root | Passed: finite limit, same-object reset to `max`, new finite limit, journal agreement, restoration to `max`, then preservation of an external finite limit. |
| Bump regression fixtures | 2 passed: a stale HTML fallback and the version script advance together; a missing HTML version marker fails before changing files. |
| Strict Clippy and `no_std` library check | Passed for the production fix before the additional ignored kernel fixture was appended. The production code did not change afterward. The formal release also checks Clippy against the complete tagged source. |
| Live user-scope smoke check | Two `/bin/true` scopes completed. At those samples the machine was healthy, the policy requested 100%, the parent correctly remained `max`, and the daemon reported no errors. This does not substitute for the finite-limit kernel regression above. |

The two privileged tests ran serially from the compiled release test executable. They required a stopped daemon, its controller lock and an absent control journal. The managed-slice fixture also refused to run if the system `zmem.slice` already existed. Neither fixture contained a process, and only the fresh fixture's subtree had the memory controller enabled. Both fixtures and their journals were removed after success.

The daemon was restarted after the tests. The existing `/etc/zmem/zmem.conf` hash was unchanged, every previously active Swap path remained active, and the fresh daemon status contained no errors.

An additional release-profile lint repeat on the workstation was cancelled while waiting behind the machine's serial compiler lock; it produced no lint finding. The earlier strict lint result covers all production changes, and the formal release workflow validates the complete source independently.

## Scope of this evidence

No extra whole-machine memory-pressure test was imposed during this patch validation. The earlier bounded-workload, disposable-limit and rollback results remain recorded in [0.1.0 acceptance](acceptance-0.1.0.md); the original upgrade and page-rounding evidence is in [0.1.1 acceptance](acceptance-0.1.1.md). Those historical measurements are not presented as rerun 0.1.2 benchmarks.

Cold boot, multi-day stability, universal application checkpointing and before/after application-throughput comparisons were not validated in this session.
