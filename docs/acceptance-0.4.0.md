# zmem 0.4.0 acceptance record

The formal 0.4.0 release was blocked by AArch64 validation: its binary test suite reported 173 passed, seven failed and three ignored. x86_64 validation and packaging succeeded, but the dependent publish job did not run. No 0.4.0 crate or GitHub Release assets were published by that run. The local x86_64 evidence below remains specific to its tested candidate. The architecture fix is recorded in [0.4.1 acceptance](acceptance-0.4.1.md).

This release adds launch admission for commands explicitly submitted as background or disposable work. The evidence below separates source tests, native command behavior and service installation. It does not establish a desktop-speed improvement percentage, a peak-memory prediction or protection for every unregistered process.

## Tested implementation

The source builds on published 0.3.1 commit `6b756269a78d4d44bca2a019e47575174e40c0b1`. The release tag identifies the complete 0.4.0 source. The local candidate was built in the separate build environment with Rust 1.100.0 and the locked, dependency-free manifest.

| Candidate property | Observed value |
| --- | --- |
| CLI version | `zmem 0.4.0` |
| Linux x86_64 executable size | 1,158,968 bytes |
| SHA-256 | `c37f7d064be2e7012a09987ee91099202a021748f050bf91463c63026a6e5e74` |
| Compiler | `rustc 1.100.0 (0ef1bc386 2026-11-09)` |

The native transfer used a verified 94,124-byte delta against the earlier local 0.3.1 candidate. The reconstructed executable matched the candidate's full SHA-256 before execution. Official release assets are separate builds and carry their own checksums; their identity is not inferred from this candidate hash.

## Source gates

All Cargo operations below ran under the shared target directory's single outer owner lock. The final source gate completed with **212 tests passed: 12 library, 180 binary and 20 policy tests**. Three existing privileged kernel integration tests remained ignored.

| Command | Actual result |
| --- | --- |
| `cargo fmt --check` | Passed |
| `cargo test --locked --offline --quiet` | 212 passed, 0 failed, 3 ignored |
| `cargo clippy --locked --offline --all-targets -- -D warnings` | Passed |
| `cargo check --locked --offline --lib --no-default-features` | Passed |
| `cargo build --release --locked --offline` | Passed |
| Python bump regression suite | 2 passed |
| `measure_responsiveness.py --self-test` | 7 groups passed |
| `git diff --check` | Passed |

Nineteen new admission-related tests cover strict kernel-observation parsing, missing information, small-RAM reserve bounds, one-basis-point thresholds, recovery interruptions, deadlines, concurrent reservations, file identity, FIFO rejection, closed stderr, cancellation and inherited ignored signals, literal argv, interactive bypass and systemd version compatibility. Synthetic sample tests validate decisions; they do not measure native desktop latency.

## Native command checks

The host has 16,476,700,672 bytes of RAM and an existing systemd user manager. A temporary, owner-only configuration with a deliberately unattainable launch reserve tested rejection without allocating a large working set. It was passed only to the launcher and did not replace `/etc/zmem/zmem.conf` or change the daemon's policy.

| Check | Actual observation |
| --- | --- |
| Zero-second wait with the test reserve | Exit 75 in about 0.022 seconds; target marker absent; stdout empty |
| Two-second wait with the test reserve | Exit 75 in about 2.022 seconds; target marker absent; stdout empty |
| SIGINT during an eligible wait | Exit 130 in about 0.134 seconds; target not started |
| SIGTERM during an eligible wait | Exit 143 in about 0.124 seconds; target not started |
| Interactive command with the same test reserve and zero timeout | Started in a zmem interactive scope without an admission event; preserved exit status 37 |
| Interactive argument and I/O passthrough | Literal `$HOME` and `${HOME}`, an empty argument, option-like text and a non-UTF-8 argument retained their bytes; environment, working directory and stdin matched the caller |
| Twenty-second background attempt under natural conditions | Exit 75 after about 20.022 seconds; four progress events and a timeout; target not started |

The signal checks set normal signal dispositions for their own test launcher. Inherited ignored and blocked signal behavior is covered separately by the source tests. No unrelated process was signalled by these native checks.

After installation, a separate background invocation using the normal configuration passed its initial check and ran successfully inside the user's `zmem.slice/zmem-background.slice`. The grant recorded 2,356,547,584 available bytes, memory `some` PSI of 83 basis points and `full` of 82 basis points. This confirms a real positive systemd scope launch as well as the earlier refusal paths.

These were different observations and invocations. A call that has already seen pressure needs the stricter recovery gate, while a new call can pass its first healthy observation. The twenty-second refusal and later immediate admission therefore do not demonstrate a controlled before/after performance change.

## Service upgrade

The tested candidate was installed through the ordinary `install --replace` path. After installation:

- The existing configuration's full SHA-256 was unchanged.
- All 15 active Swap entries retained their paths, types, sizes and priorities.
- The installed `/usr/local/bin/zmem` matched the candidate's SHA-256.
- `zmem.service` was active with zero automatic restarts in the observed service instance.
- Status returned a fresh, process-identity-verified 0.4.0 daemon report.
- Interactive protection was complete and its error list was empty.

No active Swap was globally drained and no zram device was rebuilt. This upgrade changes how new explicitly submitted jobs enter the existing managed groups; it preserves the running-task policy and existing system configuration.

## Scope of the evidence

Admission adds no permanent CPU quota or fixed disk bandwidth cap. Same-user admission reservations are spaced by three seconds, but scheduling can change the timing of actual execution. It is not a persistent FIFO queue, cross-user scheduler or application checkpoint system.

The prior host investigation still showed paging variation during ordinary work. A lower PSI sample, a successful launch or an unstarted command cannot establish a fixed throughput or mouse-latency gain. Evaluation of those gains requires comparable workloads and repeated measurements. The native KSM merge/COW and positive inactive-Swap cleanup exercises remain deferred; neither is newly validated by this launch feature.

See the [admission guide](background-admission.md) for operational thresholds and [responsiveness guide](responsiveness.md) for paging interpretation. The earlier [0.3.1 record](acceptance-0.3.1.md) documents I/O delegation and the separate scanner mitigation.
