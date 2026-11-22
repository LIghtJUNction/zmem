# Background admission

Starting a large job can make an already busy machine spend more time moving pages than doing useful work. zmem 0.4 checks current memory conditions before starting a command in the `background` or `disposable` class. Waiting happens before the target command runs, so it adds no compiler or model working set during that wait.

Admission and the daemon's running-task budgets serve different purposes. Admission decides when an explicitly submitted job may begin. After launch, the daemon adjusts the memory budget of managed background groups. Neither mechanism estimates an arbitrary program's future peak memory or automatically manages commands launched outside `zmem run`.

## Run a job

```sh
# Wait up to 60 seconds by default, then run under the background policy.
zmem run --class background -- cargo build --release --locked

# Allow a longer recovery window for a batch job.
zmem run --class background --wait-timeout 300 -- YOUR_BATCH_COMMAND

# Make one immediate admission attempt; exit 75 if it cannot be admitted.
zmem run --class background --wait-timeout 0 -- YOUR_COMMAND

# Interactive commands bypass admission.
zmem run --class interactive -- YOUR_INTERACTIVE_COMMAND
```

`--wait-timeout` is an integer number of seconds from 0 to 3600. It bounds admission waiting, not the target command's execution time. An interactive invocation accepts a valid timeout value but does not use it or wait. A timeout exits with code 75 before the target starts. SIGINT and SIGTERM during waiting cancel that invocation with exit codes 130 and 143 when the inherited signal disposition permits delivery. Inherited ignored or blocked signals retain their meaning. zmem does not suspend or terminate another task to admit this one.

The `disposable` class uses the same admission check and retains its explicit `--max-memory MiB` hard limit. Kernel OOM handling can terminate a disposable job that exceeds that limit. `--dedup` remains a separate opt-in for trusted background or disposable jobs.

## Conditions for admission

The launcher reads available RAM and the kernel's ten-second memory-pressure averages. Linux [PSI](https://docs.kernel.org/accounting/psi.html) measures time when tasks wait for a resource: `some` means at least one task is stalled; `full` means all non-idle tasks are stalled together. Admission uses memory PSI. An I/O stall from a deliberately paced background scanner does not by itself block a new job.

| Check | Initial healthy observation | Recovery after pressure or a missing observation |
| --- | --- | --- |
| Available RAM | At least the launch reserve | At least 125% of that reserve, capped at total RAM |
| Memory `some` PSI, avg10 | Below 5%, or a lower configured pressure threshold | Below half that threshold |
| Memory `full` PSI, avg10 | Below 1%, or a lower configured critical threshold | Below half that threshold |
| Stability | A healthy first observation can pass immediately | All recovery checks must remain satisfied for three seconds |

The automatic launch reserve starts with the policy's larger-of-1-GiB-or-10%-of-RAM reserve, then caps it at 25% of total RAM. This avoids an impossible 1 GiB launch floor on a small machine. An explicitly configured reserve retains its meaning; admission does not silently reduce it to the automatic cap. The 25% cap applies to launch admission, not to the daemon's other reserve calculations.

The launcher samples about once per second and rechecks immediately before recording an admission. Missing, malformed or inconsistent observations do not count as healthy. Recovery uses stricter thresholds so a brief low reading does not release a burst of work. These thresholds are zmem policy choices, not kernel recommendations or a guarantee of desktop latency.

## Concurrent starts

Launch reservations by the same user share a three-second minimum interval. A short exclusive lock protects the final observation and reservation. Waiters release the lock between attempts, and launched workloads do not retain it. This limits simultaneous admission after recovery without placing a permanent CPU quota or storage bandwidth cap on every command.

The coordination record lives in the user's runtime directory; root uses `/run/zmem-admission`. zmem verifies ownership, permissions and file identity, and uses boot identity and a monotonic clock rather than wall time. A launcher exiting early does not erase its interval, because the command may have left descendants running. This spaces admission reservations; scheduling delays can still change the interval between actual command execution. There is no FIFO promise, declared peak-memory reservation or global cross-user launch schedule.

## Script and AI contract

Admission events are JSON lines on stderr. They never include the command or its arguments. Stdout remains available for the target's output, and the command receives its argument bytes, environment, working directory and standard input through the normal scope launch.

| Event | Meaning |
| --- | --- |
| `admission_wait` | This invocation is still waiting; the record includes elapsed time, timeout and a reason. Progress events are at least five seconds apart. |
| `admission_granted` | The invocation reserved a launch opportunity after a fresh observation. This is not confirmation that the target executable has started successfully. |
| `admission_timeout` | The wait expired; the target was not started. |
| `admission_cancelled` | A termination signal cancelled the wait; the target was not started. |
| `admission_error` | Required runtime state or launch preparation could not be validated. |
| `run_error` | The run arguments or configuration were invalid; the target was not started. |

After launch, normal command output and exit status belong to the command and systemd scope. The launcher checks the host `systemd-run` version: newer versions explicitly disable variable expansion, while older scope semantics already preserve literal arguments. An unknown version fails before the target starts. The compatibility distinction is documented in the upstream [systemd 254](https://www.freedesktop.org/software/systemd/man/254/systemd-run.html) and [systemd 252](https://www.freedesktop.org/software/systemd/man/252/systemd-run.html) manuals.

`zmem schema` exposes the admission options and exit semantics. `zmem status --json` describes the running daemon and managed groups; it is not a persistent queue of pending launcher processes.

## What this changes in a development workflow

Project Z and oh-my-ztty build helpers already submit heavy Cargo commands through `zmem run --class background` when a compatible local CLI and daemon are available. With zmem 0.4 those submissions also wait for admission. Cargo metadata queries remain outside that path. If admission times out, retry after pressure subsides or select a longer wait for an explicit invocation.

Reuse a persistent Cargo target directory and avoid overlapping large checks when the working sets exceed RAM. Admission does not replace Cargo's dependency cache. A cache hit can remove compilation work; admission reduces the chance of adding that work at the worst moment. Neither change guarantees a faster cold build.

## Verification boundaries

The parser, recovery transitions, deadline handling and shared-state ownership are tested independently of host load. A host acceptance record identifies which native command checks actually ran. Unit tests and a successful launch do not establish a percentage reduction in swapping, input latency or application peak memory.
