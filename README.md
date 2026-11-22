# zmem

**Memory, with room to work.** A small Linux memory and Swap controller from [Project Z](https://github.com/LIghtJUNction/project-z).

[Website](https://lightjunction.github.io/zmem/) · [Releases](https://github.com/LIghtJUNction/zmem/releases) · [crates.io](https://crates.io/crates/zmem-manager) · [Rust API](https://docs.rs/zmem-manager) · [Host acceptance](docs/acceptance-0.1.2.md) · [Latest patch validation](docs/acceptance-pressure-journal.md)

zmem observes memory pressure, preserves headroom for interactive work, and gives managed background jobs a bounded memory budget. It brings zram setup, dynamic disk swap, pressure policy and structured inspection into one tool. The Rust implementation has no third-party crate dependencies; the policy library uses `no_std` and makes no heap allocations.

## What it does

| Capability | Behavior |
| --- | --- |
| Pressure policy | Combines available memory, memory/IO PSI and swap activity, with cooldowns and recovery hysteresis. |
| Swap management | Adopts active swap, grows disk swap within capacity and free-space limits, and shrinks only unused, owned files. |
| zram lifecycle | Keeps the current active device during migration and prepares zram on subsequent boots. |
| Workload budgets | Uses cgroup v2 `memory.high` for managed background work and explicit hard limits for disposable jobs. |
| Interactive protection | Protects desktop sessions and SSH through the system's memory-control hierarchy. |
| Observability | Exposes memory, swap, compression, process usage and policy reasons through human-readable and JSON commands. |
| Service migration | Saves prior service/configuration state, replaces existing management services, and offers rollback. |

The Linux kernel performs actual paging, compression and final OOM handling. zmem makes earlier policy decisions using those facilities. Automatic application checkpoints, general-purpose memory deduplication and energy optimization are future work. KSM statistics describe what the kernel has already merged; they are not a claim that every application's memory can be deduplicated.

The default policy preserves processes. It does not issue global cache drops or automatically kill arbitrary applications. A task explicitly started as disposable can be given a hard cgroup limit, where the kernel may terminate it if the budget is exhausted.

## Installation

Use a Linux system with systemd, PSI and cgroup v2. Swap creation uses the host's swap utilities; Btrfs hosts also need `btrfs-progs`. The daemon needs root privileges for system-wide configuration and swap management. Read-only status and policy inspection can run without root.

### From crates.io

```sh
cargo install zmem-manager --locked
zmem doctor
zmem status --json
sudo "$(command -v zmem)" install --replace
```

`install --replace` is the explicit system migration operation. It preserves active swap, records the prior management configuration and installs the zmem service. Existing swap that contains data is adopted in place. Changes that require rebuilding zram take effect at a normal subsequent boot.

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

Stopping or upgrading the management daemon leaves active swap in place. Rollback restores the saved management configuration. It does not drain active swap into RAM. Keep the previous configuration record until the new setup has completed a normal boot on your machine.

## Everyday use

```sh
zmem status
zmem status --json --processes
zmem plan --json
zmem doctor --json
zmem schema
```

`status` reports measurements. `plan` reports the policy's proposed action and its reason. `schema` describes the command contract for scripts and AI tools. Process details are collected on request rather than on every daemon tick.

To observe policy over time before applying system changes:

```sh
zmem daemon --observe
```

Start background work with its classification from the beginning:

```sh
zmem run --class background -- cargo build --release
zmem run --class interactive -- YOUR_COMMAND
zmem run --class disposable --max-memory 2048 -- YOUR_BATCH_COMMAND
```

The `--max-memory` value is in MiB. The disposable class is for work that can be restarted if it exceeds its explicit budget. Starting a job inside a cgroup gives the kernel a consistent accounting boundary for the job's allocations.

## Policy and resource limits

The policy core is a deterministic state machine. It receives a snapshot and a monotonic timestamp, then produces a bounded decision. The Linux adapter checks real ownership, kernel state, disk capacity and operation results before acting.

Busy Swap/IO takes precedence over the preference for available zram capacity. When the policy reports `swap_or_io_pressure`, the adapter keeps its swappiness hint of 20, including when Swap-in is high but IO PSI is low. Outside that condition, the existing zram and profile preferences apply.

While the daemon is running, zmem owns the aggregate `memory.high` policy of its dedicated `zmem.slice/zmem-background.slice`, including the corresponding slice below a user manager. systemd can reset that slice to `max` when a new scope starts. If the original baseline was unlimited and the cgroup is still the same object, zmem reapplies its policy and records a `managed_background_limit_reapplied` event. A manual `max` write to this dedicated slice is therefore temporary. Finite external limits, finite original baselines, other cgroups and `memory.low` retain the normal external-change protection. Run jobs outside the background class when another tool should own their limits.

Recovery records for vanished or replaced cgroups are removed every 15 sampling cycles, or about 30 seconds with the default interval. Live control baselines and records with uncertain metadata errors remain intact. Reading a recovery journal larger than 1 MiB returns an error and retains the whole file for recovery.

| Default policy setting | Value |
| --- | --- |
| Available-memory reserve | Larger of 1 GiB or 10% of physical RAM, capped at total RAM |
| Desired unused Swap headroom | 4 GiB when growth is needed |
| Maximum growth request | 2 GiB per decision |
| Managed disk Swap cap | 32 GiB |
| Filesystem free-space reserve | 8 GiB |
| Swap growth cooldown | 30 seconds |
| Stable healthy time before shrinking | 600 seconds |

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

For meaningful host acceptance, exercise a bounded test workload in its own cgroup. Record probe latency, workload progress, PSI, swap IO and daemon RSS/CPU together. Do not infer responsiveness from a lower used-memory number alone. Exact measurements should identify the tested source, kernel, storage and workload.

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
