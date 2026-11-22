//! Opt-in KSM registration, bounded observations, and a conservative scan plan.
//!
//! This module never writes sysfs. The adapter must compare each Write's
//! `expected` value, use its ownership journal, read back the result, and feed
//! failures or external changes to Governor::feedback. Ownership is supplied
//! afresh from that journal, never inferred from a proposed write. A Plan's
//! state describes observations; target_state describes a requested change.
//!
//! Scope markers express opt-in, not a security boundary. KSM's matching is
//! global: UID and cgroup separation do not isolate deduplication trust domains.
//! Scan batch sizes are initial conservative policy, not a hard CPU quota.

use std::{
    fs::{self, File},
    io::{self, Read},
    path::Path,
};

pub const KSM_ROOT: &str = "/sys/kernel/mm/ksm";
pub const PAGES_TO_SCAN: u64 = 128;
pub const SLEEP_MILLISECS: u64 = 100;
pub const STABLE_MILLIS: u64 = 30_000;
pub const MAX_TASK_AGE_MILLIS: u64 = 10_000;
const READ_LIMIT: u64 = 16 * 1024;
const ERROR_LIMIT: usize = 8;
const HARD_PROCESS_LIMIT: usize = 16_384;
const HARD_TASK_LIMIT: usize = 512;

// PSI basis points: 100 = 1% of wall time. These are deliberately conservative
// initial gates for optional scanning, not general memory-pressure thresholds.
pub const CPU_SOME_BUSY_BP: u16 = 500;
pub const MEMORY_SOME_BUSY_BP: u16 = 100;
pub const MEMORY_FULL_BUSY_BP: u16 = 20;
pub const IO_FULL_BUSY_BP: u16 = 100;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Advisor {
    /// Older kernels without the advisor knob support manual batch control.
    Missing,
    None,
    External(String),
    Unreadable,
}

/// Kernels may expose a plain active mode or an option list with exactly one
/// bracketed active item, for example "[none] scan-time".
pub fn parse_advisor(text: &str) -> io::Result<Advisor> {
    let modes: Vec<_> = text.split_whitespace().collect();
    if modes.is_empty() || modes.len() > 16 {
        return Err(invalid("invalid KSM advisor mode list"));
    }
    let mut selected = None;
    let mut seen = Vec::new();
    for mode in &modes {
        let bracketed = mode.starts_with('[') || mode.ends_with(']');
        let value = if bracketed {
            let value = mode
                .strip_prefix('[')
                .and_then(|value| value.strip_suffix(']'))
                .ok_or_else(|| invalid("malformed KSM advisor selection"))?;
            if selected.replace(value).is_some() {
                return Err(invalid("ambiguous KSM advisor selection"));
            }
            value
        } else {
            *mode
        };
        if value.is_empty()
            || value.len() > 64
            || !value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
            || seen.contains(&value)
        {
            return Err(invalid("invalid KSM advisor mode"));
        }
        seen.push(value);
    }
    let selected = selected
        .or_else(|| (modes.len() == 1).then_some(modes[0]))
        .ok_or_else(|| invalid("missing KSM advisor selection"))?;
    Ok(if selected == "none" {
        Advisor::None
    } else {
        Advisor::External(selected.to_owned())
    })
}

#[derive(Clone, Debug)]
pub struct KsmSnapshot {
    /// None means that capability detection itself failed.
    pub available: Option<bool>,
    /// False on a failed required read or malformed/unreadable existing knob.
    /// Optional counters absent on an older kernel remain None without making
    /// the control observation incomplete.
    pub complete: bool,
    pub run: Option<u8>,
    pub pages_to_scan: Option<u64>,
    pub sleep_millisecs: Option<u64>,
    pub advisor: Advisor,
    pub general_profit_bytes: Option<i64>,
    pub full_scans: Option<u64>,
    pub pages_scanned: Option<u64>,
    pub pages_shared: Option<u64>,
    pub pages_sharing: Option<u64>,
    pub pages_unshared: Option<u64>,
    pub pages_volatile: Option<u64>,
    pub pages_skipped: Option<u64>,
    pub zero_pages: Option<u64>,
    pub errors: Vec<String>,
}

impl Default for KsmSnapshot {
    fn default() -> Self {
        Self {
            available: None,
            complete: false,
            run: None,
            pages_to_scan: None,
            sleep_millisecs: None,
            advisor: Advisor::Unreadable,
            general_profit_bytes: None,
            full_scans: None,
            pages_scanned: None,
            pages_shared: None,
            pages_sharing: None,
            pages_unshared: None,
            pages_volatile: None,
            pages_skipped: None,
            zero_pages: None,
            errors: Vec::new(),
        }
    }
}

fn limited_read(path: &Path) -> io::Result<String> {
    let mut text = String::new();
    File::open(path)?
        .take(READ_LIMIT + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > READ_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "read limit exceeded",
        ));
    }
    Ok(text)
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn unsigned(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() || !text.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn signed(text: &str) -> Option<i64> {
    let text = text.trim();
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn note(errors: &mut Vec<String>, message: String) {
    if errors.len() < ERROR_LIMIT {
        errors.push(message);
    }
}

fn observed<T>(
    root: &Path,
    name: &str,
    optional: bool,
    parser: impl FnOnce(&str) -> Option<T>,
    complete: &mut bool,
    errors: &mut Vec<String>,
) -> Option<T> {
    match limited_read(&root.join(name)) {
        Ok(text) => match parser(&text) {
            Some(value) => Some(value),
            None => {
                *complete = false;
                note(errors, format!("invalid KSM field: {name}"));
                None
            }
        },
        Err(error) if optional && error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            *complete = false;
            note(errors, format!("cannot read KSM {name}: {error}"));
            None
        }
    }
}

pub fn read_ksm(root: &Path) -> KsmSnapshot {
    let mut result = KsmSnapshot::default();
    match fs::metadata(root) {
        Ok(metadata) if metadata.is_dir() => {
            result.available = Some(true);
            result.complete = true;
        }
        Ok(_) => {
            note(&mut result.errors, "KSM path is not a directory".into());
            return result;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            result.available = Some(false);
            return result;
        }
        Err(error) => {
            note(&mut result.errors, format!("cannot inspect KSM: {error}"));
            return result;
        }
    }
    result.run = observed(
        root,
        "run",
        false,
        |text| unsigned(text).filter(|value| *value <= 2).map(|v| v as u8),
        &mut result.complete,
        &mut result.errors,
    );
    result.pages_to_scan = observed(
        root,
        "pages_to_scan",
        false,
        unsigned,
        &mut result.complete,
        &mut result.errors,
    );
    result.sleep_millisecs = observed(
        root,
        "sleep_millisecs",
        false,
        unsigned,
        &mut result.complete,
        &mut result.errors,
    );
    result.advisor = match limited_read(&root.join("advisor_mode")) {
        Ok(text) => match parse_advisor(&text) {
            Ok(advisor) => advisor,
            Err(_) => {
                result.complete = false;
                note(&mut result.errors, "invalid KSM advisor_mode".into());
                Advisor::Unreadable
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Advisor::Missing,
        Err(error) => {
            result.complete = false;
            note(
                &mut result.errors,
                format!("cannot read KSM advisor_mode: {error}"),
            );
            Advisor::Unreadable
        }
    };
    result.general_profit_bytes = observed(
        root,
        "general_profit",
        true,
        signed,
        &mut result.complete,
        &mut result.errors,
    );
    for (name, field) in [
        ("full_scans", &mut result.full_scans),
        ("pages_scanned", &mut result.pages_scanned),
        ("pages_shared", &mut result.pages_shared),
        ("pages_sharing", &mut result.pages_sharing),
        ("pages_unshared", &mut result.pages_unshared),
        ("pages_volatile", &mut result.pages_volatile),
        ("pages_skipped", &mut result.pages_skipped),
        ("ksm_zero_pages", &mut result.zero_pages),
    ] {
        *field = observed(
            root,
            name,
            true,
            unsigned,
            &mut result.complete,
            &mut result.errors,
        );
    }
    result
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProcessKsm {
    pub merge_any: Option<bool>,
    pub mergeable: Option<bool>,
    pub merging_pages: Option<u64>,
    pub zero_pages: Option<u64>,
    pub rmap_items: Option<u64>,
    pub profit_bytes: Option<i64>,
}

pub fn parse_process_ksm(text: &str) -> io::Result<ProcessKsm> {
    let mut result = ProcessKsm::default();
    let mut seen = 0u8;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else { continue };
        let key = key.trim_end_matches(':');
        let bit = match key {
            "ksm_merge_any" => 1,
            "ksm_mergeable" => 2,
            "ksm_merging_pages" => 4,
            "ksm_zero_pages" => 8,
            "ksm_rmap_items" => 16,
            "ksm_process_profit" => 32,
            _ => continue,
        };
        if seen & bit != 0 {
            return Err(invalid("duplicate process KSM field"));
        }
        seen |= bit;
        let value = parts
            .next()
            .ok_or_else(|| invalid("missing process KSM value"))?;
        if parts.next().is_some() {
            return Err(invalid("invalid process KSM value"));
        }
        match key {
            "ksm_merge_any" | "ksm_mergeable" => {
                let value = match value {
                    "yes" => true,
                    "no" => false,
                    _ => return Err(invalid("invalid process KSM boolean")),
                };
                if key == "ksm_merge_any" {
                    result.merge_any = Some(value);
                } else {
                    result.mergeable = Some(value);
                }
            }
            "ksm_process_profit" => {
                result.profit_bytes =
                    Some(signed(value).ok_or_else(|| invalid("invalid KSM profit"))?);
            }
            _ => {
                let value =
                    unsigned(value).ok_or_else(|| invalid("invalid process KSM counter"))?;
                match key {
                    "ksm_merging_pages" => result.merging_pages = Some(value),
                    "ksm_zero_pages" => result.zero_pages = Some(value),
                    "ksm_rmap_items" => result.rmap_items = Some(value),
                    _ => unreachable!(),
                }
            }
        }
    }
    Ok(result)
}

fn canonical_id(text: &str) -> Option<u32> {
    let number = unsigned(text)?;
    let number = u32::try_from(number).ok()?;
    (number.to_string() == text).then_some(number)
}

/// Only exact system/user managed slices and an explicitly marked scope match.
/// Descendants of such a scope inherit its opt-in. A matching user slice and
/// user manager ID are required; arbitrary ancestor paths are never accepted.
pub fn is_managed_dedup_path(path: &str) -> bool {
    if !path.starts_with('/') || path.len() > 4096 || path.chars().any(char::is_control) {
        return false;
    }
    let components: Vec<_> = path[1..].split('/').collect();
    if components
        .iter()
        .any(|part| matches!(*part, "" | "." | ".."))
    {
        return false;
    }
    let offset = if components.first() == Some(&"user.slice") {
        let Some(user) = components
            .get(1)
            .and_then(|part| part.strip_prefix("user-"))
            .and_then(|part| part.strip_suffix(".slice"))
        else {
            return false;
        };
        if canonical_id(user).is_none()
            || components.get(2).copied() != Some(format!("user@{user}.service").as_str())
        {
            return false;
        }
        3
    } else {
        0
    };
    if components.get(offset) != Some(&"zmem.slice")
        || components.get(offset + 1) != Some(&"zmem-background.slice")
    {
        return false;
    }
    let Some(scope) = components
        .get(offset + 2)
        .and_then(|scope| scope.strip_prefix("zmem-"))
        .and_then(|scope| scope.strip_suffix(".scope"))
    else {
        return false;
    };
    let Some((class, id)) = scope.split_once("-dedup-") else {
        return false;
    };
    matches!(class, "background" | "disposable") && canonical_id(id).is_some_and(|id| id > 0)
}

fn unified_cgroup(text: &str) -> io::Result<&str> {
    let mut paths = text.lines().filter_map(|line| line.strip_prefix("0::"));
    let path = paths
        .next()
        .ok_or_else(|| invalid("missing unified cgroup"))?;
    if paths.next().is_some() || !path.starts_with('/') {
        return Err(invalid("invalid unified cgroup"));
    }
    Ok(path)
}

fn start_ticks(text: &str, pid: u32) -> io::Result<u64> {
    let (head, tail) = text
        .rsplit_once(") ")
        .ok_or_else(|| invalid("invalid process stat"))?;
    if head.split_whitespace().next().and_then(canonical_id) != Some(pid) {
        return Err(invalid("process stat PID mismatch"));
    }
    tail.split_whitespace()
        .nth(19)
        .and_then(unsigned)
        .ok_or_else(|| invalid("missing process start time"))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisteredTask {
    pub pid: u32,
    pub start_ticks: u64,
    pub cgroup: String,
    pub ksm: ProcessKsm,
}

#[derive(Clone, Copy, Debug)]
pub struct ScanLimits {
    pub max_processes: usize,
    pub max_tasks: usize,
}

impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            max_processes: 4096,
            max_tasks: 256,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct TaskScan {
    pub tasks: Vec<RegisteredTask>,
    pub processes_examined: usize,
    pub marked_processes: usize,
    pub complete: bool,
    pub truncated: bool,
    pub errors: Vec<String>,
}

impl TaskScan {
    fn error(&mut self, message: String) {
        self.complete = false;
        note(&mut self.errors, message);
    }
}

/// Bounded process-leader traversal. Only cgroup membership is read for
/// unrelated processes. A marked process must have stable PID identity and
/// membership around its KSM read. ENOENT from an exited process is benign;
/// permission, format, enumeration, and budget failures are incomplete coverage.
pub fn scan_tasks(proc_root: &Path, limits: ScanLimits) -> TaskScan {
    let mut result = TaskScan {
        complete: true,
        ..TaskScan::default()
    };
    let process_limit = limits.max_processes.min(HARD_PROCESS_LIMIT);
    let task_limit = limits.max_tasks.min(HARD_TASK_LIMIT);
    let entries = match fs::read_dir(proc_root) {
        Ok(entries) => entries,
        Err(error) => {
            result.error(format!("cannot enumerate processes: {error}"));
            return result;
        }
    };
    for (entry_index, entry) in entries.enumerate() {
        if entry_index >= process_limit.saturating_add(256) {
            result.truncated = true;
            result.error("process directory entry limit reached".into());
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                result.error(format!("cannot enumerate process entry: {error}"));
                continue;
            }
        };
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(canonical_id).filter(|pid| *pid > 0) else {
            continue;
        };
        if result.processes_examined >= process_limit {
            result.truncated = true;
            result.error("process scan limit reached".into());
            break;
        }
        result.processes_examined += 1;
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            _ => {
                result.error(format!("cannot validate process directory {pid}"));
                continue;
            }
        }
        let directory = entry.path();
        let cgroup = match limited_read(&directory.join("cgroup"))
            .and_then(|text| unified_cgroup(&text).map(str::to_owned))
        {
            Ok(cgroup) => cgroup,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                result.error(format!("cannot inspect process {pid} membership: {error}"));
                continue;
            }
        };
        if !is_managed_dedup_path(&cgroup) {
            continue;
        }
        result.marked_processes += 1;
        let inspect = || -> io::Result<Option<RegisteredTask>> {
            let before = start_ticks(&limited_read(&directory.join("stat"))?, pid)?;
            let ksm = parse_process_ksm(&limited_read(&directory.join("ksm_stat"))?)?;
            let after = start_ticks(&limited_read(&directory.join("stat"))?, pid)?;
            let membership = limited_read(&directory.join("cgroup"))?;
            if before != after || unified_cgroup(&membership)? != cgroup {
                return Err(invalid("process identity or membership changed"));
            }
            match ksm.merge_any {
                Some(true) => Ok(Some(RegisteredTask {
                    pid,
                    start_ticks: before,
                    cgroup: cgroup.clone(),
                    ksm,
                })),
                Some(false) => Ok(None),
                None => Err(invalid("process KSM registration is unavailable")),
            }
        };
        match inspect() {
            Ok(Some(task)) => {
                if result.tasks.len() >= task_limit {
                    result.truncated = true;
                    result.error("registered task limit reached".into());
                    break;
                }
                result.tasks.push(task);
            }
            Ok(None) => {}
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    && fs::metadata(&directory)
                        .is_err_and(|error| error.kind() == io::ErrorKind::NotFound) => {}
            Err(error) => result.error(format!("cannot validate opted-in process {pid}: {error}")),
        }
    }
    result.tasks.sort_unstable_by_key(|task| task.pid);
    result
}

/// Parse avg10 without floating point, rejecting unknown, negative and >100%
/// values instead of accidentally classifying them as healthy.
pub fn parse_psi_bp(text: &str, kind: &str) -> Option<u16> {
    let mut matching = text
        .lines()
        .filter(|line| line.split_whitespace().next() == Some(kind));
    let line = matching.next()?;
    if matching.next().is_some() {
        return None;
    }
    let mut values = line
        .split_whitespace()
        .filter_map(|field| field.strip_prefix("avg10="));
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    let whole = unsigned(whole)?;
    let fraction = match fraction.len() {
        0 => 0,
        1 => unsigned(fraction)? * 10,
        2 => unsigned(fraction)?,
        _ => return None,
    };
    let bp = whole.checked_mul(100)?.checked_add(fraction)?;
    (bp <= 10_000).then_some(bp as u16)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PressureInput {
    pub cpu_busy: Option<bool>,
    pub memory_busy: Option<bool>,
    pub io_busy: Option<bool>,
}

impl PressureInput {
    fn complete(self) -> bool {
        self.cpu_busy.is_some() && self.memory_busy.is_some() && self.io_busy.is_some()
    }

    fn reason(self) -> Option<&'static str> {
        if !self.complete() {
            Some("pressure_observation_incomplete")
        } else if self.memory_busy == Some(true) {
            Some("memory_pressure")
        } else if self.io_busy == Some(true) {
            Some("io_pressure")
        } else if self.cpu_busy == Some(true) {
            Some("cpu_pressure")
        } else {
            None
        }
    }
}

/// The caller supplies its headroom / memory-policy judgement. Missing PSI or
/// a missing caller judgement yields None, never a false healthy observation.
/// The caller may additionally OR a measured paging/IO-congestion signal into
/// io_busy while preserving None when the PSI read failed.
pub fn read_pressure(proc_root: &Path, memory_healthy: Option<bool>) -> PressureInput {
    let cpu = limited_read(&proc_root.join("pressure/cpu")).ok();
    let memory = limited_read(&proc_root.join("pressure/memory")).ok();
    let io = limited_read(&proc_root.join("pressure/io")).ok();
    let memory_busy = (|| {
        let text = memory.as_deref()?;
        Some(
            !memory_healthy?
                || parse_psi_bp(text, "some")? >= MEMORY_SOME_BUSY_BP
                || parse_psi_bp(text, "full")? >= MEMORY_FULL_BUSY_BP,
        )
    })();
    PressureInput {
        cpu_busy: cpu
            .as_deref()
            .and_then(|text| parse_psi_bp(text, "some"))
            .map(|bp| bp >= CPU_SOME_BUSY_BP),
        memory_busy,
        io_busy: io
            .as_deref()
            .and_then(|text| parse_psi_bp(text, "full"))
            .map(|bp| bp >= IO_FULL_BUSY_BP),
    }
}

#[derive(Clone, Debug)]
pub struct Observation {
    pub ksm: KsmSnapshot,
    pub tasks: TaskScan,
    /// The adapter can cache task discovery for ten seconds, while sampling
    /// KSM and pressure each cycle. Unknown/stale cache age cannot enable scans.
    pub task_age_ms: Option<u64>,
}

/// Last successfully owned values, with identity and pending-write validation
/// performed by the caller. None is unowned, not an expected zero.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Ownership {
    pub run: Option<u8>,
    pub pages_to_scan: Option<u64>,
    pub sleep_millisecs: Option<u64>,
}

impl Ownership {
    fn mismatches(self, observed: &KsmSnapshot) -> bool {
        self.run.zip(observed.run).is_some_and(|(a, b)| a != b)
            || self
                .pages_to_scan
                .zip(observed.pages_to_scan)
                .is_some_and(|(a, b)| a != b)
            || self
                .sleep_millisecs
                .zip(observed.sleep_millisecs)
                .is_some_and(|(a, b)| a != b)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Knob {
    Run,
    PagesToScan,
    SleepMillisecs,
}

impl Knob {
    pub const fn path(self) -> &'static str {
        match self {
            Self::Run => "/sys/kernel/mm/ksm/run",
            Self::PagesToScan => "/sys/kernel/mm/ksm/pages_to_scan",
            Self::SleepMillisecs => "/sys/kernel/mm/ksm/sleep_millisecs",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Write {
    pub knob: Knob,
    /// Compare again immediately before using the ownership-aware writer.
    pub expected: u64,
    pub value: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
    Idle,
    Scanning,
    PausedPressure,
    Warming,
    ExternallyManaged,
    Unavailable,
}

impl State {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Scanning => "scanning",
            Self::PausedPressure => "paused_pressure",
            Self::Warming => "warming",
            Self::ExternallyManaged => "externally_managed",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Plan {
    /// Actual observed state: run=1 still reports scanning when a pause write
    /// is pending/failed. No proposal alone can produce a scanning state.
    pub state: State,
    pub target_state: State,
    pub reason: &'static str,
    /// Ordered writes; never contains run=2 or a process-unmerge operation.
    pub writes: Vec<Write>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplyOutcome {
    Applied,
    Failed,
    ExternalChange,
}

#[derive(Debug)]
pub struct Governor {
    stable_since_ms: Option<u64>,
    previous_ms: Option<u64>,
    max_observation_gap_ms: u64,
    external_change: bool,
    write_failed: bool,
}

impl Default for Governor {
    fn default() -> Self {
        Self::new(2000)
    }
}

impl Governor {
    /// The expected sampling interval bounds gaps in the stability history.
    /// A delayed/missed observation restarts the 30-second healthy interval.
    pub fn new(sample_interval_ms: u64) -> Self {
        Self {
            stable_since_ms: None,
            previous_ms: None,
            max_observation_gap_ms: sample_interval_ms.clamp(250, 60_000).saturating_mul(3),
            external_change: false,
            write_failed: false,
        }
    }

    /// Call only for an actual attempted transaction, including compare/readback
    /// failures. Applied clears an error, but never grants ownership or declares
    /// success: the next observation and journal must establish that.
    pub fn feedback(&mut self, outcome: ApplyOutcome) {
        match outcome {
            ApplyOutcome::Applied => self.write_failed = false,
            ApplyOutcome::Failed => {
                self.write_failed = true;
                self.stable_since_ms = None;
            }
            ApplyOutcome::ExternalChange => {
                self.external_change = true;
                self.stable_since_ms = None;
            }
        }
    }

    fn pause(
        &mut self,
        ksm: &KsmSnapshot,
        ownership: Ownership,
        target: State,
        reason: &'static str,
    ) -> Plan {
        self.stable_since_ms = None;
        let owned_running = ksm.run == Some(1) && ownership.run == Some(1);
        Plan {
            state: if owned_running {
                State::Scanning
            } else {
                target
            },
            target_state: target,
            reason,
            writes: if owned_running {
                vec![Write {
                    knob: Knob::Run,
                    expected: 1,
                    value: 0,
                }]
            } else {
                Vec::new()
            },
        }
    }

    pub fn plan(
        &mut self,
        now_ms: u64,
        enabled: bool,
        observed: &Observation,
        pressure: PressureInput,
        ownership: Ownership,
    ) -> Plan {
        if self.previous_ms.is_some_and(|previous| {
            now_ms < previous || now_ms.saturating_sub(previous) > self.max_observation_gap_ms
        }) {
            self.stable_since_ms = None;
        }
        self.previous_ms = Some(now_ms);
        let ksm = &observed.ksm;
        if ownership.mismatches(ksm) {
            self.external_change = true;
        }
        // A knob already at our target on acquisition may have no journal
        // entry, because no write was necessary. Owning run proves that the
        // fixed scan parameters were required before we enabled it. Preserve
        // later changes to such an unowned knob instead of silently reacquiring
        // it after pausing. An unreadable value is incomplete, not proven drift.
        if ownership.run.is_some()
            && ((ownership.pages_to_scan.is_none()
                && ksm
                    .pages_to_scan
                    .is_some_and(|value| value != PAGES_TO_SCAN))
                || (ownership.sleep_millisecs.is_none()
                    && ksm
                        .sleep_millisecs
                        .is_some_and(|value| value != SLEEP_MILLISECS)))
        {
            self.external_change = true;
        }
        let external = self.external_change
            || matches!(ksm.advisor, Advisor::External(_))
            || ksm.run == Some(2)
            || (ksm.run == Some(1) && ownership.run.is_none());
        if external {
            self.stable_since_ms = None;
            return Plan {
                state: State::ExternallyManaged,
                target_state: State::ExternallyManaged,
                reason: if self.external_change {
                    "external_control_change"
                } else if matches!(ksm.advisor, Advisor::External(_)) {
                    "external_advisor"
                } else {
                    "existing_external_ksm"
                },
                writes: Vec::new(),
            };
        }
        if ksm.available != Some(true)
            || !ksm.complete
            || ksm.run.is_none()
            || ksm.pages_to_scan.is_none()
            || ksm.sleep_millisecs.is_none()
            || matches!(ksm.advisor, Advisor::Unreadable)
        {
            return self.pause(
                ksm,
                ownership,
                State::Unavailable,
                "ksm_observation_incomplete",
            );
        }
        if !enabled {
            return self.pause(ksm, ownership, State::Idle, "dedup_disabled");
        }
        if !observed.tasks.complete
            || observed
                .task_age_ms
                .is_none_or(|age| age > MAX_TASK_AGE_MILLIS)
        {
            return self.pause(
                ksm,
                ownership,
                State::PausedPressure,
                "task_observation_incomplete",
            );
        }
        if observed.tasks.tasks.is_empty() {
            return self.pause(ksm, ownership, State::Idle, "no_registered_tasks");
        }
        if let Some(reason) = pressure.reason() {
            return self.pause(ksm, ownership, State::PausedPressure, reason);
        }
        let fixed_parameters = ksm.pages_to_scan == Some(PAGES_TO_SCAN)
            && ksm.sleep_millisecs == Some(SLEEP_MILLISECS);
        if ksm.run == Some(1) {
            if !fixed_parameters {
                // Pause an owned run switch before adjusting parameters. Never
                // rewrite live knobs, even when a previous journal owned them.
                return self.pause(
                    ksm,
                    ownership,
                    State::Warming,
                    "scan_parameters_need_update",
                );
            }
            return Plan {
                state: State::Scanning,
                target_state: State::Scanning,
                reason: "registered_tasks_scanning",
                writes: Vec::new(),
            };
        }
        let since = *self.stable_since_ms.get_or_insert(now_ms);
        if now_ms.saturating_sub(since) < STABLE_MILLIS {
            return Plan {
                state: State::Warming,
                target_state: State::Scanning,
                reason: if self.write_failed {
                    "write_failed_waiting_for_stability"
                } else {
                    "waiting_for_stable_headroom"
                },
                writes: Vec::new(),
            };
        }
        let mut writes = Vec::new();
        if ksm.pages_to_scan != Some(PAGES_TO_SCAN) {
            writes.push(Write {
                knob: Knob::PagesToScan,
                expected: ksm.pages_to_scan.expect("checked control"),
                value: PAGES_TO_SCAN,
            });
        }
        if ksm.sleep_millisecs != Some(SLEEP_MILLISECS) {
            writes.push(Write {
                knob: Knob::SleepMillisecs,
                expected: ksm.sleep_millisecs.expect("checked control"),
                value: SLEEP_MILLISECS,
            });
        }
        // Enabling is last. The adapter must stop on the first write, compare,
        // or readback failure, then report failure instead of continuing here.
        writes.push(Write {
            knob: Knob::Run,
            expected: 0,
            value: 1,
        });
        Plan {
            state: State::Warming,
            target_state: State::Scanning,
            reason: "scan_start_requested",
            writes,
        }
    }
}

/// Enable process-level KSM only for this explicitly selected invocation and
/// exec the original command. The flag is inherited by fork and exec. Normal
/// run paths must not call this function. No disable/unmerge syscall is used,
/// including when exec fails. Kernel unsupported/denied errors are explicit.
#[cfg(target_os = "linux")]
pub fn exec_registered(args: &[std::ffi::OsString]) -> io::Result<()> {
    use std::{
        ffi::{c_int, c_ulong},
        os::unix::process::CommandExt,
        process::Command,
    };
    unsafe extern "C" {
        fn prctl(option: c_int, ...) -> c_int;
    }
    const PR_SET_MEMORY_MERGE: c_int = 67;
    const PR_GET_MEMORY_MERGE: c_int = 68;
    let command = args.first().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "dedup exec requires a command")
    })?;
    // prctl is variadic: all optional arguments must have the native unsigned
    // long width, including zero, on both 32-bit and 64-bit Linux.
    if unsafe {
        prctl(
            PR_SET_MEMORY_MERGE,
            1 as c_ulong,
            0 as c_ulong,
            0 as c_ulong,
            0 as c_ulong,
        )
    } != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("cannot enable process KSM: {}", io::Error::last_os_error()),
        ));
    }
    let actual = unsafe {
        prctl(
            PR_GET_MEMORY_MERGE,
            0 as c_ulong,
            0 as c_ulong,
            0 as c_ulong,
            0 as c_ulong,
        )
    };
    if actual != 1 {
        return Err(io::Error::other(if actual < 0 {
            format!("cannot verify process KSM: {}", io::Error::last_os_error())
        } else {
            "process KSM readback differs from requested value".into()
        }));
    }
    Err(Command::new(command).args(&args[1..]).exec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static SERIAL: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "zmem-dedup-test-{}-{}",
                std::process::id(),
                SERIAL.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn put(&self, path: &str, value: &str) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, value).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn healthy() -> PressureInput {
        PressureInput {
            cpu_busy: Some(false),
            memory_busy: Some(false),
            io_busy: Some(false),
        }
    }
    fn stopped() -> Observation {
        Observation {
            ksm: KsmSnapshot {
                available: Some(true),
                complete: true,
                run: Some(0),
                pages_to_scan: Some(100),
                sleep_millisecs: Some(20),
                advisor: Advisor::None,
                ..KsmSnapshot::default()
            },
            tasks: TaskScan {
                complete: true,
                tasks: vec![RegisteredTask {
                    pid: 123,
                    start_ticks: 42,
                    cgroup: "/zmem.slice/zmem-background.slice/zmem-background-dedup-123.scope"
                        .into(),
                    ksm: ProcessKsm {
                        merge_any: Some(true),
                        ..ProcessKsm::default()
                    },
                }],
                ..TaskScan::default()
            },
            task_age_ms: Some(0),
        }
    }
    fn owned_running() -> (Observation, Ownership) {
        let mut observation = stopped();
        observation.ksm.run = Some(1);
        observation.ksm.pages_to_scan = Some(PAGES_TO_SCAN);
        observation.ksm.sleep_millisecs = Some(SLEEP_MILLISECS);
        (
            observation,
            Ownership {
                run: Some(1),
                pages_to_scan: Some(PAGES_TO_SCAN),
                sleep_millisecs: Some(SLEEP_MILLISECS),
            },
        )
    }
    fn advance(
        governor: &mut Governor,
        observation: &Observation,
        from: u64,
        through: u64,
    ) -> Plan {
        let mut plan = governor.plan(from, true, observation, healthy(), Ownership::default());
        for now in ((from + 2000)..=through).step_by(2000) {
            plan = governor.plan(now, true, observation, healthy(), Ownership::default());
        }
        plan
    }
    fn assert_pause(plan: &Plan, target: State) {
        assert_eq!(
            plan.state,
            State::Scanning,
            "a pending pause must not claim that run is already zero"
        );
        assert_eq!(plan.target_state, target);
        assert_eq!(
            plan.writes,
            [Write {
                knob: Knob::Run,
                expected: 1,
                value: 0
            }]
        );
    }

    #[test]
    fn accepts_only_exact_managed_scope_markers() {
        for path in [
            "/zmem.slice/zmem-background.slice/zmem-background-dedup-42.scope",
            "/zmem.slice/zmem-background.slice/zmem-disposable-dedup-42.scope/child",
            "/user.slice/user-1000.slice/user@1000.service/zmem.slice/zmem-background.slice/zmem-background-dedup-42.scope",
        ] {
            assert!(is_managed_dedup_path(path), "{path}");
        }
        for path in [
            "/zmem.slice/zmem-background.slice/zmem-background-42.scope",
            "/zmem.slice/zmem-background.slice/zmem-interactive-dedup-42.scope",
            "/zmem.slice/zmem-background.slice/zmem-background-dedup-0.scope",
            "/zmem.slice/zmem-background.slice/zmem-background-dedup-042.scope",
            "/zmem.slice/zmem-background.slice/zmem-background-dedup-42.scope-extra",
            "/other.slice/zmem.slice/zmem-background.slice/zmem-background-dedup-42.scope",
            "/zmem.slice/zmem-background.slice-impostor/zmem-background-dedup-42.scope",
            "/user.slice/user-1000.slice/user@1001.service/zmem.slice/zmem-background.slice/zmem-background-dedup-42.scope",
            "/user.slice/user-name.slice/user@name.service/zmem.slice/zmem-background.slice/zmem-background-dedup-42.scope",
            "/zmem.slice/zmem-background.slice/zmem-background-dedup-42.scope/../escape",
            "/zmem.slice//zmem-background.slice/zmem-background-dedup-42.scope",
            "/zmem.slice/zmem-background.slice/zmem-background-dedup-42.scope/\nchild",
            "zmem.slice/zmem-background.slice/zmem-background-dedup-42.scope",
        ] {
            assert!(!is_managed_dedup_path(path), "{path:?}");
        }
    }

    #[test]
    fn process_parser_preserves_unknown_and_signed_profit() {
        let stats = parse_process_ksm("ksm_merge_any: yes\nksm_mergeable: no\nksm_process_profit -64\nksm_merging_pages 0\nfuture_field 44\n").unwrap();
        assert_eq!(stats.merge_any, Some(true));
        assert_eq!(stats.profit_bytes, Some(-64));
        assert_eq!(stats.rmap_items, None);
        assert_eq!(parse_process_ksm("").unwrap().merge_any, None);
        for text in [
            "ksm_merge_any: maybe",
            "ksm_merge_any: yes\nksm_merge_any: no",
            "ksm_rmap_items -1",
            "ksm_process_profit 9223372036854775808",
            "ksm_zero_pages 1 trailing",
        ] {
            assert!(parse_process_ksm(text).is_err(), "{text}");
        }
    }

    #[test]
    fn advisor_list_requires_one_unambiguous_current_mode() {
        for text in ["none", "[none]", "[none] scan-time", "scan-time [none]\n"] {
            assert_eq!(parse_advisor(text).unwrap(), Advisor::None);
        }
        for text in ["scan-time", "none [scan-time]"] {
            assert_eq!(
                parse_advisor(text).unwrap(),
                Advisor::External("scan-time".into())
            );
        }
        for text in [
            "",
            "none scan-time",
            "[none] [scan-time]",
            "[none scan-time",
            "none] scan-time",
            "[none] none",
            "[[none]]",
            "[]",
            "[none] scan_time",
        ] {
            assert!(parse_advisor(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn sysfs_capabilities_and_missing_counters_are_not_zero() {
        let fixture = Fixture::new();
        fixture.put("run", "0\n");
        fixture.put("pages_to_scan", "100\n");
        fixture.put("sleep_millisecs", "20\n");
        fixture.put("general_profit", "-128\n");
        let snapshot = read_ksm(&fixture.0);
        assert!(snapshot.complete);
        assert_eq!(snapshot.advisor, Advisor::Missing);
        assert_eq!(snapshot.pages_scanned, None);
        assert_eq!(snapshot.general_profit_bytes, Some(-128));
        fixture.put("advisor_mode", "[none] scan-time\n");
        assert_eq!(read_ksm(&fixture.0).advisor, Advisor::None);
        fixture.put("run", "3");
        let snapshot = read_ksm(&fixture.0);
        assert!(!snapshot.complete);
        assert_eq!(snapshot.run, None);
        assert_eq!(read_ksm(&fixture.0.join("absent")).available, Some(false));
    }

    #[test]
    fn unreadable_existing_counter_is_incomplete_not_zero() {
        let fixture = Fixture::new();
        fixture.put("run", "0");
        fixture.put("pages_to_scan", "100");
        fixture.put("sleep_millisecs", "20");
        fs::create_dir(fixture.0.join("general_profit")).unwrap();
        let snapshot = read_ksm(&fixture.0);
        assert!(!snapshot.complete);
        assert_eq!(snapshot.general_profit_bytes, None);
        assert!(!snapshot.errors.is_empty());
    }

    fn process(fixture: &Fixture, pid: u32, path: &str, merge_any: &str) {
        fixture.put(&format!("{pid}/cgroup"), &format!("0::{path}\n"));
        let mut fields = vec!["0"; 20];
        fields[0] = "S";
        fields[19] = "12345";
        fixture.put(
            &format!("{pid}/stat"),
            &format!("{pid} (name with ) parentheses) {}", fields.join(" ")),
        );
        fixture.put(
            &format!("{pid}/ksm_stat"),
            &format!("ksm_merge_any: {merge_any}\nksm_process_profit -64\n"),
        );
    }

    #[test]
    fn scan_requires_both_marker_and_kernel_registration() {
        let fixture = Fixture::new();
        let marked = "/zmem.slice/zmem-background.slice/zmem-background-dedup-41.scope";
        process(&fixture, 41, marked, "yes");
        process(&fixture, 42, marked, "no");
        process(
            &fixture,
            43,
            "/zmem.slice/zmem-background.slice/zmem-background-43.scope",
            "yes",
        );
        let result = scan_tasks(&fixture.0, ScanLimits::default());
        assert!(result.complete, "{:?}", result.errors);
        assert_eq!(result.tasks.len(), 1);
        assert_eq!(result.tasks[0].pid, 41);
        assert_eq!(result.tasks[0].start_ticks, 12345);
        assert_eq!(result.marked_processes, 2);
    }

    #[test]
    fn budget_and_missing_registration_are_incomplete() {
        let fixture = Fixture::new();
        let marked = "/zmem.slice/zmem-background.slice/zmem-background-dedup-41.scope";
        process(&fixture, 41, marked, "yes");
        process(&fixture, 42, marked, "yes");
        let result = scan_tasks(
            &fixture.0,
            ScanLimits {
                max_processes: 1,
                max_tasks: 10,
            },
        );
        assert!(!result.complete && result.truncated);
        assert_eq!(result.processes_examined, 1);
        let result = scan_tasks(
            &fixture.0,
            ScanLimits {
                max_processes: 10,
                max_tasks: 1,
            },
        );
        assert!(!result.complete && result.truncated);
        assert_eq!(result.tasks.len(), 1);
        fixture.put("41/ksm_stat", "ksm_rmap_items 0\n");
        let result = scan_tasks(&fixture.0, ScanLimits::default());
        assert!(!result.complete);
        assert_eq!(result.tasks.len(), 1);
    }

    #[test]
    fn inaccessible_membership_is_not_treated_as_no_tasks() {
        let fixture = Fixture::new();
        fixture.put(
            "41/cgroup/child",
            "directory instead of a readable proc file",
        );
        let result = scan_tasks(&fixture.0, ScanLimits::default());
        assert!(!result.complete);
        assert!(result.tasks.is_empty());
        assert!(!result.errors.is_empty());
    }

    #[test]
    fn psi_is_bounded_and_unknown_is_preserved() {
        assert_eq!(
            parse_psi_bp("some avg10=5.25 avg60=4.00 total=1", "some"),
            Some(525)
        );
        assert_eq!(parse_psi_bp("full avg10=100.00", "full"), Some(10_000));
        for text in [
            "some avg10=NaN",
            "some avg10=-1",
            "some avg10=100.01",
            "some avg10=1.234",
            "some total=0",
            "some avg10=1 avg10=2",
            "some avg10=1\nsome avg10=2",
        ] {
            assert_eq!(parse_psi_bp(text, "some"), None, "{text}");
        }
        let fixture = Fixture::new();
        assert_eq!(
            read_pressure(&fixture.0, Some(true)),
            PressureInput::default()
        );
    }

    #[test]
    fn starting_requires_thirty_observed_seconds_and_readback() {
        let mut governor = Governor::default();
        let observation = stopped();
        assert!(
            advance(&mut governor, &observation, 0, 28_000)
                .writes
                .is_empty()
        );
        let plan = governor.plan(30_000, true, &observation, healthy(), Ownership::default());
        assert_eq!(plan.state, State::Warming);
        assert_eq!(plan.target_state, State::Scanning);
        assert_eq!(
            plan.writes,
            [
                Write {
                    knob: Knob::PagesToScan,
                    expected: 100,
                    value: 128
                },
                Write {
                    knob: Knob::SleepMillisecs,
                    expected: 20,
                    value: 100
                },
                Write {
                    knob: Knob::Run,
                    expected: 0,
                    value: 1
                },
            ]
        );
        governor.feedback(ApplyOutcome::Applied);
        assert_eq!(
            governor
                .plan(32_000, true, &observation, healthy(), Ownership::default())
                .state,
            State::Warming
        );
        let (running, ownership) = owned_running();
        assert_eq!(
            governor
                .plan(34_000, true, &running, healthy(), ownership)
                .state,
            State::Scanning
        );
    }

    #[test]
    fn pressure_unknown_and_disabled_pause_only_owned_scanning() {
        let (mut observation, ownership) = owned_running();
        let mut governor = Governor::default();
        for pressure in [
            PressureInput {
                cpu_busy: Some(true),
                ..healthy()
            },
            PressureInput {
                memory_busy: Some(true),
                ..healthy()
            },
            PressureInput {
                io_busy: Some(true),
                ..healthy()
            },
            PressureInput {
                cpu_busy: None,
                ..healthy()
            },
        ] {
            assert_pause(
                &governor.plan(0, true, &observation, pressure, ownership),
                State::PausedPressure,
            );
        }
        observation.tasks.complete = false;
        assert_pause(
            &governor.plan(0, true, &observation, healthy(), ownership),
            State::PausedPressure,
        );
        observation.tasks.complete = true;
        observation.tasks.tasks.clear();
        assert_pause(
            &governor.plan(0, true, &observation, healthy(), ownership),
            State::Idle,
        );
        assert_pause(
            &governor.plan(0, false, &observation, healthy(), ownership),
            State::Idle,
        );
        observation.ksm.complete = false;
        assert_pause(
            &governor.plan(0, true, &observation, healthy(), ownership),
            State::Unavailable,
        );
        let unowned = governor.plan(0, true, &observation, healthy(), Ownership::default());
        assert_eq!(unowned.state, State::ExternallyManaged);
        assert!(unowned.writes.is_empty());
    }

    #[test]
    fn recovery_and_write_failure_restart_stability() {
        let mut governor = Governor::default();
        let mut observation = stopped();
        advance(&mut governor, &observation, 0, 30_000);
        governor.feedback(ApplyOutcome::Failed);
        let plan = advance(&mut governor, &observation, 32_000, 60_000);
        assert!(plan.writes.is_empty());
        assert_eq!(plan.reason, "write_failed_waiting_for_stability");
        assert!(
            !governor
                .plan(62_000, true, &observation, healthy(), Ownership::default())
                .writes
                .is_empty()
        );
        governor.plan(
            64_000,
            true,
            &observation,
            PressureInput {
                cpu_busy: Some(true),
                ..healthy()
            },
            Ownership::default(),
        );
        assert!(
            advance(&mut governor, &observation, 66_000, 94_000)
                .writes
                .is_empty()
        );
        assert!(
            !governor
                .plan(96_000, true, &observation, healthy(), Ownership::default())
                .writes
                .is_empty()
        );
        observation.tasks.complete = false;
        governor.plan(98_000, true, &observation, healthy(), Ownership::default());
        observation.tasks.complete = true;
        assert!(
            advance(&mut governor, &observation, 100_000, 128_000)
                .writes
                .is_empty()
        );
    }

    #[test]
    fn clock_gaps_do_not_count_as_observed_healthy_time() {
        let mut governor = Governor::default();
        let observation = stopped();
        governor.plan(0, true, &observation, healthy(), Ownership::default());
        assert!(
            governor
                .plan(60_000, true, &observation, healthy(), Ownership::default())
                .writes
                .is_empty()
        );
        assert!(
            governor
                .plan(100, true, &observation, healthy(), Ownership::default())
                .writes
                .is_empty()
        );
    }

    #[test]
    fn cached_task_age_must_be_known_and_bounded() {
        let (mut observation, ownership) = owned_running();
        let mut governor = Governor::default();
        observation.task_age_ms = Some(MAX_TASK_AGE_MILLIS);
        assert_eq!(
            governor
                .plan(0, true, &observation, healthy(), ownership)
                .state,
            State::Scanning
        );
        for age in [None, Some(MAX_TASK_AGE_MILLIS + 1)] {
            observation.task_age_ms = age;
            assert_pause(
                &governor.plan(0, true, &observation, healthy(), ownership),
                State::PausedPressure,
            );
        }
    }

    #[test]
    fn external_run_advisor_or_write_change_is_observation_only() {
        let (mut observation, ownership) = owned_running();
        let mut governor = Governor::default();
        let plan = governor.plan(0, true, &observation, healthy(), Ownership::default());
        assert_eq!(plan.state, State::ExternallyManaged);
        assert!(plan.writes.is_empty());
        observation.ksm.advisor = Advisor::External("scan-time".into());
        assert!(
            governor
                .plan(0, true, &observation, PressureInput::default(), ownership)
                .writes
                .is_empty()
        );
        observation.ksm.advisor = Advisor::None;
        observation.ksm.pages_to_scan = Some(900);
        let plan = governor.plan(0, true, &observation, healthy(), ownership);
        assert_eq!(plan.reason, "external_control_change");
        assert!(plan.writes.is_empty());
        observation.ksm.pages_to_scan = Some(PAGES_TO_SCAN);
        governor.feedback(ApplyOutcome::Applied);
        assert_eq!(
            governor
                .plan(0, true, &observation, healthy(), ownership)
                .state,
            State::ExternallyManaged
        );
        let mut governor = Governor::default();
        governor.feedback(ApplyOutcome::ExternalChange);
        assert!(
            governor
                .plan(0, true, &observation, healthy(), ownership)
                .writes
                .is_empty()
        );
    }

    #[test]
    fn missing_advisor_is_supported_but_run_two_never_touched() {
        let mut observation = stopped();
        observation.ksm.advisor = Advisor::Missing;
        let mut governor = Governor::default();
        assert!(
            !advance(&mut governor, &observation, 0, 30_000)
                .writes
                .is_empty()
        );
        observation.ksm.run = Some(2);
        let plan = governor.plan(32_000, true, &observation, healthy(), Ownership::default());
        assert_eq!(plan.state, State::ExternallyManaged);
        assert!(plan.writes.is_empty());
    }

    #[test]
    fn initially_matching_unwritten_knobs_cannot_be_reacquired_after_external_change() {
        for running in [0, 1] {
            let (mut observation, _) = owned_running();
            observation.ksm.run = Some(running);
            observation.ksm.pages_to_scan = Some(PAGES_TO_SCAN + 1);
            let ownership = Ownership {
                run: Some(running),
                ..Ownership::default()
            };
            let plan = Governor::default().plan(0, true, &observation, healthy(), ownership);
            assert_eq!(plan.state, State::ExternallyManaged);
            assert!(plan.writes.is_empty());
            observation.ksm.pages_to_scan = Some(PAGES_TO_SCAN);
            observation.ksm.sleep_millisecs = Some(SLEEP_MILLISECS + 1);
            let plan = Governor::default().plan(0, true, &observation, healthy(), ownership);
            assert_eq!(plan.state, State::ExternallyManaged);
            assert!(plan.writes.is_empty());
        }
    }

    #[test]
    fn every_generated_run_write_is_stop_or_start_only() {
        for run in [None, Some(0), Some(1), Some(2)] {
            for enabled in [false, true] {
                for owner in [None, Some(0), Some(1), Some(2)] {
                    let mut observation = stopped();
                    observation.ksm.run = run;
                    let mut governor = Governor::default();
                    for now in (0..=32_000).step_by(2000) {
                        let plan = governor.plan(
                            now,
                            enabled,
                            &observation,
                            healthy(),
                            Ownership {
                                run: owner,
                                ..Ownership::default()
                            },
                        );
                        for write in plan.writes {
                            if write.knob == Knob::Run {
                                assert!(write.value <= 1);
                            }
                            assert!(write.knob.path().starts_with(KSM_ROOT));
                        }
                    }
                }
            }
        }
    }
}
