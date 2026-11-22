# zmem

**Memory, with room to work.** A small Linux memory and Swap controller from [Project Z](https://github.com/LIghtJUNction/project-z).

[Website](https://lightjunction.github.io/zmem/) · [Releases](https://github.com/LIghtJUNction/zmem/releases) · [crates.io](https://crates.io/crates/zmem-manager) · [Rust API](https://docs.rs/zmem-manager) · [Background admission](docs/background-admission.md) · [0.4.2 acceptance](docs/acceptance-0.4.2.md) · [0.4.1 acceptance](docs/acceptance-0.4.1.md) · [Responsiveness guide](docs/responsiveness.md) · [0.3.1 acceptance](docs/acceptance-0.3.1.md) · [Swap and dedup guide](docs/swap-lifecycle-and-dedup.md) · [0.3.0 acceptance](docs/acceptance-0.3.0.md) · [0.2.0 host acceptance](docs/acceptance-0.2.0.md)

zmem observes memory and I/O pressure, waits before starting new managed background work under pressure, and gives running background jobs a bounded memory budget. It brings zram setup, dynamic disk swap, automatic cleanup, opt-in memory deduplication and structured inspection into one tool. The Rust implementation has no third-party crate dependencies; the policy library uses `no_std` and makes no heap allocations.

## What it does

| Capability | Behavior |
| --- | --- |
| Pressure policy | Reports memory pressure and IO congestion separately, using available memory, PSI and paging activity with cooldowns and recovery hysteresis. |
| Swap management | Adopts active swap, grows within capacity and free-space limits, cleans retained owned files, and retires eligible owned swap after sustained recovery. |
| zram lifecycle | Keeps the current active device during migration and prepares zram on subsequent boots. |
| Explicit memory deduplication | Registers trusted background or disposable jobs with `--dedup`; pauses owned KSM scanning under CPU, memory or IO pressure. |
| Workload budgets | Adjusts aggregate cgroup v2 `memory.high` budgets to managed background usage and available headroom; disposable jobs retain explicit hard limits. |
| Background admission | Checks available RAM and memory PSI before a background or disposable command starts; waits for stable recovery and spaces starts by the same user. |
| Interactive protection | Assigns a bounded `memory.low` budget to recognized desktop applications, terminal and remote-access scopes, including explicit interactive workloads, through their actual cgroup hierarchy. |
| Reclaim responsiveness | Caps optimistic swappiness adjustments when memory stalls, direct reclaim and anonymous refaults occur together; skips writes when current kernel readback is unavailable. |
| User I/O controls | Prepares additive I/O delegation and accounting; reports whether the calling user's existing manager has the controller available. |
| Observability | Reports daemon state, actual kernel controls, managed-workload coverage and on-demand process usage in human-readable and JSON output. |
| Service migration | Saves prior service/configuration state, replaces existing management services, and offers rollback. |

The Linux kernel performs actual paging, compression, page merging and final OOM handling. zmem makes earlier policy decisions using those facilities. Application checkpoints, automatic recovery of computation progress and energy optimization remain future work. Deduplication requires explicit task registration and kernel support; it does not enroll every application or deduplicate active swap files on disk. KSM uses a system-wide matching domain, so choose only workloads whose data may share that domain.

The default policy preserves processes. It does not issue global cache drops or automatically kill arbitrary applications. A task explicitly started as disposable can be given a hard cgroup limit, where the kernel may terminate it if the budget is exhausted.

## Installation

Use a Linux system with systemd, PSI and cgroup v2. Swap-file preparation uses the host's swap utilities; activation and deactivation also require `nsenter` from util-linux. Btrfs hosts need `btrfs-progs`. The daemon needs root privileges for system-wide configuration and swap management. Read-only status and policy inspection can run without root.

The [swap helper](src/linux.rs) enters PID 1's mount namespace and root before calling the swap syscalls. It pins and verifies the target file's identity so a changed path cannot substitute a different file. The provided [service unit](packaging/zmem.service) grants `CAP_SYS_CHROOT` and `CAP_SYS_PTRACE` for this namespace/root access while retaining its other service restrictions.

### From crates.io

```sh
cargo install zmem-manager --locked
zmem doctor
zmem status --json
sudo "$(command -v zmem)" install --replace
```

`install --replace` is the explicit system migration operation. It preserves active swap, records the prior management configuration and installs the zmem service. Existing swap that contains data is adopted in place. Changes that require rebuilding zram take effect at a normal subsequent boot.

The installer also prepares I/O delegation for user managers without replacing their other delegated controllers. An existing session may need a manager refresh before user scopes can enforce I/O controls; `zmem doctor --json` reports the observed state. See the [responsiveness guide](docs/responsiveness.md#make-user-session-io-controls-available) for refresh and kernel-readback checks. The installer does not restart user sessions automatically.

Active Swap created in an older service mount namespace can retain an old alias in `/proc/swaps` until reactivation or a normal boot. Its capacity remains available. Maintenance never infers inactivity from a missing canonical pathname: cleanup leaves active files protected by the kernel, and retirement requires the full-file headroom checks described in the [lifecycle guide](docs/swap-lifecycle-and-dedup.md#automatic-swap-maintenance).

### Prebuilt Linux executable

Download the archive and its `.sha256` file for your architecture from [GitHub Releases](https://github.com/LIghtJUNction/zmem/releases). Verify the checksum, extract the archive and run its `bin/zmem` executable. The release contains `build.json` with source identity, architecture and build libc information. Source installation is available for distributions with an older libc than the published binary requires.

```sh
# In the directory containing the downloaded archive and checksum:
sha256sum --check zmem-*.tar.gz.sha256
tar -xzf zmem-VERSION-linux-ARCH.tar.gz
./zmem-VERSION-linux-ARCH/bin/zmem doctor
sudo ./zmem-VERSION-linux-ARCH/bin/zmem install --replace
```

Replace `VERSION` and `ARCH` with the downloaded release name. Supported release architectures are `x86_64` and `aarch64`.

### Service and rollback

```sh
systemctl status zmem.service
journalctl -u zmem.service
sudo zmem rollback
```

Stopping or upgrading the management daemon leaves active swap in place. Each installation captures a separate transaction snapshot; failure recovery uses it to restore the immediately previous binary, configuration and controller states. If recovery itself fails, the snapshot is retained and the error identifies its path. The explicit `rollback` command instead restores the original first-install baseline. Neither operation drains active swap into RAM. Keep the original configuration record until the new setup has completed a normal boot on your machine.

## Everyday use

```sh
zmem status
zmem status --json --processes
zmem status --live --json
zmem plan --json
zmem doctor --json
zmem schema
```

`status` reads the report of the active daemon after checking its process identity. It retains that daemon's policy history, measured paging rates, worker state and errors. `--processes` adds a current process list to the same report. The `observation` fields identify the source, sample age, stale reports and the separate process-sampling time.

Each requested process row includes its current `cgroup` and whether it belongs to a registered background subtree (`managed_background`). Both fields are `null` when unified cgroup membership cannot be read. Membership describes current placement: moving an existing process does not transfer its existing memory charges, so classify jobs at launch ([kernel memory ownership](https://docs.kernel.org/admin-guide/cgroup-v2.html#memory-ownership)).

`status --live` and `plan` take one independent sample and show an unapplied preview. They start with a new policy state; rates remain `null` while the sample is unprimed. If the daemon report is unavailable, ordinary `status` also returns an explicitly labelled independent snapshot with the cache error. `schema` describes this contract for scripts and AI tools.

Read the controls and coverage alongside the plan:

| Report field | Meaning |
| --- | --- |
| `memory_pressure`, `io_congested` | Separate signals for memory pressure and IO congestion. A busy storage device alone does not establish memory exhaustion. |
| `paging` | Measured sampling interval, file/anonymous refaults, direct scans and available swap-rate estimates. Availability flags and `null` values distinguish missing observations from zero activity. |
| `controls` | Requested and observed swappiness, its selection reason, actual page-cluster value, protection budget, protected-scope count, and the last protection check's timestamp and errors. |
| `managed` | Observed background groups, populated groups, task count, memory usage and currently eligible reclaim candidates. |
| `background` | Requested aggregate budget, actual limits read back from populated groups, per-group limits, and sampling/application completeness. |
| `dedup` | Observed and target scan state, decision reason, control ownership, registered tasks, task-observation age and coverage. |
| `ksm` | Actual KSM controls and available kernel counters, including the signed system-wide profit estimate. Missing observations remain `null`. |
| `plan.background_high_percent` | A pressure-severity hint retained in the policy contract. Read `background` for the byte budget actually requested and observed. |
| `protection_complete`, `coverage_complete` | Completeness of the protection operation and managed-background observations, inside `controls` and `managed` respectively. |

An empty background scope has no workload to throttle or reclaim. Use `zmem run --class background` to place a job under that policy; arbitrary existing applications are not automatically assigned a background budget. Protection counts cover recognized scopes, and completeness flags expose incomplete scans or control operations.

`background.requested_budget_bytes` describes the proposed aggregate budget; `background.actual_budget_bytes` sums the limits read back from populated groups. Check `plan_available` and `readback_complete` before interpreting a `null`: with complete information, it means no temporary requested ceiling or an unlimited observed group, respectively; otherwise the value is unavailable. Per-group `requested_high_bytes` and `actual_high_bytes`, together with `applied_complete`, expose retained external limits and partial application.

To observe policy over time before applying system changes:

```sh
zmem daemon --observe
```

Start background work with its classification from the beginning:

```sh
zmem run --class background -- cargo build --release
zmem run --class background --wait-timeout 300 -- YOUR_BATCH_COMMAND
zmem run --class interactive -- YOUR_COMMAND
zmem run --class disposable --max-memory 2048 -- YOUR_BATCH_COMMAND
# Explicitly register trusted, repeat-heavy memory workloads for KSM:
zmem run --class background --dedup -- YOUR_TRUSTED_COMMAND
zmem run --class disposable --max-memory 2048 --dedup -- YOUR_TRUSTED_BATCH_COMMAND
```

The `--max-memory` value is in MiB. The disposable class is for work that can be restarted if it exceeds its explicit budget. Starting a job inside a cgroup gives the kernel a consistent accounting boundary for the job's allocations.

Background and disposable commands wait for memory admission before executing. The default wait is 60 seconds; `--wait-timeout 0` makes one immediate attempt, and values up to 3600 allow a longer recovery window. A timeout returns exit code 75 without starting the target. A healthy first observation can pass immediately; after pressure, recovery must remain stable for three seconds. Launch reservations by the same user are spaced at least three seconds apart. Interactive commands bypass this gate. See the [admission guide](docs/background-admission.md) for thresholds, cancellation and JSON events.

Admission progress goes to stderr, leaving stdout for the target. The launcher preserves literal argument bytes and the caller's environment, working directory and standard input. It does not freeze an existing application or impose a permanent single-core or storage-bandwidth limit. Admission covers commands submitted through `zmem run`; it does not automatically intercept every terminal build or model loader.

`--dedup` enables process-level KSM registration before executing the selected command and verifies that registration. It is available for `background` and `disposable`, not `interactive`. A kernel that rejects or cannot verify registration produces an explicit error before the target command starts. Ordinary `run` commands and the default managed-build wrappers do not add this flag.

The default `dedup_enabled = true` permits the daemon to manage scanning for registered tasks. It does not register tasks by itself. When zmem owns the KSM controls, CPU, memory or IO pressure pauses scanning with `run=0`, preserving merged pages. Complete, fresh observations and 30 seconds of stable headroom are required before scanning resumes. The fixed scan settings are 128 pages followed by a 100 ms sleep; these are batch controls, not a hard CPU quota. Registration is refreshed through a bounded task scan every 10 seconds, with incomplete coverage reported explicitly.

KSM matching crosses process, user and cgroup boundaries, including eligible memory registered by other software. zmem leaves an already running external KSM controller, an external advisor or externally changed controls alone. It never writes `run=2` or `PR_SET_MEMORY_MERGE=0` to force unmerging. See the [dedup guide](docs/swap-lifecycle-and-dedup.md#explicit-memory-deduplication) for trust boundaries, pause conditions and status interpretation.

## Policy and resource limits

The policy core is a deterministic state machine. It receives a snapshot and a monotonic timestamp, then produces a bounded decision. The Linux adapter checks real ownership, kernel state, disk capacity and operation results before acting.

Swappiness expresses the kernel's relative preference for swapping anonymous memory versus reclaiming file-backed pages. zmem uses measured paging evidence to select it. Confirmed disk-swap read pressure lowers the preference; IO congestion alone does not trigger that reduction. When file-page refaults dominate alongside direct reclaim scans, disk-swap pressure is absent, and the reclaim-latency guard has not triggered, the desired value is 80, or 100 when zram has measured useful compression headroom. The guard takes precedence: it caps the value at 60 and preserves an already lower setting. Refaults count pages used again after eviction; they do not directly measure storage reads. Hysteresis and bounded steps govern changes, so the requested and actual values can differ from the desired value for a sample.

Interactive protection follows recognized processes into their real nested session and application scopes. ChatGPT, Chrome/Chromium, Firefox and WeChat qualify in independent desktop application units; terminal and remote-session descendants retain their existing smaller budgets. Explicit `zmem-interactive-<pid>.scope` workloads also participate. This is application classification, not active-window tracking.

Recognized input methods receive priority within their category's allowance. Their requests include observed swapped pages, capped at 256 MiB per scope, so an evicted input method retains room to restore its working set. The [input responsiveness guide](docs/responsiveness.md#keep-input-method-demand-visible-after-eviction) describes the shared-budget rules and the optional read-only Fcitx query probe. The [0.4.3 acceptance record](docs/acceptance-0.4.3.md) includes live kernel readback and input-query observations.

A shared `memory.low` budget, at most one eighth of physical RAM and at most 2 GiB, is allocated through the required ancestors and leaf scopes. Each recognized interactive app requests at most 512 MiB of its observed usage. Session and application categories retain their 3:1 starting shares; either category can use genuinely unrequested capacity from the other category of the same user. Shares are never transferred across users or assigned to unselected slices. The report records how many critical scopes received protection and whether the operation completed. Recursive protection in the kernel can also pass an ancestor's protection to a child whose local `memory.low` is zero; interpret the full hierarchy when inspecting a session. See the [responsiveness guide](docs/responsiveness.md#protect-desktop-applications) for launcher and daemon timing.

systemd can restore an owned ancestor or service's `memory.low` to its configured zero baseline during reload. That can interrupt the protection path even while a leaf retains a nonzero value. zmem reapplies its budget only when the cgroup identity is unchanged, the control was already owned, and both the original baseline and current value are zero. Nonzero external values and nonzero original baselines retain external-change protection. To disable this automatic protection, set `protect_sessions = false` in the service configuration and restart the service; a manual zero on a currently owned node is not a persistent opt-out. `controls.protection_sampled_at_unix` and `controls.protection_errors` retain the last protection check's time and diagnostics between checks, so an intervening ordinary sample cannot hide a failed protection operation.

Background jobs inherit a dynamic budget from their managed aggregate. `run --class background` adds no fixed half-RAM limit to each new job. During a pressure episode, the daemon calculates a shared byte budget from current managed usage, available RAM and the configured reserve plus a safety margin. Populated groups share that budget with a bounded allowance for progress; empty groups consume no share. Healthy policy removes the temporary restriction while preserving existing external limits.

Budget redistribution applies tighter limits before relaxing any group. A failed tightening blocks all relaxations in that batch; a failed relaxation stops the remaining relaxations. Incomplete sampling holds existing controls, and subsequent decisions use confirmed kernel readbacks, including partial application. When measured spare headroom is available, the budget can recover in bounded steps even while `memory.high` throttling keeps PSI elevated. This lets work regain capacity without waiting for its own throttling stalls to disappear first.

Proactive reclaim is limited to populated, managed background groups with a conservative clean-file-page budget. Empty groups and groups without eligible file pages are skipped. The worker records its requested bytes separately from observed proactive-reclaim counters or net memory reduction, then backs off when progress is absent or unavailable. A successful request write by itself is not reported as memory saved.

While the daemon is running, zmem owns the aggregate `memory.high` policy of its dedicated `zmem.slice/zmem-background.slice`, including the corresponding slice below a user manager. systemd can reset that slice to `max` when a new scope starts. If the original baseline was unlimited and the cgroup is still the same object, zmem reapplies its policy and records a `managed_background_limit_reapplied` event. A manual `max` write to this dedicated slice is therefore temporary. Finite external `memory.high` limits, finite original baselines and other cgroups retain the normal external-change protection; the separate owned-zero rule above applies to `memory.low`. Run jobs outside the background class when another tool should own their limits.

Recovery records for vanished or replaced cgroups are removed every 15 sampling cycles, or about 30 seconds with the default interval. Live control baselines and records with uncertain metadata errors remain intact. Reading a recovery journal larger than 1 MiB returns an error and retains the whole file for recovery.

When growth is needed, the policy requests a full configured chunk, 2 GiB by default, bounded by the disk-swap cap. Every active swap file consumes a kernel swap slot, so creating a new small file for each minor headroom deficit can exhaust slots while disk space remains available. Full chunks provide spare headroom and use fewer slots for a given capacity. Pressure, IO and cooldown gates still determine when growth is permitted; the Linux adapter checks remaining capacity and filesystem reserve before creating a file.

The disk-swap budget includes valid owned files in the configured swap directory even when allocation or activation is incomplete. Each file is charged the larger of its logical length and allocated blocks. Other active disk Swap also counts toward the cap; zram does not. An exact canonical active path is counted once. An uncertain old alias remains a separate charge, so unproven identity can conservatively reduce the room for another allocation.

After a failed new-file activation, cleanup requires a confirmed kernel syscall error other than `EBUSY`, unchanged file identity and a successful fresh Swap inventory that does not list the canonical path as active. Setup errors, timeouts, malformed helper replies, missing observations and changed identities preserve the file. Retained allocations continue consuming the disk-swap budget. Numeric syscall errors are distinguished from helper setup errors; text that merely says a file is busy does not establish its identity or authorize cleanup.

Automatic maintenance revisits owned files at most once per 60 seconds, one candidate per attempt, after higher-priority growth or reclaim work. Cleanup can remove an inactive retained allocation without calling `swapoff`. It requires noncritical memory pressure, no IO congestion and at least half of the configured effective RAM reserve. The final operation is an ordinary, identity-checked unlink; the kernel refuses it while the file remains active or draining. A missing `/proc/swaps` pathname or an `EINVAL` result alone never proves inactivity.

Active retirement has stronger requirements: the default policy waits for 600 seconds of continuously healthy conditions and at least that long after growth. Immediately before `swapoff`, the worker rechecks low pressure, available RAM of at least twice the effective reserve plus the candidate's entire capacity, and enough free Swap to preserve the configured target after subtracting that entire capacity. Known used files are skipped. The whole-file bound covers an old mount alias whose usage cannot be established from its name. All zmem allocation, activation, retirement and removal operations share a lock that remains held for the lifetime of their helpers, including a helper that outlives a timeout. Maintenance never truncates, punches holes in, overwrites or applies filesystem deduplication to active Swap.

| Default policy setting | Value |
| --- | --- |
| Available-memory reserve | Larger of 1 GiB or 10% of physical RAM, capped at total RAM |
| Automatic launch reserve | Policy reserve above, additionally capped at 25% of RAM; explicit reserves retain their meaning |
| Background admission wait | 60 seconds; `--wait-timeout` accepts 0..3600, timeout exits 75 before launch |
| Desired unused Swap headroom | 4 GiB when growth is needed |
| Growth batch | 2 GiB per permitted decision, bounded by remaining capacity |
| Disk Swap cap, including retained owned allocations | 32 GiB |
| Filesystem free-space reserve | 8 GiB |
| Swap growth cooldown | 30 seconds |
| Automatic maintenance cadence | At least 60 seconds between attempts; at most one file per attempt |
| Stable healthy time before shrinking | 600 seconds |
| Dedup task registration | Explicit `run --dedup`; daemon scanning enabled by default |
| Dedup recovery and scan settings | 30 stable seconds; 128 pages per batch, 100 ms sleep |

These are policy defaults, not a promise about application latency. A machine can run out of physical memory and swap, or spend time waiting for a slow storage device. zmem bounds its own actions and exposes the observed pressure so the tradeoffs remain visible.

Memory and swap quantities use binary units: one MiB is 1,048,576 bytes and one GiB is 1,073,741,824 bytes. PSI measures time spent waiting for a resource. In the library, 10,000 basis points means 100% of the measurement interval.

The [interactive website](https://lightjunction.github.io/zmem/#playground) illustrates a simplified policy decision using synthetic inputs. It runs entirely in the browser and does not connect to the daemon.

## Build and validate

```sh
cargo test --locked
cargo check --locked --lib --no-default-features
cargo build --release --locked
```

The public policy types can be embedded without the operating-system adapter. The `zmem` executable targets Linux; the `no_std` policy library does not depend on Linux.

The repository includes a read-only probe for observations during ordinary work:

```sh
python3 scripts/measure_responsiveness.py --duration 60 --output artifacts/before
# Repeat under the candidate policy for the same duration and a comparable workload.
python3 scripts/measure_responsiveness.py --duration 60 --output artifacts/after
```

It records scheduler wakeup lateness, memory/IO/CPU PSI, paging counters, zram data, daemon status and managed-background activity. `--desktop-probe` adds a bounded Hyprland monitor-query round trip when available; this measures process startup plus IPC, not frame latency. The probe does not create a load or change kernel settings. Unknown measurements remain `null`, and counter resets are excluded from rate calculations. Output files contain reproducible samples and a summary with owner-only permissions.

Natural-workload before/after observations describe the machine during those windows. Workload and cache changes remain possible causes of a difference. For a controlled comparison, identify the tested source, kernel, storage and workload, repeat comparable conditions, and assess latency together with task progress, pressure and paging activity. A lower used-memory number alone does not measure responsiveness.

The published [0.2.0 acceptance record](docs/acceptance-0.2.0.md) applies to its recorded binaries and observation windows. It does not validate the automatic lifecycle and dedup additions in 0.3.0 or establish a performance gain for them. The [lifecycle and dedup guide](docs/swap-lifecycle-and-dedup.md#evidence-and-limits) separates current source behavior from measured results.

## Release a version

Open **Actions → Release zmem → Run workflow**. Leave `tag` empty and set `version` to `patch`, `minor`, `major`, or an explicit `X.Y.Z`.

The workflow updates `Cargo.toml`, `Cargo.lock` and the website version, validates the source, commits to `main`, and creates the release tag. The same run builds native Linux x86_64 and ARM64 executables, publishes the crate with the repository's `CARGO_REGISTRY_TOKEN` secret, and uploads the archives and checksums to GitHub Releases. A direct version-tag push runs the release pipeline too. All releases share a concurrency group to prevent overlapping version publication.

To rebuild an existing tag after a failed infrastructure step, set `tag` to that version. An existing crate version is retained; an existing release asset must match the rebuilt file exactly. The workflow does not silently replace published artifacts.

Local version editing is also available:

```sh
python3 scripts/bump.py patch
# Or commit, tag and atomically push a validated release from a clean main:
python3 scripts/bump.py patch --release
```

Local `--release` pushes a tag, which starts the official release workflow when pushed using a normal repository credential. GitHub's workflow token does not recursively start tag workflows; the one-click path therefore continues its release in the same workflow run.

The Pages workflow deploys `site/` directly. There is no package install, external font, analytics script or frontend build step. Daily source work is tested locally; the automated test jobs belong to the explicit release pipeline.

## Kernel references

- [Pressure Stall Information](https://docs.kernel.org/accounting/psi.html)
- [cgroup v2 memory controls](https://docs.kernel.org/admin-guide/cgroup-v2.html)
- [zram administration](https://docs.kernel.org/admin-guide/blockdev/zram.html)
- [Virtual-memory sysctls](https://docs.kernel.org/admin-guide/sysctl/vm.html)
- [Kernel Samepage Merging](https://docs.kernel.org/admin-guide/mm/ksm.html)
- [Btrfs swap files](https://btrfs.readthedocs.io/en/latest/Swapfile.html)

The website uses the warm graphite, linen and copper direction of [oh-my-ztty](https://lightjunction.github.io/oh-my-ztty/). MIT licensed.
