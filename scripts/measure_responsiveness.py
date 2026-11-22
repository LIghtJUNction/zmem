#!/usr/bin/env python3
"""Observe an unchanged Linux workload; never apply a memory policy.

Examples:
  python3 scripts/measure_responsiveness.py --duration 30
  python3 scripts/measure_responsiveness.py --duration 60 --desktop-probe \
      --output artifacts/responsiveness-baseline
  python3 scripts/measure_responsiveness.py --self-test

--output is a directory. Existing measurement files are never overwritten.
Only this script's output files are written; all procfs/sysfs/cgroup reads and
the zmem/Hyprland requests are observational. No process arguments, environment
contents, window titles, monitor descriptions or raw command stderr are saved.
"""
from __future__ import annotations

import argparse
import collections
import hashlib
import itertools
import json
import math
import os
from pathlib import Path
import platform
import resource
import selectors
import shutil
import signal
import stat
import subprocess
import sys
import threading
import time


INTERVAL_NS = 2_000_000_000
WAKE_INTERVAL_NS = 100_000_000
COMMAND_TIMEOUT_S = 0.8
MAX_COMMAND_BYTES = 256 * 1024
MAX_GROUPS = 128
CGROOT = Path("/sys/fs/cgroup")
VM_KEYS = ("pswpin", "pswpout", "workingset_refault_file", "workingset_refault_anon")
MM_FIELDS = ("orig_data_size_bytes", "compr_data_size_bytes", "mem_used_total_bytes",
             "mem_limit_bytes", "mem_used_max_bytes", "same_pages", "pages_compacted",
             "huge_pages", "huge_pages_since")
REPORT_KEYS = frozenset(("schema", "version", "pid", "sampled_at_unix", "profile",
    "instance", "pressure", "memory_pressure", "reason", "io_congested", "memory",
    "memory_usage", "swap", "psi", "zram", "zram_devices", "ksm", "plan", "worker",
    "errors", "paging", "controls", "managed", "observation"))
SENSITIVE_KEYS = frozenset(("cmdline", "command_line", "argv", "env", "environ",
                          "environment", "title", "window_title"))


def nonnegative(value):
    try:
        number = int(value)
        return number if number >= 0 else None
    except (TypeError, ValueError):
        return None


def parse_pairs(text):
    result = {}
    for line in (text or "").splitlines():
        parts = line.split()
        if len(parts) == 2:
            result[parts[0]] = nonnegative(parts[1])
    return result


def parse_psi(text):
    result = {"some": None, "full": None}
    for line in (text or "").splitlines():
        parts = line.split()
        if parts and parts[0] in result:
            fields = dict(item.split("=", 1) for item in parts[1:] if "=" in item)
            result[parts[0]] = nonnegative(fields.get("total"))
    return result


def parse_vmstat(text):
    values = parse_pairs(text)
    selected = {name: values.get(name) for name in VM_KEYS}
    zones = ("dma", "dma32", "normal", "high", "highmem", "movable", "device")
    direct_sources = ["pgscan_direct"] if values.get("pgscan_direct") is not None else [
        "pgscan_direct_" + zone for zone in zones
        if values.get("pgscan_direct_" + zone) is not None]
    selected["pgscan_direct"] = (
        sum(values[key] for key in direct_sources) if direct_sources else None)
    selected.update((key, values[key]) for key in direct_sources)
    return selected, direct_sources


def parse_meminfo(text):
    values = {"MemAvailable": None, "MemTotal": None}
    for line in (text or "").splitlines():
        key, sep, rest = line.partition(":")
        fields = rest.split()
        if sep and key in values and len(fields) == 2 and fields[1] == "kB":
            value = nonnegative(fields[0])
            values[key] = value * 1024 if value is not None else None
    return values


def counter_delta(before, after):
    if before is None or after is None:
        return None, "unavailable"
    if after < before:
        return None, "counter_reset"
    return after - before, None


def percentile(values, fraction):
    values = sorted(value for value in values if value is not None and math.isfinite(value))
    if not values:
        return None
    index = (len(values) - 1) * fraction
    lo, hi = math.floor(index), math.ceil(index)
    return values[lo] + (values[hi] - values[lo]) * (index - lo)


def distribution(values):
    values = [value for value in values if value is not None and math.isfinite(value)]
    return {"count": len(values), "p50": percentile(values, .5),
            "p95": percentile(values, .95), "p99": percentile(values, .99),
            "min": min(values) if values else None, "max": max(values) if values else None}


def read_text(path, errors, label, limit=256 * 1024):
    try:
        with open(path, "rb") as source:
            data = source.read(limit + 1)
        if len(data) > limit:
            errors.append({"source": label, "error": "size_limit"})
            return None
        return data.decode("utf-8")
    except (OSError, UnicodeError) as error:
        errors.append({"source": label, "error": type(error).__name__})
        return None


def page_size():
    try:
        size = os.sysconf("SC_PAGE_SIZE")
        return size if size > 0 and size & (size - 1) == 0 else None
    except (OSError, ValueError):
        return None


def number_list(text):
    if text is None:
        return None
    values = [nonnegative(value) for value in text.split()]
    return values if values and all(value is not None for value in values) else None


def read_zram(errors):
    devices = {}
    try:
        paths = list(Path("/sys/block").iterdir())
    except OSError as error:
        errors.append({"source": "zram_devices", "error": type(error).__name__})
        return None
    for path in sorted(paths):
        if not path.name.startswith("zram") or not path.name[4:].isdigit():
            continue
        block = number_list(read_text(path / "stat", errors, path.name + ".stat", 4096))
        mm = number_list(read_text(path / "mm_stat", errors, path.name + ".mm_stat", 4096))
        device = read_text(path / "dev", errors, path.name + ".dev", 128)
        # diskseq is optional on older kernels. Counter decreases still
        # invalidate the corresponding window.
        try:
            diskseq = nonnegative((path / "diskseq").read_text().strip())
        except OSError:
            diskseq = None
        devices[path.name] = {
            "identity": {"dev": device.strip() if device is not None else None, "diskseq": diskseq},
            "stat": block, "mm_stat": mm,
            "mm": {key: mm[index] if mm is not None and index < len(mm) else None
                   for index, key in enumerate(MM_FIELDS)},
        }
    return devices


def parse_io_stat(text):
    if text is None:
        return None
    devices = {}
    for line in text.splitlines():
        fields = line.split()
        if fields:
            devices[fields[0]] = {key: nonnegative(value)
                for key, value in (entry.split("=", 1) for entry in fields[1:] if "=" in entry)}
    return devices


def read_background(errors):
    if not (CGROOT / "cgroup.controllers").is_file():
        return None, False
    candidates = itertools.chain(
        [CGROOT / "zmem.slice/zmem-background.slice"],
        CGROOT.glob("user.slice/user-*.slice/user@*.service/zmem.slice/zmem-background.slice"),
    )
    groups = {}
    truncated = False
    for path in candidates:
        try:
            identity = path.stat()
        except FileNotFoundError:
            continue
        except OSError as error:
            errors.append({"source": "background_groups", "error": type(error).__name__})
            continue
        if not stat.S_ISDIR(identity.st_mode):
            continue
        if len(groups) >= MAX_GROUPS:
            truncated = True
            break
        key = str(path.relative_to(CGROOT))
        # Parent cgroup counters include descendants: do not count scopes twice.
        groups[key] = {
            "identity": [identity.st_dev, identity.st_ino],
            "cpu": parse_pairs(read_text(path / "cpu.stat", errors, key + ".cpu")),
            "io": parse_io_stat(read_text(path / "io.stat", errors, key + ".io")),
            "events": parse_pairs(read_text(path / "cgroup.events", errors, key + ".events")),
            "memory_events": parse_pairs(read_text(path / "memory.events", errors, key + ".memory_events")),
            "memory_bytes": nonnegative(read_text(path / "memory.current", errors, key + ".memory", 128)),
            "pids": nonnegative(read_text(path / "pids.current", errors, key + ".pids", 128)),
        }
    return groups, truncated


def collect_kernel():
    begin = time.monotonic_ns()
    errors = []
    size_before = page_size()
    psi = {kind: parse_psi(read_text(Path("/proc/pressure") / kind, errors, "psi." + kind, 4096))
           for kind in ("memory", "io", "cpu")}
    vmstat, direct_sources = parse_vmstat(read_text("/proc/vmstat", errors, "vmstat"))
    memory = parse_meminfo(read_text("/proc/meminfo", errors, "meminfo"))
    zram = read_zram(errors)
    background, truncated = read_background(errors)
    size_after = page_size()
    end = time.monotonic_ns()
    return {"at_monotonic_ns": (begin + end) // 2, "read_span_ns": end - begin,
            "wall_time_unix_ns": time.time_ns(), "page_size_before_bytes": size_before,
            "page_size_after_bytes": size_after, "psi_totals_us": psi,
            "vmstat_pages": vmstat, "direct_scan_sources": direct_sources,
            "meminfo_bytes": memory, "zram": zram, "background": background,
            "background_truncated": truncated, "read_errors": errors}


def stop_probe(process):
    if process.poll() is None:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    try:
        process.wait(timeout=.2)
    except subprocess.TimeoutExpired:
        pass


def reject_constant(value):
    raise ValueError("non-finite JSON constant: " + value)


def command_json(program, arguments):
    """Bound both probe wall time and captured bytes; never keep stderr."""
    if program is None:
        return {"status": "unavailable", "reason": "program_not_found", "round_trip_ms": None}, None
    started = time.monotonic_ns()
    process = None
    try:
        process = subprocess.Popen([program, *arguments], stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, start_new_session=True)
        deadline = time.monotonic() + COMMAND_TIMEOUT_S
        data = bytearray()
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            while selector.get_map():
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise subprocess.TimeoutExpired(Path(program).name, COMMAND_TIMEOUT_S)
                for key, _ in selector.select(remaining):
                    chunk = os.read(key.fd, 65536)
                    if not chunk:
                        selector.unregister(key.fileobj)
                    elif len(data) + len(chunk) > MAX_COMMAND_BYTES:
                        return {"status": "unavailable", "reason": "output_limit", "round_trip_ms": None}, None
                    else:
                        data.extend(chunk)
            code = process.wait(timeout=max(0, deadline - time.monotonic()))
        elapsed = (time.monotonic_ns() - started) / 1_000_000
        if code != 0:
            return {"status": "unavailable", "reason": "command_failed", "round_trip_ms": elapsed}, None
        try:
            value = json.loads(data, parse_constant=reject_constant)
        except (UnicodeError, ValueError, RecursionError):
            return {"status": "unavailable", "reason": "invalid_json", "round_trip_ms": elapsed}, None
        return {"status": "available", "reason": None, "round_trip_ms": elapsed}, value
    except subprocess.TimeoutExpired:
        return {"status": "unavailable", "reason": "timeout",
                "round_trip_ms": None, "timeout_bound_ms": COMMAND_TIMEOUT_S * 1000}, None
    except OSError as error:
        return {"status": "unavailable", "reason": type(error).__name__, "round_trip_ms": None}, None
    finally:
        if process is not None:
            stop_probe(process)
            if process.stdout is not None:
                process.stdout.close()


def strip_sensitive(value):
    if isinstance(value, dict):
        return {key: strip_sensitive(item) for key, item in value.items()
                if key.lower() not in SENSITIVE_KEYS}
    if isinstance(value, list):
        return [strip_sensitive(item) for item in value]
    return value


def zmem_probe(program):
    probe, report = command_json(program, ["status", "--json"])
    if probe["status"] == "available" and isinstance(report, dict):
        probe["report"] = strip_sensitive({key: value for key, value in report.items() if key in REPORT_KEYS})
    else:
        if probe["status"] == "available":
            probe.update(status="unavailable", reason="unexpected_json_type")
        probe["report"] = None
    return probe


def desktop_probe(program, enabled):
    if not enabled:
        return {"status": "not_requested", "round_trip_ms": None}
    probe, monitors = command_json(program, ["-j", "monitors"])
    if probe["status"] == "available" and not isinstance(monitors, list):
        probe.update(status="unavailable", reason="unexpected_json_type")
    # Discard monitor data. Only request success and process + IPC time matter.
    return probe


def same_devices(before, after):
    if before is None or after is None or set(before) != set(after):
        return False
    for key in before:
        identity = before[key]["identity"]
        if identity is None or identity != after[key]["identity"]:
            return False
        if isinstance(identity, dict) and identity.get("dev") is None:
            return False
    return True


def aggregate_delta(before, after, extract, empty_is_zero=False):
    if not same_devices(before, after):
        return None, "coverage_or_identity_changed"
    if not before:
        return (0, None) if empty_is_zero else (None, "no_managed_groups")
    total = 0
    for name in before:
        delta, error = counter_delta(extract(before[name]), extract(after[name]))
        if error:
            return None, error
        total += delta
    return total, None


def background_io_delta(before, after, counter):
    if not same_devices(before, after):
        return None, "coverage_or_identity_changed"
    if not before:
        return None, "no_managed_groups"
    total = 0
    for name in before:
        first, last = before[name].get("io"), after[name].get("io")
        if first is None or last is None:
            return None, "unavailable"
        if set(first) != set(last):
            return None, "io_device_set_changed"
        for device in first:
            delta, error = counter_delta(first[device].get(counter), last[device].get(counter))
            if error:
                return None, error
            total += delta
    return total, None


def window_between(before, after):
    seconds = (after["at_monotonic_ns"] - before["at_monotonic_ns"]) / 1_000_000_000
    metrics, invalid = {}, {}

    def add(name, delta, error, unit, factor=1):
        if seconds <= 0:
            delta, error = None, "invalid_interval"
        if error:
            invalid[name] = error
        metrics[name] = {"delta": delta, "value": delta / seconds * factor if delta is not None else None,
                         "unit": unit, "factor": factor}

    for kind in ("memory", "io", "cpu"):
        for scope in ("some", "full"):
            delta, error = counter_delta(before["psi_totals_us"][kind][scope], after["psi_totals_us"][kind][scope])
            if kind == "cpu" and scope == "full":
                delta, error = None, "system_cpu_full_undefined"
            add("psi." + kind + "." + scope, delta, error, "percent", 100 / 1_000_000)
    sizes = [row[key] for row in (before, after)
             for key in ("page_size_before_bytes", "page_size_after_bytes")]
    pages_ok = sizes[0] is not None and all(size == sizes[0] for size in sizes)
    for key, name in (("pswpin", "swap_in"), ("pswpout", "swap_out"),
                      ("workingset_refault_file", "file_refault"), ("workingset_refault_anon", "anon_refault"),
                      ("pgscan_direct", "direct_scan")):
        delta, error = counter_delta(before["vmstat_pages"][key], after["vmstat_pages"][key])
        if key == "pgscan_direct":
            sources = before.get("direct_scan_sources", ["pgscan_direct"])
            if sources != after.get("direct_scan_sources", ["pgscan_direct"]):
                delta, error = None, "counter_sources_changed"
            else:
                for source in sources:
                    _, source_error = counter_delta(before["vmstat_pages"].get(source), after["vmstat_pages"].get(source))
                    if source_error:
                        delta, error = None, source_error
                        break
        if not pages_ok:
            delta, error = None, "page_size_changed_or_unavailable"
        add("paging." + name, delta * sizes[0] if delta is not None else None, error, "bytes_per_second")
    for index, name in ((2, "read"), (6, "write")):
        def extract(device, index=index):
            values = device.get("stat")
            return values[index] if values is not None and len(values) > index else None
        delta, error = aggregate_delta(before["zram"], after["zram"], extract, empty_is_zero=True)
        add("zram." + name, delta * 512 if delta is not None else None, error, "bytes_per_second")
    groups_before, groups_after = before["background"], after["background"]
    if before["background_truncated"] or after["background_truncated"]:
        groups_before = None
    delta, error = aggregate_delta(groups_before, groups_after, lambda group: group["cpu"].get("usage_usec"))
    add("background.cpu_time", delta, error, "cpu_microseconds_per_second")
    for key, name in (("rbytes", "read"), ("wbytes", "write")):
        delta, error = background_io_delta(groups_before, groups_after, key)
        add("background." + name, delta, error, "bytes_per_second")
    return {"seconds": seconds, "page_size_consistent": pages_ok, "metrics": metrics, "invalid": invalid}


class Wakeups(threading.Thread):
    def __init__(self, start, end):
        super().__init__(name="zmem-observer-wakeup", daemon=True)
        self.begin, self.end = start, end
        self.stop_event = threading.Event()
        self.samples = []

    def run(self):
        deadline = self.begin + WAKE_INTERVAL_NS
        while deadline <= self.end:
            if self.stop_event.wait(max(0, deadline - time.monotonic_ns()) / 1_000_000_000):
                break
            woke = time.monotonic_ns()
            next_deadline = deadline + WAKE_INTERVAL_NS
            skipped = max(0, (woke - next_deadline) // WAKE_INTERVAL_NS + 1)
            self.samples.append({"scheduled_elapsed_ns": deadline - self.begin,
                "woke_elapsed_ns": woke - self.begin, "lateness_ns": max(0, woke - deadline),
                "skipped_deadlines": skipped})
            deadline = next_deadline + skipped * WAKE_INTERVAL_NS


def write_json(file, value):
    file.write(json.dumps(value, sort_keys=True, separators=(",", ":"), allow_nan=False) + "\n")
    file.flush()


class Outputs:
    def __init__(self, directory):
        self.files = {}
        if directory is None:
            return
        directory = Path(directory)
        directory.mkdir(mode=0o700, parents=True, exist_ok=True)
        if directory.is_symlink() or not directory.is_dir():
            raise ValueError("output must be a directory, not a symlink")
        names = ("metadata.json", "samples.jsonl", "scheduler.jsonl", "summary.json")
        if any((directory / name).exists() or (directory / name).is_symlink() for name in names):
            raise ValueError("output already contains a measurement; choose another directory")
        try:
            for name in names:
                fd = os.open(directory / name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
                os.fchmod(fd, 0o600)
                self.files[name] = os.fdopen(fd, "w", encoding="utf-8")
        except BaseException:
            self.close()
            raise

    def write(self, name, value):
        if name in self.files:
            write_json(self.files[name], value)

    def close(self):
        for file in self.files.values():
            file.close()


def nested(value, *keys):
    for key in keys:
        if not isinstance(value, dict):
            return None
        value = value.get(key)
    return value


def summarize(samples, wakeups, requested, elapsed, interrupted, resources):
    windows = [sample["window"] for sample in samples if sample["window"] is not None]
    names = sorted(set().union(*(window["metrics"] for window in windows))) if windows else []
    metrics = {}
    for name in names:
        valid = [(window["metrics"][name], window["seconds"]) for window in windows
                 if window["metrics"][name]["value"] is not None]
        duration = sum(seconds for _, seconds in valid)
        delta = sum(metric["delta"] for metric, _ in valid) if valid else None
        factor = valid[0][0]["factor"] if valid else 1
        metrics[name] = {**distribution(metric["value"] for metric, _ in valid),
            "total_delta": delta, "valid_duration_s": duration,
            "time_weighted_mean": delta / duration * factor if duration and delta is not None else None,
            "unit": windows[0]["metrics"][name]["unit"]}
    invalid = collections.Counter(reason for window in windows for reason in window["invalid"].values())
    probes = [sample["desktop_probe"] for sample in samples]
    reports = [sample["zmem"].get("report") for sample in samples]
    sizes = [sample["kernel"][key] for sample in samples
             for key in ("page_size_before_bytes", "page_size_after_bytes")]
    return {
        "schema": "zmem.responsiveness.summary.v1", "requested_duration_s": requested,
        "observed_duration_s": elapsed, "sample_count": len(samples), "interrupted": interrupted,
        "page_size_consistent": bool(sizes) and sizes[0] is not None and all(size == sizes[0] for size in sizes),
        "scheduler_wakeup_lateness_ms": {**distribution(row["lateness_ns"] / 1_000_000 for row in wakeups),
            "skipped_deadlines": sum(row["skipped_deadlines"] for row in wakeups)},
        "hyprctl_process_and_ipc_round_trip_ms": {
            **distribution(probe["round_trip_ms"] for probe in probes if probe["status"] == "available"),
            "statuses": dict(collections.Counter(probe["status"] for probe in probes)),
            "timeouts": sum(probe.get("reason") == "timeout" for probe in probes),
            "is_frame_latency": False},
        "mem_available_bytes": distribution(sample["kernel"]["meminfo_bytes"]["MemAvailable"] for sample in samples),
        "window_metrics": metrics, "invalid_counter_reasons": dict(invalid),
        "kernel_read_span_ms": distribution(sample["kernel"]["read_span_ns"] / 1_000_000 for sample in samples),
        "sample_schedule_lateness_ms": distribution(sample["schedule_lateness_ns"] / 1_000_000 for sample in samples),
        "background_groups_observed": distribution(len(sample["kernel"]["background"]) if sample["kernel"]["background"] is not None else None for sample in samples),
        "background_progress_interpretation": "CPU time and I/O counter progress in sampled managed groups; not useful throughput or task completion",
        "zmem": {
            "statuses": dict(collections.Counter(sample["zmem"]["status"] for sample in samples)),
            "versions": sorted({value for report in reports if isinstance(value := nested(report, "version"), str)}),
            "report_sources": dict(collections.Counter(nested(report, "observation", "source") or "unknown" for report in reports)),
            "memory_pressure": dict(collections.Counter(nested(report, "memory_pressure") or "unknown" for report in reports)),
            "diagnostic_occurrences": sum(len(value) for report in reports if isinstance(value := nested(report, "errors"), list)),
        },
        "observer_resources": resources,
        "comparison_limit": "Natural-workload observation, not a controlled causal experiment",
    }


def measure(args):
    output = Outputs(args.output)
    programs = {name: shutil.which(name) for name in ("zmem", "hyprctl")}
    start = time.monotonic_ns()
    end = start + args.duration * 1_000_000_000
    self_start, children_start = resource.getrusage(resource.RUSAGE_SELF), resource.getrusage(resource.RUSAGE_CHILDREN)
    wakeups = Wakeups(start, end)
    metadata = {
        "schema": "zmem.responsiveness.metadata.v1", "started_unix_ns": time.time_ns(),
        "requested_duration_s": args.duration, "interval_ns": INTERVAL_NS,
        "scheduler_interval_ns": WAKE_INTERVAL_NS, "command_timeout_s": COMMAND_TIMEOUT_S,
        "desktop_probe_requested": args.desktop_probe, "programs_found": {name: path is not None for name, path in programs.items()},
        "page_size_bytes": page_size(), "kernel_release": platform.release(), "machine": platform.machine(),
        "python_version": platform.python_version(), "script_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "units": {"psi_total": "microseconds", "vmstat": "base pages", "block_sectors": "512 bytes"},
        "quantiles": "linear interpolation at (count - 1) * quantile",
        "window_clock": "monotonic midpoint of each sequential kernel snapshot; read span records non-atomic source skew",
        "scheduler_metric": "Python thread timed-wait lateness; includes scheduler, timer and interpreter effects, not input/frame latency",
        "desktop_metric": "hyprctl -j monitors process spawn + IPC + response time; monitor payload is discarded; not frame latency",
        "cpu_full": "System-level CPU full PSI is undefined; raw counter retained, derived percentage is null",
        "privacy": "No process command lines, environment contents, window titles or raw command stderr are collected",
        "references": ["https://docs.kernel.org/accounting/psi.html", "https://docs.kernel.org/block/stat.html",
                       "https://docs.kernel.org/admin-guide/blockdev/zram.html"],
    }
    output.write("metadata.json", metadata)
    samples, previous, interrupted = [], None, False
    wakeups.start()
    deadline = start
    try:
        while deadline <= end:
            time.sleep(max(0, deadline - time.monotonic_ns()) / 1_000_000_000)
            began = time.monotonic_ns()
            kernel = collect_kernel()
            sample = {"schema": "zmem.responsiveness.sample.v1", "index": len(samples),
                "scheduled_elapsed_ns": deadline - start, "schedule_lateness_ns": max(0, began - deadline),
                "kernel": kernel, "window": window_between(previous, kernel) if previous is not None else None,
                "zmem": zmem_probe(programs["zmem"]),
                "desktop_probe": desktop_probe(programs["hyprctl"], args.desktop_probe)}
            sample["collection_duration_ns"] = time.monotonic_ns() - began
            output.write("samples.jsonl", sample)
            samples.append(sample)
            previous = kernel
            if deadline == end:
                break
            next_deadline = min(deadline + INTERVAL_NS, end)
            now = time.monotonic_ns()
            if next_deadline < now and now < end:
                next_deadline = min(start + ((now - start) // INTERVAL_NS + 1) * INTERVAL_NS, end)
            elif now >= end:
                break
            deadline = next_deadline
    except KeyboardInterrupt:
        interrupted = True
    finally:
        wakeups.stop_event.set()
        wakeups.join(timeout=1)
    self_end, children_end = resource.getrusage(resource.RUSAGE_SELF), resource.getrusage(resource.RUSAGE_CHILDREN)
    resources = {"cpu_seconds": self_end.ru_utime + self_end.ru_stime - self_start.ru_utime - self_start.ru_stime,
        "probe_children_cpu_seconds": children_end.ru_utime + children_end.ru_stime - children_start.ru_utime - children_start.ru_stime,
        "peak_rss_bytes": self_end.ru_maxrss * 1024}
    summary = summarize(samples, wakeups.samples, args.duration, (time.monotonic_ns() - start) / 1_000_000_000, interrupted, resources)
    for row in wakeups.samples:
        output.write("scheduler.jsonl", row)
    output.write("summary.json", summary)
    output.close()
    print(json.dumps(summary, sort_keys=True, separators=(",", ":"), allow_nan=False))
    return 130 if interrupted else 0


def self_test():
    assert parse_psi("some avg10=1 total=100\nfull total=25\n") == {"some": 100, "full": 25}
    assert parse_psi("some total=-1\n")["some"] is None
    assert counter_delta(20, 19) == (None, "counter_reset")
    assert counter_delta(None, 0) == (None, "unavailable")
    assert counter_delta(10, 20) == (10, None)
    assert percentile([], .95) is None and percentile([8], .99) == 8
    assert percentile([0, 10, 20], .95) == 19
    counters, sources = parse_vmstat("pgscan_direct_dma 2\npgscan_direct_normal 3\npgscan_direct_throttle 99\n")
    assert counters["pgscan_direct"] == 5 and len(sources) == 2
    counters, sources = parse_vmstat("pgscan_direct 7\npgscan_direct_normal 3\n")
    assert counters["pgscan_direct"] == 7 and sources == ["pgscan_direct"]
    assert parse_meminfo("MemAvailable: 12 kB\nMemTotal: 40 kB\n")["MemAvailable"] == 12288
    assert parse_meminfo("MemFree: 12 kB\n")["MemAvailable"] is None
    old = {"x": {"identity": [1, 2], "v": 4}}
    new = {"x": {"identity": [1, 3], "v": 100}}
    assert aggregate_delta(old, new, lambda row: row["v"])[0] is None
    assert aggregate_delta({}, {}, lambda row: row["v"])[0] is None
    assert aggregate_delta({}, {}, lambda row: row["v"], empty_is_zero=True) == (0, None)
    first = {"g": {"identity": [1, 2], "io": {"1:1": {"rbytes": 100}, "1:2": {"rbytes": 100}}}}
    last = {"g": {"identity": [1, 2], "io": {"1:1": {"rbytes": 1}, "1:2": {"rbytes": 1000}}}}
    assert background_io_delta(first, last, "rbytes") == (None, "counter_reset")
    raw = {"name": "program", "nested": {"argv": ["secret"], "env": "secret", "value": 1}}
    assert strip_sensitive(raw) == {"name": "program", "nested": {"value": 1}}
    kernel = {"at_monotonic_ns": 0, "psi_totals_us": {key: {"some": 0, "full": 0} for key in ("memory", "io", "cpu")},
        "page_size_before_bytes": 4096, "page_size_after_bytes": 4096,
        "vmstat_pages": {key: 0 for key in (*VM_KEYS, "pgscan_direct")},
        "zram": {}, "background": {}, "background_truncated": False}
    later = json.loads(json.dumps(kernel))
    later["at_monotonic_ns"] = 2_000_000_000
    later["vmstat_pages"]["pswpin"] = 2
    window = window_between(kernel, later)
    assert window["metrics"]["paging.swap_in"]["value"] == 4096
    assert window["metrics"]["psi.cpu.full"]["value"] is None
    later["page_size_after_bytes"] = 65536
    assert window_between(kernel, later)["metrics"]["paging.swap_in"]["value"] is None
    print('{"self_test":"passed","groups":7}')
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--duration", type=int, default=30, help="requested seconds, usually 30 or 60; range 2..300")
    parser.add_argument("--output", type=Path, help="directory for private raw samples and summary; prefer ignored artifacts/")
    parser.add_argument("--desktop-probe", action="store_true", help="time a bounded hyprctl read request; not frame latency")
    parser.add_argument("--self-test", action="store_true", help="run pure-function regression checks without probing the system")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    if not 2 <= args.duration <= 300:
        parser.error("--duration must be between 2 and 300 seconds")
    if not sys.platform.startswith("linux"):
        parser.error("this observer requires Linux procfs/sysfs")
    try:
        return measure(args)
    except (OSError, ValueError) as error:
        print(f"measurement failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
