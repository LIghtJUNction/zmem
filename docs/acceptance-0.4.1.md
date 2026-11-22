# zmem 0.4.1 acceptance record

Version 0.4.1 fixes the file-opening ABI used by the AArch64 build. Admission thresholds, recovery, timeouts, running-task budgets and configuration remain unchanged from 0.4.0. This record identifies the defect and the source validation; the formal release workflow provides the native runner results for both published architectures.

## Why the previous publication stopped

The [0.4.0 formal workflow](https://github.com/LIghtJUNction/zmem/actions/runs/45015929238) passed x86_64 validation and packaging, but seven AArch64 admission tests failed while opening the coordination directory with `EINVAL`. The dependent publication job was skipped, so that run published neither a crate nor GitHub Release assets. Its public source tag was retained.

Linux assigns different numeric directory and no-follow flags to the two architectures. The implementation had used x86 values for both. The [ARM64 header](https://github.com/torvalds/linux/blob/v6.8/arch/arm64/include/uapi/asm/fcntl.h), [generic Linux header](https://github.com/torvalds/linux/blob/v6.8/include/uapi/asm-generic/fcntl.h) and [glibc ARM64 header](https://github.com/bminor/glibc/blob/glibc-2.39/sysdeps/unix/sysv/linux/aarch64/bits/fcntl.h) establish the corrected values:

| Flag | x86_64 | AArch64 |
| --- | --- | --- |
| `O_DIRECTORY` | `0o200000` | `0o40000` |
| `O_NOFOLLOW` | `0o400000` | `0o100000` |
| `O_NONBLOCK` | `0o4000` | `0o4000` |
| `O_PATH` | `0o10000000` | `0o10000000` |

The new internal `linux_flags` module supplies these values to admission, installation, swap helpers and I/O delegation. Existing file-type and identity checks remain in place. With `O_PATH | O_NOFOLLOW`, opening a final symlink pins the symlink itself; the subsequent metadata check must still reject it when a regular file is required.

## Actual regression checks

Three new tests perform real file operations instead of comparing constants with expected literals. They verify that a directory opens correctly, a regular file cannot be opened as a directory (`ENOTDIR`), an ordinary no-follow open rejects a symlink (`ELOOP`), and an `O_PATH` descriptor has the expected inode and cannot read file data (`EBADF`). The symlink-descriptor case also verifies type and identity.

The complete x86_64 source gate, under the shared target's single outer lock, passed **215 tests: 12 library, 183 binary and 20 policy tests**. Three existing privileged kernel integration tests remained ignored.

| Gate | Actual result |
| --- | --- |
| Formatting | Passed |
| Locked offline Rust tests | 215 passed, 0 failed, 3 ignored |
| Strict Clippy, all targets | Passed |
| Locked offline `no_std` library check | Passed |
| Locked offline release build | Passed |
| Bump regression tests | 2 passed |
| Responsiveness-probe self-tests | 7 groups passed |
| Diff whitespace check | Passed |

The local x86_64 build used Rust 1.100.0. Its executable is 1,158,952 bytes, reports `zmem 0.4.1`, and has SHA-256 `c29ffabbee8a9680772a859df23fa8feaa5e122074587bf62a0805b1df2c1f40`. This local candidate was not installed on the development host for these source checks. Formal release binaries are built independently and have their own published checksums.

## Native evidence and publication boundary

The [0.4.0 native record](acceptance-0.4.0.md) remains the evidence for the previously installed x86_64 candidate: bounded refusal and cancellation, literal argument and I/O passthrough, interactive bypass, a positive background scope launch, and preservation of configuration and all 15 active Swap entries. Those observations are not relabelled as tests of the 0.4.1 binary.

The [release workflow](../.github/workflows/release.yml) retains independent native x86_64 and AArch64 validation before publication. Both execute the new file-operation tests as part of the complete suite. Publication proceeds only after both jobs succeed; the [release assets](https://github.com/LIghtJUNction/zmem/releases) identify their build source and checksums. No skipped or incomplete ARM job counts as a pass.

This ABI correction does not establish a memory-saving, throughput or input-latency improvement percentage. The native KSM merge/COW and positive inactive-Swap cleanup exercises remain outside this record. See the [admission guide](background-admission.md) for the behavior introduced in the 0.4 series and its explicit coverage limits.
