# 0.4.3 input responsiveness acceptance

## Outcome

The development host runs the tested 0.4.3 daemon, and its default `zmem` command resolves to the same installed version. Kernel readback confirms a 256 MiB `memory.low` allocation for the running Fcitx scope, with sufficient protection through every ancestor. The shared ceiling remains 2,059,587,584 bytes on this host.

The [sanitized observation data](observations/0.4.3-input.json) records the initial diagnosis and two 30-second observation windows. It contains no process IDs, command arguments, environment contents, window titles or typed text.

## Problem and policy

The initial observation found both Fcitx5 and the desktop compositor in disk sleep. Fcitx had about 72 MiB resident and 550 MiB swapped out, while its cgroup received about 32.5 MiB of protection. The previous planner capped input requests at 64 MiB, used resident usage alone, and proportionally shared application protection with other selected applications. Eviction could therefore reduce an input method's future protection request.

The new planner estimates input demand from `memory.current + memory.swap.current`, caps each input scope at 256 MiB, and funds input requests before ordinary requests inside each existing category. Multiple input scopes share an oversubscribed category proportionally. Duplicate roles retain Input priority and its cap. Other roles retain their resident-usage requests; system, user, category and ancestor ceilings remain bounded.

The allocation protects the observed cgroup. A terminal-launched input method can share that group with other processes. `memory.low` is a best-effort reclaim preference; faulting pages return on demand. See the kernel's [reclaim protection and memory ownership documentation](https://docs.kernel.org/admin-guide/cgroup-v2.html).

## Validation

| Check | Result |
| --- | --- |
| Rust regression suite | 231 passed; 3 existing privileged kernel fixtures remain ignored |
| Input-policy regressions | Swap-backed and fully evicted input demand, overflow, duplicate roles, oversubscribed applications, user/category separation, page-size rounding and discovery fallback passed |
| Formatting and Clippy | `cargo fmt --check` and `cargo clippy --locked --all-targets -- -D warnings` passed |
| Policy core | `cargo check --locked --lib --no-default-features` passed |
| Observer | 8 self-test groups passed, including valid/invalid Fcitx states, disabled and unavailable probes, command failure, timeout and aggregation |
| Version tooling | 2 Python version-bump tests passed |
| Release executable | Built with Rust 1.100.0 for x86_64 Linux, executed successfully on the host |
| Live controls | Daemon active; input `memory.low=268435456`; all ancestor allocations cover it; protection complete with no reported errors |
| CLI entry | The prior suite-provided 0.4.1 symlink was preserved in a local receipt and redirected to the active system installation; `zmem --version` returns 0.4.3 |

Compilation and source checks ran outside the development host. The installed binary matched the tested artifact, SHA-256 `62b2c9d4562d50cda2da9a36ea74a86f8df9910595388267b481d77ef55f6114`. Installation retained active swap and preserved the prior executable for local recovery.

## Live observation

Both windows used the same observer with:

```sh
python3 scripts/measure_responsiveness.py \
  --duration 30 --desktop-probe --input-probe \
  --output artifacts/input-observation
```

Each invocation needs a distinct output directory. The observer uses `fcitx5-remote --check` to query an existing daemon without activating it or collecting text. Query time includes process startup and IPC; timeouts have an 800 ms threshold and remain separate from successful timing distributions.

| Observed metric | Before, daemon 0.4.2 | After, daemon 0.4.3 |
| --- | ---: | ---: |
| Samples | 16 | 16 |
| Fcitx queries above the 800 ms deadline | 13 / 16 | 0 / 16 |
| Successful Fcitx query range | 1.96–71.12 ms, 3 successes | 2.05–7.00 ms, 16 successes |
| Compositor query timeouts | 2 / 16 | 0 / 16 |
| Longest successful compositor query | 634.46 ms | 48.25 ms |
| Mean memory full PSI | 7.60% | 5.85% |
| Mean IO full PSI | 45.40% | 77.24% |

Input and compositor service queries completed throughout the second window. Whole-system storage contention remained substantial, and the IO full metric increased. The two windows used changing live workloads, including maintenance-service state changes, so the measurements describe those windows without assigning every difference to the patch. No physical keyboard-to-screen timing was recorded.

The daemon observed zero populated managed background groups in these windows. Existing compiler, scanner and application work therefore remained largely outside the explicit background-admission path. New batch work can use `zmem run --class background --wait-timeout 300 -- COMMAND`; the [admission guide](background-admission.md) describes that boundary.

## Evidence review

| Question | Evidence and assessment |
| --- | --- |
| Does the change address an observed failure? | The input scope had substantial swapped usage and a small protection allocation; swap-aware demand and Input priority cover both causes in regression fixtures. |
| Are the policy and units reproducible? | Role caps, byte counters, category priority, global ceiling and command flags are specified above and in the responsiveness guide. |
| What performance result was observed? | The second window had 16 successful input queries and no input or compositor query timeouts; the full observation data also retains the high IO pressure. |
| What remains outside this measurement? | Physical keystroke-to-screen latency, controlled workload comparisons and the three unrelated privileged kernel fixtures. |
| Are resource bounds retained? | Tests cover total/system/user/category ceilings and tiny page budgets; actual input and ancestor controls were read back from the running kernel. |
