//! Bounded, per-user admission for commands explicitly launched through run.
//! No daemon state, process control, or IO-pressure inference is used here.
use crate::{
    json,
    linux_flags::{O_DIRECTORY, O_NOFOLLOW, O_NONBLOCK},
};
use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicI32, AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};
use zmem::Config;

pub const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 60;
pub const MAX_WAIT_TIMEOUT_SECS: u64 = 3600;
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const RECOVERY_MS: u64 = 3000;
const START_INTERVAL_MS: u64 = 3000;
const EVENT_INTERVAL: Duration = Duration::from_secs(5);
const STATE_LIMIT: u64 = 512;
static CANCELLED: AtomicI32 = AtomicI32::new(0);
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" {
    fn geteuid() -> u32;
    fn flock(fd: i32, operation: i32) -> i32;
    // Linux signal handlers, SIG_DFL and SIG_IGN all use one pointer-sized word.
    fn signal(number: i32, handler: usize) -> usize;
    fn clock_gettime(clock: i32, result: *mut Timespec) -> i32;
    fn sigemptyset(set: *mut SignalSet) -> i32;
    fn sigaddset(set: *mut SignalSet, signal: i32) -> i32;
    fn sigprocmask(how: i32, set: *const SignalSet, old: *mut SignalSet) -> i32;
}

#[repr(C)]
struct Timespec {
    seconds: std::ffi::c_long,
    nanos: std::ffi::c_long,
}
#[repr(C)]
struct SignalSet {
    words: [std::ffi::c_ulong; 128 / std::mem::size_of::<std::ffi::c_ulong>()],
}

#[derive(Debug)]
pub struct Failure {
    pub code: i32,
    event: &'static str,
    message: String,
}
impl Failure {
    pub(crate) fn state(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            event: "admission_error",
            message: message.into(),
        }
    }
    fn timeout(reason: &str) -> Self {
        Self {
            code: 75,
            event: "admission_timeout",
            message: format!("command was not started: {reason}"),
        }
    }
    fn cancelled(signal: i32) -> Self {
        Self {
            code: 128 + signal,
            event: "admission_cancelled",
            message: format!("command was not started; signal {signal}"),
        }
    }
    pub fn json(&self) -> String {
        format!(
            "{{\"event\":{},\"exit_code\":{},\"message\":{}}}",
            json::quote(self.event),
            self.code,
            json::quote(&self.message)
        )
    }
}
impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Failure {}
impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self::state(error.to_string())
    }
}
type Result<T> = std::result::Result<T, Failure>;

pub fn diagnostic(text: &str) {
    // Diagnostics must not turn closed stderr into a panic or change an
    // otherwise successful launch/temporary-unavailable exit status.
    let _ = writeln!(io::stderr().lock(), "{text}");
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Sample {
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub some_bp: u16,
    pub full_bp: u16,
}

fn limited_read(path: &Path, limit: u64) -> io::Result<String> {
    let mut text = String::new();
    File::open(path)?
        .take(limit + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "kernel observation exceeds read limit",
        ));
    }
    Ok(text)
}

fn memory(text: &str) -> io::Result<(u64, u64)> {
    let mut total = None;
    let mut available = None;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let target = match fields.next() {
            Some("MemTotal:") => &mut total,
            Some("MemAvailable:") => &mut available,
            _ => continue,
        };
        let bytes = fields
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .and_then(|value| value.checked_mul(1024));
        if target.is_some()
            || bytes.is_none()
            || fields.next() != Some("kB")
            || fields.next().is_some()
        {
            return invalid("invalid or duplicate memory observation");
        }
        *target = bytes;
    }
    match (total, available) {
        (Some(total), Some(available)) if total > 0 && available <= total => Ok((total, available)),
        _ => invalid("MemTotal/MemAvailable are unavailable or inconsistent"),
    }
}

fn basis_points(value: &str) -> io::Result<u16> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || !whole.bytes().all(|v| v.is_ascii_digit())
        || fraction.len() > 2
        || !fraction.bytes().all(|v| v.is_ascii_digit())
    {
        return invalid("memory PSI avg10 must be a finite percentage");
    }
    let whole: u16 = whole
        .parse()
        .map_err(|_| io::Error::other("invalid PSI percentage"))?;
    let fraction: u16 = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse::<u16>()
            .map_err(|_| io::Error::other("invalid PSI fraction"))?
            * if fraction.len() == 1 { 10 } else { 1 }
    };
    whole
        .checked_mul(100)
        .and_then(|v| v.checked_add(fraction))
        .filter(|value| *value <= 10000)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "memory PSI percentage is outside 0..100",
            )
        })
}

fn pressure(text: &str) -> io::Result<(u16, u16)> {
    let mut some = None;
    let mut full = None;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let target = match fields.next() {
            Some("some") => &mut some,
            Some("full") => &mut full,
            _ => continue,
        };
        if target.is_some() {
            return invalid("duplicate memory PSI row");
        }
        let mut average = None;
        for field in fields {
            if let Some(value) = field.strip_prefix("avg10=") {
                if average.is_some() {
                    return invalid("duplicate memory PSI avg10");
                }
                average = Some(basis_points(value)?);
            }
        }
        *target = Some(average.ok_or_else(|| io::Error::other("missing memory PSI avg10"))?);
    }
    match (some, full) {
        (Some(some), Some(full)) if full <= some => Ok((some, full)),
        _ => invalid("memory PSI some/full are unavailable or inconsistent"),
    }
}

fn invalid<T>(message: &str) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidData, message))
}

fn sample(proc_root: &Path) -> io::Result<Sample> {
    let (total_bytes, available_bytes) =
        memory(&limited_read(&proc_root.join("meminfo"), 64 * 1024)?)?;
    let (some_bp, full_bp) = pressure(&limited_read(&proc_root.join("pressure/memory"), 4096)?)?;
    Ok(Sample {
        total_bytes,
        available_bytes,
        some_bp,
        full_bp,
    })
}

#[derive(Clone, Copy)]
struct Limits {
    reserve: u64,
    recovered_reserve: u64,
    some: u16,
    full: u16,
}
impl Limits {
    fn new(config: Config, total: u64) -> Self {
        let reserve = config.effective_reserve_bytes(total);
        // The policy's 1 GiB automatic floor is not a viable launch gate on
        // a <=1 GiB machine. Explicit user reserves retain their full meaning.
        let reserve = if config.reserve_bytes == 0 {
            reserve.min(total / 4)
        } else {
            reserve
        };
        Self {
            reserve,
            recovered_reserve: reserve.saturating_add(reserve / 4).min(total),
            some: config.pressure_some_bp.clamp(1, 500),
            full: config.critical_full_bp.clamp(1, 100),
        }
    }
    fn initial(self, sample: Sample) -> bool {
        sample.available_bytes >= self.reserve
            && sample.some_bp < self.some
            && sample.full_bp < self.full
    }
    fn recovered(self, sample: Sample) -> bool {
        sample.available_bytes >= self.recovered_reserve
            && u32::from(sample.some_bp) * 2 < u32::from(self.some)
            && u32::from(sample.full_bp) * 2 < u32::from(self.full)
    }
}

struct Gate {
    config: Config,
    recovering: bool,
    good_since: Option<u64>,
    last_time: Option<u64>,
    total: Option<u64>,
}
impl Gate {
    fn new(config: Config) -> Self {
        Self {
            config,
            recovering: false,
            good_since: None,
            last_time: None,
            total: None,
        }
    }
    fn unknown(&mut self) {
        self.recovering = true;
        self.good_since = None;
    }
    fn ready(&mut self, sample: Sample, now_ms: u64) -> bool {
        if self.last_time.is_some_and(|last| now_ms < last)
            || self
                .last_time
                .is_some_and(|last| now_ms.saturating_sub(last) > 2000)
            || self.total.is_some_and(|total| total != sample.total_bytes)
        {
            self.unknown();
        }
        self.last_time = Some(now_ms);
        self.total = Some(sample.total_bytes);
        let limits = Limits::new(self.config, sample.total_bytes);
        if !self.recovering && limits.initial(sample) {
            return true;
        }
        self.recovering = true;
        if !limits.recovered(sample) {
            self.good_since = None;
            return false;
        }
        now_ms.saturating_sub(*self.good_since.get_or_insert(now_ms)) >= RECOVERY_MS
    }
}

extern "C" fn cancel(signal: i32) {
    CANCELLED.store(signal, Ordering::Relaxed);
}
struct Signals {
    previous: Option<(usize, usize)>,
}
struct BlockSignals(SignalSet);
impl BlockSignals {
    fn new() -> io::Result<Self> {
        let mut set = SignalSet {
            words: [0; 128 / std::mem::size_of::<std::ffi::c_ulong>()],
        };
        let mut old = SignalSet {
            words: [0; 128 / std::mem::size_of::<std::ffi::c_ulong>()],
        };
        if unsafe { sigemptyset(&mut set) } != 0
            || unsafe { sigaddset(&mut set, 2) } != 0
            || unsafe { sigaddset(&mut set, 15) } != 0
            || unsafe { sigprocmask(0, &set, &mut old) } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(old))
    }
}
impl Drop for BlockSignals {
    fn drop(&mut self) {
        unsafe {
            sigprocmask(2, &self.0, std::ptr::null_mut());
        }
    }
}
impl Signals {
    fn install() -> Result<Self> {
        let _blocked = BlockSignals::new()?;
        CANCELLED.store(0, Ordering::Relaxed);
        // These calls only install a signal-safe atomic-store handler. The
        // original handlers (including SIG_IGN) are restored before exec.
        let int = unsafe { signal(2, cancel as *const () as usize) };
        if int == usize::MAX {
            return Err(io::Error::last_os_error().into());
        }
        let term = unsafe { signal(15, cancel as *const () as usize) };
        if term == usize::MAX {
            unsafe {
                signal(2, int);
            }
            return Err(io::Error::last_os_error().into());
        }
        // Shell background jobs may deliberately inherit SIGINT ignored.
        // Retain that behavior during the wait as well as after exec.
        if int == 1 {
            unsafe {
                signal(2, int);
            }
        }
        if term == 1 {
            unsafe {
                signal(15, term);
            }
        }
        Ok(Self {
            previous: Some((int, term)),
        })
    }
    fn restore(&mut self) {
        if let Some((int, term)) = self.previous.take() {
            unsafe {
                signal(2, int);
                signal(15, term);
            }
        }
    }
    fn check() -> Result<()> {
        match CANCELLED.load(Ordering::Relaxed) {
            0 => Ok(()),
            signal => Err(Failure::cancelled(signal)),
        }
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        self.restore();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Identity {
    boot: String,
    time_dev: u64,
    time_ino: u64,
    uid: u32,
    pid: u32,
    start_ticks: u64,
}
fn boot_ms() -> io::Result<u64> {
    let mut value = Timespec {
        seconds: 0,
        nanos: 0,
    };
    // CLOCK_BOOTTIME includes suspend and has the same basis for all callers
    // in the recorded time namespace. Wall-clock adjustments do not affect it.
    if unsafe { clock_gettime(7, &mut value) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if value.seconds < 0 || !(0..1_000_000_000).contains(&value.nanos) {
        return invalid("invalid CLOCK_BOOTTIME observation");
    }
    (value.seconds as u64)
        .checked_mul(1000)
        .and_then(|n| n.checked_add(value.nanos as u64 / 1_000_000))
        .ok_or_else(|| io::Error::other("CLOCK_BOOTTIME overflow"))
}
fn valid_boot_id(boot: &str) -> bool {
    boot.len() == 36
        && boot.bytes().enumerate().all(|(index, value)| {
            if [8, 13, 18, 23].contains(&index) {
                value == b'-'
            } else {
                value.is_ascii_hexdigit()
            }
        })
}
fn process_start(text: &str, pid: u32) -> io::Result<u64> {
    if text.split_once(' ').and_then(|v| v.0.parse::<u32>().ok()) != Some(pid) {
        return invalid("process identity changed");
    }
    text.rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|ticks| *ticks != 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing process start time"))
}
impl Identity {
    fn current(uid: u32) -> io::Result<Self> {
        let boot = limited_read(Path::new("/proc/sys/kernel/random/boot_id"), 128)?
            .trim()
            .to_owned();
        if !valid_boot_id(&boot) {
            return invalid("invalid boot identity");
        }
        let time = match fs::metadata("/proc/self/ns/time") {
            Ok(metadata) => (metadata.dev(), metadata.ino()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // Kernels before 5.6 have no time namespaces. A missing proc
                // namespace directory is not evidence of that older ABI.
                if !fs::metadata("/proc/self/ns")?.is_dir() {
                    return invalid("missing proc namespaces");
                }
                (0, 0)
            }
            Err(error) => return Err(error),
        };
        let pid = std::process::id();
        let start_ticks = process_start(&limited_read(Path::new("/proc/self/stat"), 4096)?, pid)?;
        Ok(Self {
            boot,
            time_dev: time.0,
            time_ino: time.1,
            uid,
            pid,
            start_ticks,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Reservation {
    owner: Identity,
    at_ms: u64,
}
impl Reservation {
    fn encode(&self) -> String {
        format!(
            "zmem-admission-v1\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            self.owner.boot,
            self.owner.time_dev,
            self.owner.time_ino,
            self.owner.uid,
            self.owner.pid,
            self.owner.start_ticks,
            self.at_ms
        )
    }
    fn decode(text: &str) -> io::Result<Self> {
        if !text.ends_with('\n') || text[..text.len().saturating_sub(1)].contains('\n') {
            return invalid("incomplete admission state");
        }
        let fields: Vec<_> = text.trim_end_matches('\n').split('\t').collect();
        if fields.len() != 8 || fields[0] != "zmem-admission-v1" || !valid_boot_id(fields[1]) {
            return invalid("invalid admission state format");
        }
        let number = |index: usize| -> io::Result<u64> {
            if fields[index].is_empty() || !fields[index].bytes().all(|v| v.is_ascii_digit()) {
                return invalid("invalid admission state integer");
            }
            fields[index]
                .parse()
                .map_err(|_| io::Error::other("admission state integer overflow"))
        };
        let uid =
            u32::try_from(number(4)?).map_err(|_| io::Error::other("invalid admission uid"))?;
        let pid =
            u32::try_from(number(5)?).map_err(|_| io::Error::other("invalid admission pid"))?;
        let start_ticks = number(6)?;
        if pid == 0 || pid > i32::MAX as u32 || start_ticks == 0 {
            return invalid("invalid admission process identity");
        }
        Ok(Self {
            owner: Identity {
                boot: fields[1].into(),
                time_dev: number(2)?,
                time_ino: number(3)?,
                uid,
                pid,
                start_ticks,
            },
            at_ms: number(7)?,
        })
    }
    fn remaining_ms(&self, current: &Identity, now_ms: u64) -> io::Result<u64> {
        if self.owner.uid != current.uid {
            return invalid("admission state belongs to a different uid");
        }
        if self.owner.boot != current.boot {
            return Ok(0);
        }
        if (self.owner.time_dev, self.owner.time_ino) != (current.time_dev, current.time_ino) {
            return invalid("admission state uses a different time namespace");
        }
        let elapsed = now_ms
            .checked_sub(self.at_ms)
            .ok_or_else(|| io::Error::other("admission reservation is in the future"))?;
        // PID exit or reuse is not permission to bypass the launch interval.
        Ok(START_INTERVAL_MS.saturating_sub(elapsed))
    }
}

fn secure_directory(path: &Path, uid: u32) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECTORY | O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return invalid(
            "admission directory must be owned by this uid and inaccessible to other users",
        );
    }
    Ok(file)
}
fn secure_file(file: &File, uid: u32) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
    {
        return invalid("admission file must be an owned mode-0600 regular file with one link");
    }
    Ok(())
}
fn same_inode(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

struct Coordinator {
    path: PathBuf,
    directory: File,
    lock: File,
    identity: Identity,
}
struct Locked<'a> {
    coordinator: &'a Coordinator,
}
impl Drop for Locked<'_> {
    fn drop(&mut self) {
        unsafe {
            flock(self.coordinator.lock.as_raw_fd(), 8);
        }
    }
}
impl Coordinator {
    fn open() -> Result<Self> {
        let uid = unsafe { geteuid() };
        let parent = if uid == 0 {
            PathBuf::from("/run")
        } else {
            PathBuf::from(format!("/run/user/{uid}"))
        };
        let parent_meta = fs::symlink_metadata(&parent)?;
        let forbidden = if uid == 0 { 0o022 } else { 0o077 };
        if !parent_meta.is_dir()
            || parent_meta.uid() != uid
            || parent_meta.mode() & forbidden != 0
            || fs::canonicalize(&parent)? != parent
        {
            return Err(Failure::state(
                "no safe per-user runtime directory for admission",
            ));
        }
        let path = parent.join("zmem-admission");
        Self::at(path, Identity::current(uid)?)
    }
    fn at(path: PathBuf, identity: Identity) -> Result<Self> {
        match fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let directory = secure_directory(&path, identity.uid)?;
        let anchor = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK)
            .open(anchor.join("lock"))?;
        secure_file(&lock, identity.uid)?;
        let coordinator = Self {
            path,
            directory,
            lock,
            identity,
        };
        coordinator.validate()?;
        Ok(coordinator)
    }
    fn anchor(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.directory.as_raw_fd()))
    }
    fn validate(&self) -> io::Result<()> {
        let directory = fs::symlink_metadata(&self.path)?;
        if !directory.is_dir()
            || directory.uid() != self.identity.uid
            || directory.mode() & 0o077 != 0
            || !same_inode(&directory, &self.directory.metadata()?)
        {
            return invalid("admission directory identity changed");
        }
        secure_file(&self.lock, self.identity.uid)?;
        let path = fs::symlink_metadata(self.anchor().join("lock"))?;
        if !path.is_file() || !same_inode(&path, &self.lock.metadata()?) {
            return invalid("admission lock identity changed");
        }
        Ok(())
    }
    fn try_lock(&self) -> Result<Option<Locked<'_>>> {
        if unsafe { flock(self.lock.as_raw_fd(), 2 | 4) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error.into());
        }
        let lock = Locked { coordinator: self };
        self.validate()?;
        Ok(Some(lock))
    }
}
impl Locked<'_> {
    fn remaining_ms(&self, now_ms: u64) -> Result<u64> {
        let path = self.coordinator.anchor().join("reservation");
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error.into()),
        };
        secure_file(&file, self.coordinator.identity.uid)?;
        let mut text = String::new();
        file.take(STATE_LIMIT + 1).read_to_string(&mut text)?;
        if text.len() as u64 > STATE_LIMIT {
            return Err(Failure::state("admission state exceeds read limit"));
        }
        Ok(Reservation::decode(&text)?.remaining_ms(&self.coordinator.identity, now_ms)?)
    }
    fn reserve(&self, at_ms: u64) -> Result<()> {
        self.coordinator.validate()?;
        let anchor = self.coordinator.anchor();
        let temporary = anchor.join(format!(
            ".reservation-{}-{}-{}",
            self.coordinator.identity.pid,
            self.coordinator.identity.start_ticks,
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(O_NOFOLLOW)
                .open(&temporary)?;
            let record = Reservation {
                owner: self.coordinator.identity.clone(),
                at_ms,
            };
            file.write_all(record.encode().as_bytes())?;
            file.sync_all()?;
            self.coordinator.validate()?;
            fs::rename(&temporary, anchor.join("reservation"))?;
            self.coordinator.directory.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }
}

pub struct Permit {
    pub sample: Sample,
    coordinator: Coordinator,
    signals: Signals,
    started: Instant,
    timeout: Duration,
}
impl Permit {
    pub fn before_exec(&mut self) -> Result<()> {
        Signals::check()?;
        if !self.timeout.is_zero() && self.started.elapsed() >= self.timeout {
            return Err(Failure::timeout("deadline reached before exec"));
        }
        // Refresh the short reservation immediately before handoff, still
        // owning the same lock. It is not a prediction of COMMAND's peak RAM
        // or an assertion about scheduler-controlled actual start times.
        let lock = Locked {
            coordinator: &self.coordinator,
        };
        lock.reserve(boot_ms()?)?;
        std::mem::forget(lock);
        // Restore first, then check the captured cancellation flag: no signal
        // observed by our handler may silently turn into a command launch.
        self.signals.restore();
        Signals::check()
    }
}

pub fn acquire(
    config: Config,
    timeout_secs: u64,
    validate: impl Fn(Sample) -> std::result::Result<(), String>,
) -> Result<Permit> {
    if timeout_secs > MAX_WAIT_TIMEOUT_SECS {
        return Err(Failure::state("wait timeout must be 0..3600 seconds"));
    }
    let signals = Signals::install()?;
    let coordinator = Coordinator::open()?;
    wait_for_permit(
        config,
        timeout_secs,
        validate,
        coordinator,
        signals,
        Path::new("/proc"),
    )
}

fn wait_for_permit(
    config: Config,
    timeout_secs: u64,
    validate: impl Fn(Sample) -> std::result::Result<(), String>,
    coordinator: Coordinator,
    signals: Signals,
    proc_root: &Path,
) -> Result<Permit> {
    let start = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);
    let mut gate = Gate::new(config);
    let mut last_event: Option<Instant> = None;
    let mut checked_once = false;
    let mut last_reason = "memory_pressure".to_owned();
    loop {
        Signals::check()?;
        let elapsed = start.elapsed();
        if checked_once && elapsed >= timeout {
            return Err(Failure::timeout(&last_reason));
        }
        checked_once = true;
        let elapsed_ms = elapsed.as_millis().min(u64::MAX as u128) as u64;
        let mut reason = "memory_pressure".to_owned();
        let mut granted = None;
        match sample(proc_root) {
            Err(error) => {
                gate.unknown();
                reason = format!("kernel_observation_unavailable: {error}");
            }
            Ok(observed) => {
                validate(observed).map_err(Failure::state)?;
                if gate.ready(observed, elapsed_ms) {
                    match coordinator.try_lock()? {
                        None => reason = "another_admission_check".into(),
                        Some(lock) => {
                            // The first observation may have aged while a
                            // competitor reserved its launch. Recheck under
                            // the same short lock that commits this request.
                            match sample(proc_root) {
                                Err(error) => {
                                    gate.unknown();
                                    reason = format!("kernel_observation_unavailable: {error}");
                                }
                                Ok(latest) => {
                                    validate(latest).map_err(Failure::state)?;
                                    let now = boot_ms()?;
                                    if gate.ready(
                                        latest,
                                        start.elapsed().as_millis().min(u64::MAX as u128) as u64,
                                    ) {
                                        if lock.remaining_ms(now)? == 0 {
                                            Signals::check()?;
                                            if timeout_secs != 0 && start.elapsed() >= timeout {
                                                return Err(Failure::timeout(
                                                    "deadline reached before reservation",
                                                ));
                                            }
                                            lock.reserve(now)?;
                                            // Keep the short exclusive lock through exec. Every
                                            // owned descriptor is CLOEXEC; never lock a workload's lifetime.
                                            std::mem::forget(lock);
                                            granted = Some(latest);
                                        }
                                        reason = "launch_interval".into();
                                    } else {
                                        reason = "memory_recovery".into();
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(latest) = granted {
            diagnostic(&format!(
                "{{\"event\":\"admission_granted\",\"waited_ms\":{elapsed_ms},\"available_bytes\":{},\"memory_some_bp\":{},\"memory_full_bp\":{}}}",
                latest.available_bytes, latest.some_bp, latest.full_bp
            ));
            return Ok(Permit {
                sample: latest,
                coordinator,
                signals,
                started: start,
                timeout,
            });
        }
        if start.elapsed() >= timeout {
            return Err(Failure::timeout(&reason));
        }
        last_reason.clone_from(&reason);
        if last_event.is_none_or(|last| last.elapsed() >= EVENT_INTERVAL) {
            diagnostic(&format!(
                "{{\"event\":\"admission_wait\",\"waited_ms\":{elapsed_ms},\"timeout_seconds\":{timeout_secs},\"reason\":{}}}",
                json::quote(&reason)
            ));
            last_event = Some(Instant::now());
        }
        let next = Instant::now() + SAMPLE_INTERVAL.min(timeout.saturating_sub(start.elapsed()));
        while Instant::now() < next {
            Signals::check()?;
            thread::sleep(POLL_INTERVAL.min(next.saturating_duration_since(Instant::now())));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        ffi::CString,
        os::unix::{
            ffi::OsStrExt,
            fs::{PermissionsExt, symlink},
        },
        process::{Child, Command, Stdio},
        sync::{Arc, Barrier, atomic::AtomicUsize},
    };
    use zmem::{GIB, MIB};

    unsafe extern "C" {
        fn mkfifo(path: *const std::ffi::c_char, mode: u32) -> i32;
        fn kill(pid: i32, signal: i32) -> i32;
        fn close(fd: i32) -> i32;
        fn fcntl(fd: i32, command: i32, ...) -> i32;
    }

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "zmem-admission-test-{}-{}",
                std::process::id(),
                TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
        fn coordinator(&self) -> Coordinator {
            Coordinator::at(
                self.0.join("runtime"),
                Identity::current(unsafe { geteuid() }).unwrap(),
            )
            .unwrap()
        }
        fn proc(&self, available: u64) -> PathBuf {
            let root = self.0.join("proc");
            fs::create_dir_all(root.join("pressure")).unwrap();
            fs::write(
                root.join("meminfo"),
                format!(
                    "MemTotal: {} kB\nMemAvailable: {} kB\n",
                    16 * GIB / 1024,
                    available / 1024
                ),
            )
            .unwrap();
            fs::write(root.join("pressure/memory"), "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n").unwrap();
            root
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn healthy() -> Sample {
        Sample {
            total_bytes: 16 * GIB,
            available_bytes: 8 * GIB,
            some_bp: 0,
            full_bp: 0,
        }
    }
    fn unhandled() -> Signals {
        Signals { previous: None }
    }

    #[test]
    fn strict_kernel_observations_never_invent_zero_pressure_or_available_memory() {
        assert_eq!(
            memory("MemTotal: 1024 kB\nMemAvailable: 512 kB\n").unwrap(),
            (MIB, MIB / 2)
        );
        for text in [
            "MemTotal: 1024 kB\n",
            "MemTotal: 0 kB\nMemAvailable: 0 kB\n",
            "MemTotal: 1 kB\nMemAvailable: 2 kB\n",
            "MemTotal: 2 MB\nMemAvailable: 1 kB\n",
            "MemTotal: 2 kB\nMemTotal: 2 kB\nMemAvailable: 1 kB\n",
        ] {
            assert!(memory(text).is_err());
        }
        assert_eq!(
            pressure("some avg10=5.25\nfull avg10=1.00\n").unwrap(),
            (525, 100)
        );
        for text in [
            "some avg10=0.00\n",
            "some avg10=NaN\nfull avg10=0.00\n",
            "some avg10=0.00\nfull avg10=inf\n",
            "some avg60=0.00\nfull avg10=0.00\n",
            "some avg10=0.00 avg10=1.00\nfull avg10=0.00\n",
            "some avg10=0.00\nfull avg10=1.00\n",
            "some avg10=101.00\nfull avg10=0.00\n",
        ] {
            assert!(pressure(text).is_err(), "{text}");
        }
    }

    #[test]
    fn initial_healthy_check_is_immediate_but_recovery_resets_on_bad_or_missing_reads() {
        let mut gate = Gate::new(Config::default());
        assert!(gate.ready(healthy(), 0));
        assert!(!gate.ready(
            Sample {
                available_bytes: 0,
                ..healthy()
            },
            1
        ));
        for at in [1000, 2000, 3000] {
            assert!(!gate.ready(healthy(), at));
        }
        gate.unknown();
        for at in [4000, 5000, 6000] {
            assert!(!gate.ready(healthy(), at));
        }
        assert!(gate.ready(healthy(), 7000));
        assert!(!gate.ready(
            Sample {
                full_bp: 100,
                some_bp: 100,
                ..healthy()
            },
            7001
        ));
        assert!(!gate.ready(healthy(), 8000));
        // A long unobserved interval cannot stand in for stable recovery.
        assert!(!gate.ready(healthy(), 20_000));
        assert!(!gate.ready(healthy(), 100));
    }

    #[test]
    fn small_ram_and_one_basis_point_configuration_can_recover() {
        let total = 512 * MIB;
        let sample = Sample {
            total_bytes: total,
            available_bytes: total / 2,
            ..healthy()
        };
        let limits = Limits::new(Config::default(), total);
        assert_eq!(limits.reserve, total / 4);
        assert!(limits.initial(sample));
        assert_eq!(
            Limits::new(
                Config {
                    reserve_bytes: 400 * MIB,
                    ..Config::default()
                },
                total
            )
            .reserve,
            400 * MIB
        );
        let mut gate = Gate::new(Config {
            pressure_some_bp: 1,
            critical_full_bp: 1,
            ..Config::default()
        });
        gate.unknown();
        for at in [0, 1000, 2000] {
            assert!(!gate.ready(sample, at));
        }
        assert!(gate.ready(sample, 3000));
    }

    #[test]
    fn pressure_thresholds_and_recovery_headroom_are_separate_from_io() {
        let limits = Limits::new(Config::default(), 16 * GIB);
        assert!(limits.initial(Sample {
            available_bytes: limits.reserve,
            some_bp: 499,
            full_bp: 99,
            ..healthy()
        }));
        for sample in [
            Sample {
                available_bytes: limits.reserve - 1,
                ..healthy()
            },
            Sample {
                some_bp: 500,
                ..healthy()
            },
            Sample {
                some_bp: 100,
                full_bp: 100,
                ..healthy()
            },
        ] {
            assert!(!limits.initial(sample));
        }
        assert!(!limits.recovered(Sample {
            available_bytes: limits.recovered_reserve - 1,
            ..healthy()
        }));
        assert!(!limits.recovered(Sample {
            some_bp: 250,
            ..healthy()
        }));
        assert!(!limits.recovered(Sample {
            some_bp: 50,
            full_bp: 50,
            ..healthy()
        }));
        let fixture = Fixture::new();
        // There is deliberately no pressure/io node in this fixture.
        let root = fixture.proc(8 * GIB);
        assert!(sample(&root).is_ok());
        // Global IO pressure can include a deliberately throttled scanner.
        // It must not prevent initial admission when memory is healthy.
        fs::write(
            root.join("pressure/io"),
            "some avg10=75.00\nfull avg10=65.00\n",
        )
        .unwrap();
        assert!(Gate::new(Config::default()).ready(sample(&root).unwrap(), 0));
        // The same busy disk does not weaken the existing memory gate.
        fs::write(
            root.join("pressure/memory"),
            "some avg10=5.00\nfull avg10=2.00\n",
        )
        .unwrap();
        assert!(!Gate::new(Config::default()).ready(sample(&root).unwrap(), 0));
    }

    #[test]
    fn reservation_checks_boot_clock_uid_and_pid_start_identity_without_liveness_shortcuts() {
        let identity = Identity::current(unsafe { geteuid() }).unwrap();
        let reservation = Reservation {
            owner: identity.clone(),
            at_ms: 1000,
        };
        assert_eq!(
            Reservation::decode(&reservation.encode()).unwrap(),
            reservation
        );
        assert_eq!(reservation.remaining_ms(&identity, 1001).unwrap(), 2999);
        assert_eq!(reservation.remaining_ms(&identity, 4000).unwrap(), 0);
        assert!(reservation.remaining_ms(&identity, 999).is_err());
        let mut reused_pid = identity.clone();
        reused_pid.start_ticks += 1;
        assert_eq!(reservation.remaining_ms(&reused_pid, 1001).unwrap(), 2999);
        let mut different_time = identity.clone();
        different_time.time_ino += 1;
        assert!(reservation.remaining_ms(&different_time, 1001).is_err());
        let mut different_uid = identity.clone();
        different_uid.uid = identity.uid.wrapping_add(1);
        assert!(reservation.remaining_ms(&different_uid, 1001).is_err());
        let mut reboot = identity.clone();
        reboot.boot = "00000000-0000-0000-0000-000000000000".into();
        if reboot.boot == identity.boot {
            reboot.boot.replace_range(..1, "1");
        }
        assert_eq!(reservation.remaining_ms(&reboot, 0).unwrap(), 0);
        for text in ["", "zmem-admission-v1\n", "zmem-admission-v1\tNaN\n"] {
            assert!(Reservation::decode(text).is_err());
        }
        let stat = format!("42 (name ) with parentheses) S {} 987", ["0"; 18].join(" "));
        assert_eq!(process_start(&stat, 42).unwrap(), 987);
        assert!(process_start(&stat, 43).is_err());
    }

    #[test]
    fn simultaneous_contenders_have_one_shared_reservation_and_a_stable_lock_inode() {
        let fixture = Fixture::new();
        let identity = Identity::current(unsafe { geteuid() }).unwrap();
        let original = fixture.coordinator();
        let lock_inode = original.lock.metadata().unwrap().ino();
        drop(original);
        let barrier = Arc::new(Barrier::new(9));
        let admitted = Arc::new(AtomicUsize::new(0));
        let mut threads = Vec::new();
        for _ in 0..8 {
            let (path, identity, barrier, admitted) = (
                fixture.0.join("runtime"),
                identity.clone(),
                barrier.clone(),
                admitted.clone(),
            );
            threads.push(thread::spawn(move || {
                let coordinator = Coordinator::at(path, identity).unwrap();
                barrier.wait();
                if let Some(lock) = coordinator.try_lock().unwrap()
                    && lock.remaining_ms(1000).unwrap() == 0
                {
                    lock.reserve(1000).unwrap();
                    admitted.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(admitted.load(Ordering::Relaxed), 1);
        let coordinator = fixture.coordinator();
        assert_eq!(coordinator.lock.metadata().unwrap().ino(), lock_inode);
        let lock = coordinator.try_lock().unwrap().unwrap();
        assert_eq!(lock.remaining_ms(3999).unwrap(), 1);
        assert_eq!(lock.remaining_ms(4000).unwrap(), 0);
        lock.reserve(4000).unwrap();
        assert_eq!(coordinator.lock.metadata().unwrap().ino(), lock_inode);
        assert_ne!(
            unsafe { fcntl(coordinator.lock.as_raw_fd(), 1) } & 1,
            0,
            "lock must close on exec"
        );
    }

    #[test]
    fn runtime_files_reject_symlinks_wrong_ownership_links_modes_and_replaced_lock() {
        let fixture = Fixture::new();
        let identity = Identity::current(unsafe { geteuid() }).unwrap();
        let victim = fixture.0.join("victim");
        fs::write(&victim, "unchanged").unwrap();
        symlink(&fixture.0, fixture.0.join("linked-directory")).unwrap();
        assert!(Coordinator::at(fixture.0.join("linked-directory"), identity.clone()).is_err());
        let coordinator = fixture.coordinator();
        let lock_path = coordinator.path.join("lock");
        fs::hard_link(&lock_path, fixture.0.join("extra-link")).unwrap();
        assert!(coordinator.try_lock().is_err());
        fs::remove_file(fixture.0.join("extra-link")).unwrap();
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(coordinator.try_lock().is_err());
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::rename(&lock_path, fixture.0.join("old-lock")).unwrap();
        symlink(&victim, &lock_path).unwrap();
        assert!(coordinator.try_lock().is_err());
        assert_eq!(fs::read_to_string(&victim).unwrap(), "unchanged");
        let mut wrong_owner = identity;
        wrong_owner.uid = wrong_owner.uid.wrapping_add(1);
        assert!(Coordinator::at(fixture.0.join("other"), wrong_owner).is_err());
    }

    #[test]
    fn reservation_state_is_bounded_and_never_follows_or_overwrites_a_symlink() {
        let fixture = Fixture::new();
        let coordinator = fixture.coordinator();
        let lock = coordinator.try_lock().unwrap().unwrap();
        let state = coordinator.path.join("reservation");
        let victim = fixture.0.join("victim");
        fs::write(&victim, "unchanged").unwrap();
        symlink(&victim, &state).unwrap();
        assert!(lock.remaining_ms(1000).is_err());
        assert_eq!(fs::read_to_string(&victim).unwrap(), "unchanged");
        fs::remove_file(&state).unwrap();
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&state)
            .unwrap();
        file.write_all(&vec![b'x'; STATE_LIMIT as usize + 1])
            .unwrap();
        assert!(
            lock.remaining_ms(1000)
                .unwrap_err()
                .to_string()
                .contains("exceeds read limit")
        );
        drop(file);
        fs::write(&state, "incomplete").unwrap();
        assert!(lock.remaining_ms(1000).is_err());
        fs::remove_file(&state).unwrap();
        lock.reserve(1000).unwrap();
        fs::hard_link(&state, fixture.0.join("state-link")).unwrap();
        assert!(lock.remaining_ms(1000).is_err());
    }

    #[test]
    fn missing_reads_timeout_without_launch_then_retry_uses_fresh_kernel_data() {
        let fixture = Fixture::new();
        let root = fixture.proc(8 * GIB);
        fs::remove_file(root.join("pressure/memory")).unwrap();
        let outcome = wait_for_permit(
            Config::default(),
            0,
            |_| Ok(()),
            fixture.coordinator(),
            unhandled(),
            &root,
        );
        assert_eq!(outcome.err().unwrap().code, 75);
        assert!(!fixture.0.join("runtime/reservation").exists());
        fixture.proc(8 * GIB);
        let permit = wait_for_permit(
            Config::default(),
            0,
            |_| Ok(()),
            fixture.coordinator(),
            unhandled(),
            &root,
        )
        .unwrap();
        assert_eq!(permit.sample.available_bytes, 8 * GIB);
        drop(permit);
        let second = wait_for_permit(
            Config::default(),
            0,
            |_| Ok(()),
            fixture.coordinator(),
            unhandled(),
            &root,
        );
        assert_eq!(second.err().unwrap().code, 75);
    }

    #[test]
    fn positive_deadline_is_rechecked_under_lock_and_before_exec() {
        let fixture = Fixture::new();
        let root = fixture.proc(8 * GIB);
        let visits = AtomicUsize::new(0);
        let outcome = wait_for_permit(
            Config::default(),
            1,
            |_| {
                if visits.fetch_add(1, Ordering::Relaxed) == 0 {
                    thread::sleep(Duration::from_millis(1050));
                }
                Ok(())
            },
            fixture.coordinator(),
            unhandled(),
            &root,
        );
        assert_eq!(outcome.err().unwrap().code, 75);
        assert!(!fixture.0.join("runtime/reservation").exists());
        let mut permit = wait_for_permit(
            Config::default(),
            1,
            |_| Ok(()),
            fixture.coordinator(),
            unhandled(),
            &root,
        )
        .unwrap();
        permit.started = Instant::now() - Duration::from_secs(2);
        assert_eq!(permit.before_exec().unwrap_err().code, 75);
    }

    fn child(fixture: &Fixture, role: &str) -> Child {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "admission::tests::child_case", "--nocapture"])
            .env("ZMEM_ADMISSION_TEST_CHILD", role)
            .env("ZMEM_ADMISSION_TEST_ROOT", &fixture.0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }
    fn finish(mut child: Child, timeout: Duration) -> std::process::Output {
        let start = Instant::now();
        loop {
            if child.try_wait().unwrap().is_some() {
                return child.wait_with_output().unwrap();
            }
            if start.elapsed() > timeout {
                let _ = child.kill();
                let _ = child.wait();
                panic!("admission child exceeded its external test deadline");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn fifo_state_and_closed_stderr_cannot_hang_or_panic_the_launcher() {
        for role in ["fifo", "closed-stderr"] {
            let fixture = Fixture::new();
            fixture.proc(8 * GIB);
            let output = finish(child(&fixture, role), Duration::from_secs(3));
            assert!(
                output.status.success(),
                "{role}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            if role == "closed-stderr" {
                assert!(fixture.0.join("launched").exists());
            }
        }
    }

    #[test]
    fn wait_is_cancellable_preserves_inherited_ignore_and_never_starts_command() {
        for (role, signal, code) in [("int", 2, 130), ("term", 15, 143), ("ignored-int", 2, 75)] {
            let fixture = Fixture::new();
            fixture.proc(0);
            let child = child(&fixture, role);
            let start = Instant::now();
            while !fixture.0.join("ready").exists() {
                assert!(start.elapsed() < Duration::from_secs(2));
                thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(unsafe { kill(child.id() as i32, signal) }, 0);
            let output = finish(child, Duration::from_secs(3));
            assert_eq!(
                output.status.code(),
                Some(code),
                "{role}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!fixture.0.join("launched").exists());
            assert!(
                output
                    .stderr
                    .split(|byte| *byte == b'\n')
                    .filter(|line| !line.is_empty())
                    .all(|line| line.starts_with(b"{"))
            );
        }
    }

    #[test]
    fn child_case() {
        let Ok(role) = std::env::var("ZMEM_ADMISSION_TEST_CHILD") else {
            return;
        };
        let root = PathBuf::from(std::env::var_os("ZMEM_ADMISSION_TEST_ROOT").unwrap());
        let coordinator = Coordinator::at(
            root.join("runtime"),
            Identity::current(unsafe { geteuid() }).unwrap(),
        )
        .unwrap();
        if role == "fifo" {
            let path =
                CString::new(coordinator.path.join("reservation").as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { mkfifo(path.as_ptr(), 0o600) }, 0);
            assert!(
                coordinator
                    .try_lock()
                    .unwrap()
                    .unwrap()
                    .remaining_ms(boot_ms().unwrap())
                    .is_err()
            );
            return;
        }
        if role == "ignored-int" {
            unsafe {
                signal(2, 1);
            }
        }
        if role == "closed-stderr" {
            unsafe {
                close(2);
            }
        }
        let signals = Signals::install().unwrap();
        fs::write(root.join("ready"), "ready").unwrap();
        let result = wait_for_permit(
            Config::default(),
            if role == "ignored-int" { 1 } else { 30 },
            |_| Ok(()),
            coordinator,
            signals,
            &root.join("proc"),
        );
        match result {
            Ok(mut permit) => {
                permit.before_exec().unwrap();
                fs::write(root.join("launched"), "command").unwrap();
            }
            Err(error) => {
                diagnostic(&error.json());
                std::process::exit(error.code);
            }
        }
    }
}
