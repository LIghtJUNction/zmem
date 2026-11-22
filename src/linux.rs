use crate::{
    Result,
    config::Settings,
    dedup, json,
    linux_flags::{O_NOFOLLOW, O_NONBLOCK, O_PATH},
};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use zmem::{Decision, GIB, MIB, Policy, Snapshot, reclaim as reclaim_policy};

#[path = "background.rs"]
mod background;
#[path = "dedup_adapter.rs"]
mod dedup_adapter;
#[path = "protection.rs"]
mod protection;
#[path = "swap_lifecycle.rs"]
mod swap_lifecycle;
#[path = "swappiness.rs"]
mod swappiness;
type SwappinessGovernor = swappiness::Governor;

const RUNTIME: &str = "/run/zmem";
const CONTROL_FILE: &str = "/run/zmem/controls.tsv";
const CGROOT: &str = "/sys/fs/cgroup";
const SWAP_MUTATION_LOCK: &str = "/var/lib/zmem/.swap-mutation.lock";
static STOP: AtomicBool = AtomicBool::new(false);
// These Unix interfaces provide locking, signal delivery, and the native page size.
// The signal handler touches one lock-free atomic and performs no allocation.
unsafe extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
    fn fcntl(fd: i32, command: i32, ...) -> i32;
    fn signal(sig: i32, handler: usize) -> usize;
    fn kill(pid: i32, signal: i32) -> i32;
    fn getpagesize() -> i32;
    fn swapon(path: *const std::ffi::c_char, flags: i32) -> i32;
    fn swapoff(path: *const std::ffi::c_char) -> i32;
}
extern "C" fn stop_signal(_: i32) {
    STOP.store(true, Ordering::Relaxed);
}

pub fn read(path: impl AsRef<Path>) -> Result<String> {
    const LIMIT: u64 = 1024 * 1024;
    let path = path.as_ref();
    let mut out = String::new();
    File::open(path)?.take(LIMIT + 1).read_to_string(&mut out)?;
    if out.len() as u64 > LIMIT {
        return Err(format!(
            "file exceeds the {LIMIT}-byte read limit: {}",
            path.display()
        )
        .into());
    }
    Ok(out)
}
fn number(path: impl AsRef<Path>) -> u64 {
    read(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub fn root_required() -> Result<()> {
    let uid = read("/proc/self/status")?
        .lines()
        .find(|l| l.starts_with("Uid:"))
        .and_then(|l| l.split_whitespace().nth(2))
        .unwrap_or("-1")
        .to_owned();
    if uid != "0" {
        return Err("this operation requires root; use sudo zmem <command>".into());
    }
    Ok(())
}
pub fn command(program: &str, args: &[&str], seconds: u64) -> Result<String> {
    let exe = ["/usr/sbin", "/usr/bin", "/sbin", "/bin"]
        .iter()
        .map(|p| Path::new(p).join(program))
        .find(|p| p.is_file())
        .ok_or_else(|| format!("required system command not found: {program}"))?;
    let mut child = Command::new(exe)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let start = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            let output = child.wait_with_output()?;
            if !output.status.success() {
                return Err(format!(
                    "{program} failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )
                .into());
            }
            return Ok(String::from_utf8_lossy(&output.stdout).to_string());
        }
        if start.elapsed() > Duration::from_secs(seconds) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{program} timed out after {seconds}s").into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

// All swap mutations, including allocation, share one stable lock inode. Its
// descriptor survives exec so an uninterruptible child retains exclusion after
// the worker/controller times out. Closing only our descriptor is essential:
// LOCK_UN would also release the lock held through a child's duplicate.
struct SwapMutationLock(File);

impl SwapMutationLock {
    fn acquire() -> Result<Self> {
        root_required()?;
        secure_dir(Path::new("/var/lib/zmem"), true)?;
        let path = Path::new(SWAP_MUTATION_LOCK);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW)
            .open(path)?;
        Self::validate(&file)?;
        Self::lock(file)
    }

    fn validate(file: &File) -> Result<()> {
        let current = fs::symlink_metadata(SWAP_MUTATION_LOCK)?;
        let opened = file.metadata()?;
        if !current.is_file()
            || current.uid() != 0
            || current.mode() & 0o077 != 0
            || current.nlink() != 1
            || SwapIdentity::of(&current) != SwapIdentity::of(&opened)
        {
            return Err("invalid or replaced swap mutation lock; preserving swap files".into());
        }
        Ok(())
    }

    fn lock(file: File) -> Result<Self> {
        if unsafe { flock(file.as_raw_fd(), 2 | 4) } != 0 {
            return Err(format!(
                "swap mutation deferred while another helper holds the lock: {}",
                std::io::Error::last_os_error()
            )
            .into());
        }
        // Only a short-lived mutation scope makes the descriptor inheritable.
        // Allocation commands and the namespace/syscall helper must all retain
        // this open file description until their actual exit.
        let fd = file.as_raw_fd();
        let flags = unsafe { fcntl(fd, 1) }; // F_GETFD
        if flags < 0 || unsafe { fcntl(fd, 2, flags & !1) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self(file))
    }

    fn inherited(fd: i32) -> Result<Self> {
        if fd < 3 || unsafe { fcntl(fd, 1) } < 0 {
            return Err("missing inherited swap mutation lock".into());
        }
        // The private helper owns its inherited descriptor. It has not been
        // wrapped in a Rust File elsewhere in this fresh exec'd process.
        let file = unsafe { File::from_raw_fd(fd) };
        Self::validate(&file)?;
        Self::lock(file)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SwapAction {
    Activate(i32),
    Deactivate,
}

// Only a successfully decoded child response proves that the swap syscall
// returned this errno. Setup, transport and timeout errors remain distinct.
#[derive(Debug)]
struct SwapSyscallError(i32);

impl std::fmt::Display for SwapSyscallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "swap syscall failed: {}",
            std::io::Error::from_raw_os_error(self.0)
        )
    }
}

impl std::error::Error for SwapSyscallError {}

fn swap_syscall_errno(error: &(dyn std::error::Error + 'static)) -> Option<i32> {
    error
        .downcast_ref::<SwapSyscallError>()
        .map(|error| error.0)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SwapIdentity {
    dev: u64,
    ino: u64,
    rdev: u64,
}

impl SwapIdentity {
    fn of(meta: &fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            rdev: meta.rdev(),
        }
    }
}

fn verify_swap_identity(file: &File, expected: SwapIdentity) -> Result<()> {
    if SwapIdentity::of(&file.metadata()?) != expected {
        return Err("swap file identity changed across mount namespace entry".into());
    }
    Ok(())
}

fn swap_flags(priority: i32) -> Result<i32> {
    match priority {
        -1 => Ok(0),
        0..=32767 => Ok(0x8000 | priority),
        _ => Err("swap priority must be -1 or between 0 and 32767".into()),
    }
}

fn swap_target(path: &Path) -> Result<File> {
    if !path.is_absolute() || fs::canonicalize(path)? != path {
        return Err("swap helper requires a canonical absolute path".into());
    }
    // O_PATH pins the inode without reading swap data or opening a block device
    // for I/O. O_NOFOLLOW pins a replaced final symlink for metadata rejection.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_PATH | O_NOFOLLOW)
        .open(path)?;
    let meta = file.metadata()?;
    if meta.is_file() {
        if meta.uid() != 0 || meta.mode() & 0o077 != 0 {
            return Err("swap helper requires a root-owned private regular file".into());
        }
        secure_dir(path.parent().ok_or("missing swap parent")?, false)?;
    } else if !meta.file_type().is_block_device()
        || path.parent() != Some(Path::new("/dev"))
        || !path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("zram"))
            .is_some_and(|index| {
                !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit())
            })
    {
        return Err("swap helper only accepts private regular files or /dev/zram devices".into());
    }
    Ok(file)
}

fn swap_helper_args(
    executable: &Path,
    path: &Path,
    action: SwapAction,
    identity: SwapIdentity,
    lock_fd: i32,
) -> Result<Vec<String>> {
    if lock_fd < 3 {
        return Err("invalid inherited swap mutation descriptor".into());
    }
    let (verb, priority) = match action {
        SwapAction::Activate(priority) => {
            swap_flags(priority)?;
            ("__swap-on", priority)
        }
        SwapAction::Deactivate => ("__swap-off", -1),
    };
    Ok(vec![
        "--target".into(),
        "1".into(),
        "--mount".into(),
        "--root".into(),
        "--wdns=/".into(),
        "--no-fork".into(),
        "--".into(),
        executable
            .to_str()
            .ok_or("invalid helper executable")?
            .into(),
        verb.into(),
        path.to_str().ok_or("invalid swap path")?.into(),
        priority.to_string(),
        identity.dev.to_string(),
        identity.ino.to_string(),
        identity.rdev.to_string(),
        lock_fd.to_string(),
    ])
}

fn swap_reply_errno(reply: &str) -> Result<i32> {
    let code = reply
        .strip_prefix("zmem-swap-v1\t")
        .and_then(|value| value.strip_suffix('\n'))
        .filter(|value| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
        .ok_or("invalid swap helper response")?
        .parse::<i32>()?;
    if !(0..=4095).contains(&code) {
        return Err("invalid swap helper errno".into());
    }
    Ok(code)
}

fn host_swap_command(
    path: &Path,
    action: SwapAction,
    seconds: u64,
    lock: &SwapMutationLock,
) -> Result<()> {
    root_required()?;
    let pinned = swap_target(path)?;
    host_swap_pinned(path, action, seconds, lock, &pinned)
}

fn host_swap_pinned(
    path: &Path,
    action: SwapAction,
    seconds: u64,
    lock: &SwapMutationLock,
    pinned: &File,
) -> Result<()> {
    let identity = SwapIdentity::of(&pinned.metadata()?);
    SwapMutationLock::validate(&lock.0)?;
    let args = swap_helper_args(
        &std::env::current_exe()?,
        path,
        action,
        identity,
        lock.0.as_raw_fd(),
    )?;
    let executable = ["/usr/bin/nsenter", "/bin/nsenter"]
        .into_iter()
        .map(Path::new)
        .find(|path| path.is_file())
        .ok_or("required host mount namespace helper is missing: nsenter")?;
    let mut child = Command::new(executable)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let start = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            let output = child.wait_with_output()?;
            if !output.status.success() {
                return Err(format!(
                    "host swap helper failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )
                .into());
            }
            let errno = swap_reply_errno(std::str::from_utf8(&output.stdout)?)?;
            return if errno == 0 {
                Ok(())
            } else {
                Err(SwapSyscallError(errno).into())
            };
        }
        if STOP.load(Ordering::Relaxed) || start.elapsed() > Duration::from_secs(seconds) {
            // nsenter --no-fork execs the single helper in this PID. Inherit
            // the enclosing worker's process group so cancelling a grow/shrink
            // worker also reaches this child. Never wait forever for a syscall
            // that has an uninterruptible operation and pending SIGKILL.
            let _ = child.kill();
            let cleanup = Instant::now();
            while child.try_wait()?.is_none() && cleanup.elapsed() < Duration::from_secs(1) {
                thread::sleep(Duration::from_millis(20));
            }
            return Err(
                "host swap helper was interrupted or exceeded its deadline; file retained".into(),
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Private child protocol: execute exactly one syscall after nsenter has
/// selected PID 1's mount namespace, root and working directory. Numeric errno
/// is returned separately from setup/validation failures.
pub fn swap_helper(action: &str, args: &[String]) -> Result<()> {
    root_required()?;
    if !matches!(action, "__swap-on" | "__swap-off") || !matches!(args.len(), 5 | 6) {
        return Err("invalid private swap helper request".into());
    }
    for (current, host) in [
        ("/proc/self/ns/mnt", "/proc/1/ns/mnt"),
        ("/", "/proc/1/root"),
    ] {
        let current = fs::metadata(current)?;
        let host = fs::metadata(host)?;
        if (current.dev(), current.ino()) != (host.dev(), host.ino()) {
            return Err("swap helper is outside the host mount namespace or root".into());
        }
    }
    let priority = args[1].parse::<i32>()?;
    let flags = swap_flags(priority)?;
    if action == "__swap-off" && priority != -1 {
        return Err("invalid deactivation priority".into());
    }
    let expected = SwapIdentity {
        dev: args[2].parse()?,
        ino: args[3].parse()?,
        rdev: args[4].parse()?,
    };
    let _lock = if let Some(fd) = args.get(5) {
        SwapMutationLock::inherited(fd.parse()?)?
    } else {
        SwapMutationLock::acquire()?
    };
    let pinned = swap_target(Path::new(&args[0]))?;
    verify_swap_identity(&pinned, expected)?;
    // Resolve the syscall through the pinned host file, so a concurrent rename
    // cannot substitute another inode after the identity check.
    let name = std::ffi::CString::new(format!("/proc/self/fd/{}", pinned.as_raw_fd()))?;
    let result = unsafe {
        if action == "__swap-on" {
            swapon(name.as_ptr(), flags)
        } else {
            swapoff(name.as_ptr())
        }
    };
    let errno = if result == 0 {
        0
    } else {
        std::io::Error::last_os_error()
            .raw_os_error()
            .ok_or("swap syscall did not provide errno")?
    };
    println!("zmem-swap-v1\t{errno}");
    Ok(())
}

fn swap_busy(error: &(dyn std::error::Error + 'static)) -> bool {
    swap_syscall_errno(error) == Some(16)
}

fn activate_existing_swap(path: &Path, priority: i32, lock: &SwapMutationLock) -> Result<()> {
    match host_swap_command(path, SwapAction::Activate(priority), 10, lock) {
        Err(error) if swap_busy(error.as_ref()) => {
            // An orphan mount can change /proc/swaps' displayed name. EBUSY
            // preserves an existing file without claiming an alias identity:
            // some filesystems can also report a temporarily busy activation.
            println!(
                "{{\"event\":\"swap_activation_busy_preserved\",\"path\":{},\"errno\":16,\"active_identity_confirmed\":false}}",
                json::quote(&path.to_string_lossy())
            );
            Ok(())
        }
        result => result,
    }
}

pub fn atomic_write(path: &Path, text: &str, mode: u32) -> Result<()> {
    let name = path
        .file_name()
        .ok_or("missing file name")?
        .to_string_lossy();
    let temp = path.with_file_name(format!(".{name}.{}", std::process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(mode)
        .open(&temp)?;
    let result = (|| -> Result<()> {
        file.write_all(text.as_bytes())?;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
fn secure_dir(path: &Path, create: bool) -> Result<()> {
    if create {
        fs::create_dir_all(path)?;
    }
    let mut current = PathBuf::from("/");
    for c in path.components().skip(1) {
        current.push(c.as_os_str());
        let m = fs::symlink_metadata(&current)?;
        if m.file_type().is_symlink() || !m.is_dir() || m.uid() != 0 || m.mode() & 0o022 != 0 {
            return Err(format!(
                "requires a root-owned directory without writable parents or symlinks: {}",
                current.display()
            )
            .into());
        }
    }
    Ok(())
}
pub struct InstanceLock(File);
impl InstanceLock {
    pub fn acquire() -> Result<Self> {
        root_required()?;
        secure_dir(Path::new(RUNTIME), true)?;
        let p = Path::new(RUNTIME).join("daemon.lock");
        if fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err("refusing symlink lock file".into());
        }
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(p)?;
        // LOCK_EX | LOCK_NB; the file descriptor stays owned by this guard.
        if unsafe { flock(f.as_raw_fd(), 2 | 4) } != 0 {
            return Err("another zmem controller is active".into());
        }
        Ok(Self(f))
    }
}
impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = unsafe { flock(self.0.as_raw_fd(), 8) };
    }
}

#[derive(Clone, Debug)]
pub struct Swap {
    pub path: String,
    pub kind: String,
    pub size: u64,
    pub used: u64,
    pub priority: i32,
}
pub fn parse_swaps(text: &str) -> Result<Vec<Swap>> {
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let p: Vec<_> = line.split_whitespace().collect();
        if p.len() != 5 {
            return Err("invalid /proc/swaps row".into());
        }
        out.push(Swap {
            path: unescape_path(p[0]),
            kind: p[1].into(),
            size: p[2]
                .parse::<u64>()?
                .checked_mul(1024)
                .ok_or("swap size overflow")?,
            used: p[3]
                .parse::<u64>()?
                .checked_mul(1024)
                .ok_or("swap usage overflow")?,
            priority: p[4].parse()?,
        });
    }
    Ok(out)
}
fn unescape_path(s: &str) -> String {
    s.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}
pub fn swaps() -> Result<Vec<Swap>> {
    parse_swaps(&read("/proc/swaps")?)
}
pub fn parse_meminfo(text: &str) -> Result<Snapshot> {
    let values: BTreeMap<_, _> = text
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k, v.split_whitespace().next()?.parse::<u64>().ok()?))
        })
        .collect();
    let kb = |k: &str| -> Result<u64> {
        values
            .get(k)
            .ok_or_else(|| format!("missing /proc/meminfo {k}"))?
            .checked_mul(1024)
            .ok_or_else(|| "meminfo count overflow".into())
    };
    Ok(Snapshot {
        total_bytes: kb("MemTotal")?,
        available_bytes: kb("MemAvailable")?,
        swap_total_bytes: kb("SwapTotal")?,
        swap_free_bytes: kb("SwapFree")?,
        ..Snapshot::default()
    })
}
fn psi(text: &str, kind: &str) -> (u16, u64) {
    let line = text.lines().find(|l| l.starts_with(kind)).unwrap_or("");
    let mut avg = 0;
    let mut total = 0;
    for word in line.split_whitespace() {
        if let Some(v) = word.strip_prefix("avg10=") {
            avg = (v.parse::<f64>().unwrap_or(0.0) * 100.0).clamp(0.0, 10000.0) as u16;
        }
        if let Some(v) = word.strip_prefix("total=") {
            total = v.parse().unwrap_or(0);
        }
    }
    (avg, total)
}
fn vmstats(text: &str) -> (u64, u64) {
    let mut input = 0;
    let mut output = 0;
    for l in text.lines() {
        let mut p = l.split_whitespace();
        match p.next() {
            Some("pswpin") => input = p.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            Some("pswpout") => output = p.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            _ => {}
        }
    }
    (input, output)
}
#[derive(Clone, Debug)]
pub struct Zram {
    pub name: String,
    pub original: u64,
    pub compressed: u64,
    pub resident: u64,
    pub same_pages: u64,
    pub logical_size: u64,
    pub logical_used: u64,
    pub memory_limit: u64,
    pub algorithm: String,
}
#[derive(Clone, Debug, Default)]
pub struct Paging {
    pub rates_available: bool,
    pub zram_rates_available: bool,
    pub disk_swap_estimate_available: bool,
    pub zswap_enabled: Option<bool>,
    pub sample_interval_ms: u64,
    pub file_refault_bytes_per_sec: u64,
    pub anon_refault_bytes_per_sec: u64,
    pub direct_scan_bytes_per_sec: u64,
    pub zram_read_bytes_per_sec: u64,
    pub estimated_disk_swap_in_bytes_per_sec: u64,
}
#[derive(Clone, Debug, Default)]
pub struct Managed {
    pub background_groups: usize,
    pub populated_background_groups: usize,
    pub background_tasks: usize,
    pub background_memory_bytes: u64,
    pub reclaim_candidates: usize,
    pub coverage_complete: bool,
}
#[derive(Clone, Debug, Default)]
pub struct Applied {
    pub swappiness_requested: Option<u16>,
    pub swappiness_reason: &'static str,
    pub protection_budget_bytes: u64,
    pub protected_critical_scopes: usize,
    pub protection_complete: bool,
    pub protection_errors: Vec<String>,
    pub protection_sampled_at: Option<u64>,
}
#[derive(Clone, Debug, Default)]
pub struct Telemetry {
    pub sample: Snapshot,
    pub swaps: Vec<Swap>,
    pub zram: Vec<Zram>,
    pub ksm: dedup::KsmSnapshot,
    pub dedup: dedup_adapter::Report,
    pub sampled_at: u64,
    pub paging: Paging,
    pub managed: Managed,
    pub background: background::Report,
    pub applied: Applied,
    pub file_cache_bytes: u64,
    pub anon_bytes: u64,
    pub free_bytes: u64,
}
pub struct Sampler {
    last: Option<(Instant, [u64; 9])>,
    last_zram_key: Option<String>,
    page_size: u64,
}
impl Sampler {
    pub fn new() -> Self {
        Self {
            last: None,
            last_zram_key: None,
            page_size: control_page_size().unwrap_or(4096),
        }
    }
    pub fn sample(&mut self) -> Result<Telemetry> {
        let meminfo = read("/proc/meminfo")?;
        let mut sample = parse_meminfo(&meminfo)?;
        let memory = read("/proc/pressure/memory")?;
        let io = read("/proc/pressure/io")?;
        let (some, ts) = psi(&memory, "some ");
        let (full, tf) = psi(&memory, "full ");
        let (ioavg, ti) = psi(&io, "full ");
        let vmtext = read("/proc/vmstat")?;
        let (pin, pout) = vmstats(&vmtext);
        let vm = counters(&vmtext);
        let file_refault = vm.get("workingset_refault_file").copied().unwrap_or(0);
        let anon_refault = vm.get("workingset_refault_anon").copied().unwrap_or(0);
        let direct_scan = vm.get("pgscan_direct").copied().unwrap_or(0);
        let swaps = swaps()?;
        let mut zram_sectors = Some(0u64);
        let mut zram_key = String::new();
        for s in &swaps {
            let Some(name) = s
                .path
                .strip_prefix("/dev/")
                .filter(|n| n.starts_with("zram") && !n.contains('/'))
            else {
                continue;
            };
            zram_key.push_str(&format!("{name}:{};", s.size));
            let sectors = read(format!("/sys/block/{name}/stat"))
                .ok()
                .and_then(|text| {
                    text.split_whitespace()
                        .nth(2)
                        .and_then(|s| s.parse::<u64>().ok())
                });
            zram_sectors = zram_sectors
                .zip(sectors)
                .and_then(|(sum, n)| sum.checked_add(n));
        }
        let current = [
            ts,
            tf,
            ti,
            pin,
            pout,
            file_refault,
            anon_refault,
            direct_scan,
            zram_sectors.unwrap_or(0),
        ];
        let zswap_enabled = match read("/sys/module/zswap/parameters/enabled") {
            Ok(text) => match text.trim() {
                "Y" | "y" | "1" => Some(true),
                "N" | "n" | "0" => Some(false),
                _ => None,
            },
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                Some(false)
            }
            Err(_) => None,
        };
        let mut paging = Paging {
            zswap_enabled,
            ..Paging::default()
        };
        let nowi = Instant::now();
        sample.memory_some_bp = some;
        sample.memory_full_bp = full;
        sample.io_full_bp = ioavg;
        if let Some((last, counters)) = self.last.filter(|(last, previous)| {
            nowi.duration_since(*last) >= Duration::from_millis(100)
                && current[..8].iter().zip(&previous[..8]).all(|(a, b)| a >= b)
        }) {
            let us = nowi.duration_since(last).as_micros().max(1);
            let percent =
                |a: u64, b: u64| ((a.saturating_sub(b) as u128 * 10000 / us).min(10000)) as u16;
            sample.memory_some_bp = percent(ts, counters[0]);
            sample.memory_full_bp = percent(tf, counters[1]);
            sample.io_full_bp = percent(ti, counters[2]);
            let rate = |a: u64, b: u64| {
                ((a.saturating_sub(b) as u128 * self.page_size as u128 * 1_000_000 / us)
                    .min(u64::MAX as u128)) as u64
            };
            sample.swap_in_bytes_per_sec = rate(pin, counters[3]);
            sample.swap_out_bytes_per_sec = rate(pout, counters[4]);
            paging.rates_available = [
                "workingset_refault_file",
                "workingset_refault_anon",
                "pgscan_direct",
                "pswpin",
                "pswpout",
            ]
            .iter()
            .all(|key| vm.contains_key(*key));
            paging.sample_interval_ms = (us / 1000).min(u64::MAX as u128) as u64;
            paging.file_refault_bytes_per_sec = rate(file_refault, counters[5]);
            paging.anon_refault_bytes_per_sec = rate(anon_refault, counters[6]);
            paging.direct_scan_bytes_per_sec = rate(direct_scan, counters[7]);
            paging.zram_rates_available = paging.rates_available
                && zram_sectors.is_some_and(|n| n >= counters[8])
                && self.last_zram_key.as_ref() == Some(&zram_key);
            if paging.zram_rates_available {
                paging.zram_read_bytes_per_sec =
                    ((current[8].saturating_sub(counters[8]) as u128 * 512 * 1_000_000 / us)
                        .min(u64::MAX as u128)) as u64;
                paging.estimated_disk_swap_in_bytes_per_sec = sample
                    .swap_in_bytes_per_sec
                    .saturating_sub(paging.zram_read_bytes_per_sec);
            }
            paging.disk_swap_estimate_available =
                paging.zram_rates_available && paging.zswap_enabled == Some(false);
        }
        self.last = Some((nowi, current));
        self.last_zram_key = zram_sectors.map(|_| zram_key);
        let mut zram = Vec::new();
        for s in &swaps {
            if let Some(name) = s
                .path
                .strip_prefix("/dev/")
                .filter(|n| n.starts_with("zram") && !n.contains('/'))
            {
                let data = read(format!("/sys/block/{name}/mm_stat")).unwrap_or_default();
                let v: Vec<u64> = data
                    .split_whitespace()
                    .filter_map(|n| n.parse().ok())
                    .collect();
                if v.len() >= 6 {
                    zram.push(Zram {
                        name: name.into(),
                        original: v[0],
                        compressed: v[1],
                        resident: v[2],
                        same_pages: v[5],
                        logical_size: s.size,
                        logical_used: s.used,
                        memory_limit: v[3],
                        algorithm: read(format!("/sys/block/{name}/comp_algorithm"))
                            .ok()
                            .and_then(|s| {
                                s.split_once('[')
                                    .and_then(|(_, s)| s.split_once(']'))
                                    .map(|(name, _)| name.to_owned())
                            })
                            .unwrap_or_default(),
                    });
                }
            }
        }
        let background = background::sample();
        let managed = managed_snapshot(&background);
        Ok(Telemetry {
            sample,
            swaps,
            zram,
            ksm: dedup::read_ksm(Path::new(dedup::KSM_ROOT)),
            dedup: dedup_adapter::Report::default(),
            sampled_at: now(),
            paging,
            managed,
            background,
            file_cache_bytes: meminfo_bytes(&meminfo, "Cached"),
            anon_bytes: meminfo_bytes(&meminfo, "AnonPages"),
            free_bytes: meminfo_bytes(&meminfo, "MemFree"),
            applied: Applied::default(),
        })
    }
}

fn counters(text: &str) -> BTreeMap<String, u64> {
    text.lines()
        .filter_map(|line| {
            let mut values = line.split_whitespace();
            Some((values.next()?.to_owned(), values.next()?.parse().ok()?))
        })
        .collect()
}
fn meminfo_bytes(text: &str, key: &str) -> u64 {
    text.lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            (name == key)
                .then(|| value.split_whitespace().next()?.parse::<u64>().ok())
                .flatten()
        })
        .unwrap_or(0)
        .saturating_mul(1024)
}
fn reclaim_sample(path: &Path) -> Result<reclaim_policy::Sample> {
    let meta = fs::metadata(path)?;
    let stats = counters(&read(path.join("memory.stat"))?);
    let events = counters(&read(path.join("cgroup.events"))?);
    let get = |key: &str| stats.get(key).copied().unwrap_or(0);
    Ok(reclaim_policy::Sample {
        dev: meta.dev(),
        ino: meta.ino(),
        populated: events.get("populated") == Some(&1),
        current_bytes: read(path.join("memory.current"))?.trim().parse()?,
        file_bytes: get("file"),
        inactive_file_bytes: get("inactive_file"),
        dirty_bytes: get("file_dirty"),
        writeback_bytes: get("file_writeback"),
        shmem_bytes: get("shmem"),
        pgsteal_proactive: stats.get("pgsteal_proactive").copied(),
    })
}
fn managed_snapshot(background: &background::Report) -> Managed {
    let mut result = Managed {
        background_groups: background.groups.len(),
        coverage_complete: background.sample_complete,
        ..Managed::default()
    };
    let mut pids = std::collections::BTreeSet::new();
    for group in &background.groups {
        result.background_memory_bytes = result
            .background_memory_bytes
            .saturating_add(group.current_bytes);
        if !group.populated {
            continue;
        }
        result.populated_background_groups += 1;
        let mut pending = vec![(group.path.clone(), 0)];
        let mut visited = 0;
        while let Some((path, depth)) = pending.pop() {
            visited += 1;
            if visited > 1024 || depth > 8 {
                result.coverage_complete = false;
                break;
            }
            if let Ok(text) = read(path.join("cgroup.procs")) {
                pids.extend(
                    text.split_whitespace()
                        .filter_map(|s| s.parse::<u32>().ok()),
                );
            } else {
                result.coverage_complete = false;
            }
            match fs::read_dir(&path) {
                Ok(entries) => {
                    for (index, entry) in entries.take(1025).enumerate() {
                        if index == 1024 {
                            result.coverage_complete = false;
                            break;
                        }
                        match entry {
                            Ok(e) if e.file_type().is_ok_and(|t| t.is_dir()) => {
                                pending.push((e.path(), depth + 1))
                            }
                            Err(_) => result.coverage_complete = false,
                            _ => {}
                        }
                    }
                    if pending.len() > 1024 {
                        result.coverage_complete = false;
                        break;
                    }
                }
                Err(_) => result.coverage_complete = false,
            }
        }
    }
    result.background_tasks = pids.len();
    result
}
fn optional_number(value: Option<u64>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "null".into())
}
fn observed_number(path: &str) -> Option<u64> {
    read(path).ok()?.trim().parse().ok()
}
fn process_start_ticks() -> u64 {
    read("/proc/self/stat")
        .ok()
        .and_then(|s| {
            s.rsplit_once(") ")
                .and_then(|(_, tail)| tail.split_whitespace().nth(19))
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or(0)
}

pub fn telemetry_json(
    t: &Telemetry,
    d: &Decision,
    profile: &str,
    worker: Option<&str>,
    errors: &[String],
    processes: bool,
) -> String {
    let s = &t.sample;
    let mut all_errors = errors.to_vec();
    for error in &t.applied.protection_errors {
        if !all_errors.contains(error) {
            all_errors.push(error.clone());
        }
    }
    for error in &t.background.errors {
        if !all_errors.contains(error) {
            all_errors.push(error.clone());
        }
    }
    let swap_rows = t
        .swaps
        .iter()
        .map(|x| {
            format!(
                "{{\"path\":{},\"kind\":{},\"size_bytes\":{},\"used_bytes\":{},\"priority\":{}}}",
                json::quote(&x.path),
                json::quote(&x.kind),
                x.size,
                x.used,
                x.priority
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let zram=t.zram.iter().map(|z|format!("{{\"device\":{},\"original_bytes\":{},\"compressed_bytes\":{},\"resident_bytes\":{},\"estimated_saved_bytes\":{},\"same_pages\":{}}}",json::quote(&z.name),z.original,z.compressed,z.resident,z.original.saturating_sub(z.resident),z.same_pages)).collect::<Vec<_>>().join(",");
    let mut status = format!(
        "{{\"schema\":\"zmem.status.v1\",\"version\":{},\"pid\":{},\"sampled_at_unix\":{},\"profile\":{},\"pressure\":{},\"reason\":{},\"memory\":{{\"total_bytes\":{},\"available_bytes\":{}}},\"swap\":{{\"total_bytes\":{},\"free_bytes\":{},\"in_bytes_per_sec\":{},\"out_bytes_per_sec\":{},\"devices\":[{}]}},\"psi\":{{\"memory_some_bp\":{},\"memory_full_bp\":{},\"io_full_bp\":{}}},\"zram\":[{}],\"ksm\":{},\"plan\":{{\"grow_swap_bytes\":{},\"shrink_allowed\":{},\"background_high_percent\":{},\"reclaim_request_bytes\":{},\"swappiness_hint\":{}}},\"worker\":{},\"errors\":{},\"processes\":{}}}",
        json::quote(env!("CARGO_PKG_VERSION")),
        std::process::id(),
        t.sampled_at,
        json::quote(profile),
        json::quote(d.pressure.as_str()),
        json::quote(d.reason.as_str()),
        s.total_bytes,
        s.available_bytes,
        s.swap_total_bytes,
        s.swap_free_bytes,
        optional_number(t.paging.rates_available.then_some(s.swap_in_bytes_per_sec)),
        optional_number(t.paging.rates_available.then_some(s.swap_out_bytes_per_sec)),
        swap_rows,
        s.memory_some_bp,
        s.memory_full_bp,
        s.io_full_bp,
        zram,
        dedup_adapter::ksm_json(&t.ksm),
        d.grow_swap_bytes,
        d.shrink_allowed,
        d.background_high_percent,
        d.reclaim_bytes,
        d.recommended_swappiness,
        worker.map(json::quote).unwrap_or_else(|| "null".into()),
        json::strings(&all_errors),
        if processes {
            top_processes(20)
        } else {
            "[]".into()
        }
    );
    status.pop();
    let p = &t.paging;
    let m = &t.managed;
    let a = &t.applied;
    let boot_id = read("/proc/sys/kernel/random/boot_id").unwrap_or_default();
    let device_details = t.zram.iter().map(|z| format!(
        "{{\"device\":{},\"logical_size_bytes\":{},\"logical_used_bytes\":{},\"resident_bytes\":{},\"memory_limit_bytes\":{},\"algorithm\":{},\"mode\":\"active_device_preserved\"}}",
        json::quote(&z.name),z.logical_size,z.logical_used,z.resident,z.memory_limit,json::quote(&z.algorithm)
    )).collect::<Vec<_>>().join(",");
    status.push_str(&format!(
        ",\"memory_pressure\":{},\"io_congested\":{},\"instance\":{{\"pid\":{},\"start_ticks\":{},\"boot_id\":{}}},\"paging\":{{\"rates_available\":{},\"zram_rates_available\":{},\"disk_swap_estimate_available\":{},\"zswap_enabled\":{},\"sample_interval_ms\":{},\"file_refault_bytes_per_sec\":{},\"anon_refault_bytes_per_sec\":{},\"direct_scan_bytes_per_sec\":{},\"zram_read_bytes_per_sec\":{},\"estimated_disk_swap_in_bytes_per_sec\":{}}},\"memory_usage\":{{\"free_bytes\":{},\"file_cache_bytes\":{},\"anonymous_bytes\":{}}},\"controls\":{{\"swappiness_requested\":{},\"swappiness_actual\":{},\"swappiness_reason\":{},\"page_cluster_actual\":{},\"protection_budget_bytes\":{},\"protected_critical_scopes\":{},\"protection_complete\":{},\"protection_sampled_at_unix\":{},\"protection_errors\":{}}},\"managed\":{{\"background_groups\":{},\"populated_background_groups\":{},\"background_tasks\":{},\"background_memory_bytes\":{},\"reclaim_candidates\":{},\"coverage_complete\":{}}},\"zram_devices\":[{}]}}",
        json::quote(d.pressure.as_str()),d.io_congested,std::process::id(),process_start_ticks(),json::quote(boot_id.trim()),
        p.rates_available,p.zram_rates_available,p.disk_swap_estimate_available,
        p.zswap_enabled.map(|b| b.to_string()).unwrap_or_else(|| "null".into()),p.sample_interval_ms,
        optional_number(p.rates_available.then_some(p.file_refault_bytes_per_sec)),
        optional_number(p.rates_available.then_some(p.anon_refault_bytes_per_sec)),
        optional_number(p.rates_available.then_some(p.direct_scan_bytes_per_sec)),
        optional_number(p.zram_rates_available.then_some(p.zram_read_bytes_per_sec)),
        optional_number(p.disk_swap_estimate_available.then_some(p.estimated_disk_swap_in_bytes_per_sec)),
        t.free_bytes,t.file_cache_bytes,t.anon_bytes,
        optional_number(a.swappiness_requested.map(u64::from)),optional_number(observed_number("/proc/sys/vm/swappiness")),json::quote(a.swappiness_reason),optional_number(observed_number("/proc/sys/vm/page-cluster")),a.protection_budget_bytes,a.protected_critical_scopes,a.protection_complete,optional_number(a.protection_sampled_at),json::strings(&a.protection_errors),
        m.background_groups,m.populated_background_groups,m.background_tasks,m.background_memory_bytes,m.reclaim_candidates,m.coverage_complete,device_details
    ));
    status.pop();
    status.push_str(&format!(
        ",\"background\":{},\"dedup\":{}}}",
        background::json(&t.background),
        dedup_adapter::report_json(&t.dedup)
    ));
    status
}
pub fn top_processes(limit: usize) -> String {
    let mut rows = Vec::new();
    if let Ok(entries) = fs::read_dir("/proc") {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Ok(pid) = name.parse::<u32>() else {
                continue;
            };
            let Ok(status) = read(e.path().join("status")) else {
                continue;
            };
            let mut command = String::new();
            let mut rss = 0u64;
            let mut swap = 0u64;
            for line in status.lines() {
                if let Some(n) = line.strip_prefix("Name:\t") {
                    command = n.to_owned();
                } else if line.starts_with("VmRSS:") {
                    rss = line
                        .split_whitespace()
                        .nth(1)
                        .and_then(|x| x.parse::<u64>().ok())
                        .unwrap_or(0)
                        .saturating_mul(1024);
                } else if line.starts_with("VmSwap:") {
                    swap = line
                        .split_whitespace()
                        .nth(1)
                        .and_then(|x| x.parse::<u64>().ok())
                        .unwrap_or(0)
                        .saturating_mul(1024);
                }
            }
            if rss > 0 || swap > 0 {
                rows.push((rss.saturating_add(swap), pid, command, rss, swap));
            }
        }
    }
    rows.sort_unstable_by_key(|a| std::cmp::Reverse(a.0));
    rows.truncate(limit);
    format!(
        "[{}]",
        rows.iter()
            .map(|(_, pid, name, rss, swap)| {
                let cgroup = read(format!("/proc/{pid}/cgroup")).ok().and_then(|text| {
                    text.lines().find_map(|line| line.strip_prefix("0::").map(str::to_owned))
                });
                let managed = cgroup.as_deref().map(is_managed_background_path);
                format!(
                    "{{\"pid\":{pid},\"name\":{},\"rss_bytes\":{rss},\"swap_bytes\":{swap},\"cgroup\":{},\"managed_background\":{}}}",
                    json::quote(name), cgroup.as_deref().map(json::quote).unwrap_or_else(|| "null".into()),
                    managed.map(|v| v.to_string()).unwrap_or_else(|| "null".into())
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn is_managed_background_path(path: &str) -> bool {
    const ROOT: &str = "/zmem.slice/zmem-background.slice";
    let Some(index) = path.find(ROOT) else {
        return false;
    };
    let end = index + ROOT.len();
    if !path.starts_with('/') || (path.len() != end && path.as_bytes()[end] != b'/') {
        return false;
    }
    is_managed_background_control(&Path::new(CGROOT).join(&path[1..end]).join("memory.high"))
}

#[derive(Clone)]
struct Control {
    path: PathBuf,
    original: String,
    last: String,
    dev: u64,
    ino: u64,
    pending_previous: Option<String>,
}
struct Controls {
    entries: BTreeMap<PathBuf, Control>,
    swappiness_governor: SwappinessGovernor,
    background: background::State,
    pub protection_budget_bytes: u64,
    pub protected_critical_scopes: usize,
    pub protection_complete: bool,
    pub protection_errors: Vec<String>,
    pub protection_sampled_at: Option<u64>,
}
impl Controls {
    fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            swappiness_governor: SwappinessGovernor::new(),
            background: background::State::default(),
            protection_budget_bytes: 0,
            protected_critical_scopes: 0,
            protection_complete: false,
            protection_errors: Vec::new(),
            protection_sampled_at: None,
        }
    }
    fn prune_stale(&mut self) -> bool {
        let before = self.entries.len();
        self.entries
            .retain(|_, entry| !stale_control(entry, fs::metadata(&entry.path)));
        self.entries.len() != before
    }
    fn protection_reset_pending(&self) -> bool {
        self.entries.values().any(|entry| {
            entry.last != "0"
                && entry.original == "0"
                && entry.path.ends_with("memory.low")
                && fs::metadata(&entry.path)
                    .is_ok_and(|meta| meta.dev() == entry.dev && meta.ino() == entry.ino)
                && read(&entry.path).is_ok_and(|value| value.trim() == "0")
        })
    }
    fn save(&self) -> Result<()> {
        let text = self
            .entries
            .values()
            .map(|e| {
                format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\n",
                    e.path.display(),
                    e.original,
                    e.last,
                    e.dev,
                    e.ino,
                    e.pending_previous.as_deref().unwrap_or("")
                )
            })
            .collect::<String>();
        atomic_write(Path::new(CONTROL_FILE), &text, 0o600)
    }
    fn set(&mut self, path: &Path, value: &str, nonblock: bool) -> Result<()> {
        if !valid_control(path) {
            return Err("invalid control path".into());
        }
        let page_size = control_page_size()?;
        let value = canonical_control_value(path, value, page_size)?;
        let old = read(path)?.trim().to_string();
        let meta = fs::metadata(path)?;
        if self
            .entries
            .get(path)
            .is_some_and(|entry| entry.dev != meta.dev() || entry.ino != meta.ino())
        {
            // systemd may destroy an unused slice and later recreate it at
            // the same path. Its new controls have a new baseline.
            self.entries.remove(path);
            self.save()?;
        }
        if old == value {
            return Ok(());
        }
        let mut managed_reset = false;
        if let Some(entry) = self.entries.get(path) {
            if !owned_control_value(
                path,
                &entry.last,
                entry.pending_previous.as_deref(),
                &old,
                page_size,
            )? {
                // systemd can reapply the default unlimited value when a new
                // scope joins our dedicated aggregate slice. zmem owns that
                // slice's policy limit while running. An explicit `max` there
                // is a temporary reset; finite external limits stay protected.
                if managed_background_high_reset(path, &entry.original, &old)
                    || owned_protection_default_reset(path, &entry.original, &old)
                {
                    managed_reset = true;
                } else {
                    return Err(
                        format!("external control change preserved: {}", path.display()).into(),
                    );
                }
            }
        }
        let previous = self.entries.get(path).cloned();
        let entry = self
            .entries
            .entry(path.to_path_buf())
            .or_insert_with(|| Control {
                path: path.into(),
                original: old.clone(),
                last: old.clone(),
                dev: meta.dev(),
                ino: meta.ino(),
                pending_previous: None,
            });
        entry.last = value.clone();
        // Persist both sides of the pending write. If the process exits before
        // or after the kernel write, recovery can recognize the owned value.
        entry.pending_previous = Some(old.clone());
        if let Err(error) = self.save() {
            match previous {
                Some(entry) => {
                    self.entries.insert(path.into(), entry);
                }
                None => {
                    self.entries.remove(path);
                }
            }
            return Err(error);
        }
        let result = write_control(path, &value, nonblock);
        let observed = read(path).map(|text| text.trim().to_owned());
        if result.is_err()
            && observed
                .as_deref()
                .is_ok_and(|current| current == old.as_str())
        {
            // The write did not change the control. Restore the preceding
            // ownership record, including the absence of a first-write record.
            match previous {
                Some(entry) => {
                    self.entries.insert(path.into(), entry);
                }
                None => {
                    self.entries.remove(path);
                }
            }
            self.save()?;
            return result;
        }
        if let Ok(current) = &observed {
            // Never adopt an unknown read-back value: another controller may
            // have written it. Clearing pending prevents recovery from treating
            // a later return to the previous value as our in-flight write.
            if let Some(entry) = self.entries.get_mut(path) {
                entry.pending_previous = None;
            }
            self.save()?;
            if current != &value {
                return Err(format!(
                    "control read-back differs from requested value; external change preserved: {}",
                    path.display()
                )
                .into());
            }
        }
        result?;
        observed?;
        if managed_reset {
            println!(
                "{{\"event\":{},\"path\":{},\"observed\":{},\"applied\":{}}}",
                json::quote(if path.ends_with("memory.low") {
                    "protection_default_reapplied"
                } else {
                    "managed_background_limit_reapplied"
                }),
                json::quote(&path.to_string_lossy()),
                json::quote(&old),
                json::quote(&value)
            );
        }
        Ok(())
    }
    fn protect(&mut self, path: &Path, bytes: u64) -> Result<bool> {
        if !valid_control(path) || path.file_name().and_then(|s| s.to_str()) != Some("memory.low") {
            return Err("invalid protection control path".into());
        }
        let current = read(path)?;
        let current = current.trim();
        let meta = fs::metadata(path)?;
        let page_size = control_page_size()?;
        let requested = canonical_control_value(path, &bytes.to_string(), page_size)?;
        if can_manage_protection(
            self.entries.get(path),
            path,
            current,
            meta.dev(),
            meta.ino(),
            page_size,
        )? {
            // Unlike the former minimum-only helper, an owned allocation can
            // also decrease when its scope's working set or budget changes.
            self.set(path, &requested, false)?;
            return Ok(true);
        }
        // A pre-existing nonzero control belongs to another administrator.
        // Preserve it even when it is smaller than our desired allocation.
        Ok(current == "max" || current.parse::<u64>()? >= requested.parse::<u64>()?)
    }

    fn forget_protection(&mut self, path: &Path) -> Result<()> {
        if let Some(entry) = self.entries.remove(path) {
            if let Err(error) = self.save() {
                self.entries.insert(path.into(), entry);
                return Err(error);
            }
        }
        Ok(())
    }

    fn retire_protection(&mut self, path: &Path) -> Result<()> {
        let Some(entry) = self.entries.get(path).cloned() else {
            return Ok(());
        };
        if path.file_name().and_then(|s| s.to_str()) != Some("memory.low") {
            return Err("cannot retire a non-protection control".into());
        }
        let meta = match fs::metadata(path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return self.forget_protection(path);
            }
            Err(error) => return Err(error.into()),
        };
        if entry.dev != meta.dev() || entry.ino != meta.ino() {
            return self.forget_protection(path);
        }
        let current = read(path)?;
        if owned_control_value(
            path,
            &entry.last,
            entry.pending_previous.as_deref(),
            current.trim(),
            control_page_size()?,
        )? {
            // Use the existing write-ahead journal for restoration as well.
            // A failed write/read-back leaves a recoverable ownership record.
            self.set(path, &entry.original, false)?;
        }
        // An external replacement is preserved; ownership is relinquished.
        self.forget_protection(path)
    }
}

fn can_manage_protection(
    entry: Option<&Control>,
    path: &Path,
    current: &str,
    dev: u64,
    ino: u64,
    page_size: u64,
) -> Result<bool> {
    if let Some(entry) = entry.filter(|entry| entry.dev == dev && entry.ino == ino) {
        if !owned_control_value(
            path,
            &entry.last,
            entry.pending_previous.as_deref(),
            current,
            page_size,
        )? && !owned_protection_default_reset(path, &entry.original, current)
        {
            return Err(format!("external control change preserved: {}", path.display()).into());
        }
        return Ok(true);
    }
    Ok(current == "0")
}
fn stale_control(entry: &Control, metadata: std::io::Result<fs::Metadata>) -> bool {
    match metadata {
        Ok(meta) => entry.dev != meta.dev() || entry.ino != meta.ino(),
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}
fn valid_control(path: &Path) -> bool {
    let raw = path.to_string_lossy();
    if raw.contains(['\t', '\n', '\r'])
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return false;
    }
    raw == "/proc/sys/vm/swappiness"
        || raw == "/proc/sys/vm/page-cluster"
        || matches!(
            raw.as_ref(),
            "/sys/kernel/mm/ksm/run"
                | "/sys/kernel/mm/ksm/pages_to_scan"
                | "/sys/kernel/mm/ksm/sleep_millisecs"
        )
        || (path.starts_with(CGROOT)
            && matches!(
                path.file_name().and_then(|s| s.to_str()),
                Some("memory.low" | "memory.high")
            ))
}
fn managed_background_high_reset(path: &Path, original: &str, observed: &str) -> bool {
    if original != "max" || observed != "max" {
        return false;
    }
    is_managed_background_control(path)
}
fn owned_protection_default_reset(path: &Path, original: &str, observed: &str) -> bool {
    // systemd reapplies configured defaults on daemon-reload. A previously
    // owned zero baseline is still managed while protect_sessions is enabled;
    // finite nonzero external settings always retain priority.
    original == "0"
        && observed == "0"
        && path.starts_with(CGROOT)
        && path.file_name().and_then(|value| value.to_str()) == Some("memory.low")
}
fn is_managed_background_control(path: &Path) -> bool {
    let Some(relative) = path.strip_prefix(CGROOT).ok().and_then(|p| p.to_str()) else {
        return false;
    };
    const SUFFIX: &str = "zmem.slice/zmem-background.slice/memory.high";
    if relative == SUFFIX {
        return true;
    }
    let Some(user) = relative.strip_prefix("user.slice/user-") else {
        return false;
    };
    let Some((uid, tail)) = user.split_once(".slice/user@") else {
        return false;
    };
    !uid.is_empty()
        && uid.bytes().all(|c| c.is_ascii_digit())
        && tail == format!("{uid}.service/{SUFFIX}")
}
fn control_page_size() -> Result<u64> {
    // libc reports the running kernel's base-page size without spawning a
    // command or guessing 4096 on machines configured for larger pages.
    let size = unsafe { getpagesize() };
    if size <= 0 || !(size as u64).is_power_of_two() {
        return Err("invalid system page size".into());
    }
    Ok(size as u64)
}
fn canonical_control_value(path: &Path, value: &str, page_size: u64) -> Result<String> {
    let memory = path.starts_with(CGROOT)
        && matches!(
            path.file_name().and_then(|s| s.to_str()),
            Some("memory.low" | "memory.high")
        );
    let value = value.trim();
    if memory && value == "max" {
        return Ok("max".into());
    }
    let bytes = value.parse::<u64>()?;
    if path == Path::new(dedup::Knob::Run.path()) && bytes > 1 {
        return Err("KSM run must be 0 or 1; unmerge is never a managed operation".into());
    }
    if memory {
        if page_size == 0 || !page_size.is_power_of_two() {
            return Err("invalid control page size".into());
        }
        // The memory controller stores byte limits as a whole number of pages.
        Ok((bytes / page_size * page_size).to_string())
    } else {
        Ok(bytes.to_string())
    }
}
fn owned_control_value(
    path: &Path,
    last: &str,
    pending_previous: Option<&str>,
    observed: &str,
    page_size: u64,
) -> Result<bool> {
    if observed == canonical_control_value(path, last, page_size)? {
        return Ok(true);
    }
    match pending_previous {
        Some(previous) => Ok(observed == canonical_control_value(path, previous, page_size)?),
        None => Ok(false),
    }
}
fn write_control(path: &Path, value: &str, nonblock: bool) -> Result<()> {
    let value = canonical_control_value(path, value, control_page_size()?)?;
    let mut o = OpenOptions::new();
    o.write(true);
    if nonblock {
        o.custom_flags(O_NONBLOCK);
    }
    o.open(path)?.write_all(value.as_bytes())?;
    Ok(())
}
pub fn restore_controls() -> Result<()> {
    restore_controls_from(Path::new(CONTROL_FILE))
}
fn restore_controls_from(path: &Path) -> Result<()> {
    let text = match read(path) {
        Ok(t) => t,
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    restore_control_snapshot(&text)?;
    fs::remove_file(path)?;
    Ok(())
}
pub fn restore_control_snapshot(text: &str) -> Result<()> {
    let page_size = control_page_size()?;
    let mut errors = Vec::new();
    // Pause only an owned run switch, before restoring any KSM scan knobs.
    // The original state must be paused: zmem never acquires a running KSM.
    // An external run value or identity change preserves all KSM controls.
    let mut restore_ksm = match pause_ksm_for_restore(text, page_size) {
        Ok(owned) => owned,
        Err(error) => {
            errors.push(error.to_string());
            false
        }
    };
    for line in text.lines().rev() {
        let v: Vec<_> = line.split('\t').collect();
        if v.len() != 5 && v.len() != 6 {
            continue;
        }
        let p = Path::new(v[0]);
        if !valid_control(p) {
            continue;
        }
        if p.starts_with(dedup::KSM_ROOT) {
            if !restore_ksm {
                continue;
            }
            match read(dedup::Knob::Run.path()) {
                Ok(value) if value.trim() == "0" => {}
                Ok(_) => {
                    restore_ksm = false;
                    continue;
                }
                Err(error) => {
                    errors.push(error.to_string());
                    restore_ksm = false;
                    continue;
                }
            }
            // Already restored first, before the rate controls.
            if p == Path::new(dedup::Knob::Run.path()) {
                continue;
            }
        }
        let m = match fs::metadata(p) {
            Ok(meta) => meta,
            Err(error)
                if p.starts_with(dedup::KSM_ROOT)
                    && error.kind() != std::io::ErrorKind::NotFound =>
            {
                errors.push(error.to_string());
                continue;
            }
            Err(_) => continue,
        };
        if m.dev().to_string() != v[3] || m.ino().to_string() != v[4] {
            continue;
        }
        let result = (|| -> Result<()> {
            let current = read(p)?;
            let pending = v.get(5).copied().filter(|s| !s.is_empty());
            if owned_control_value(p, v[2], pending, current.trim(), page_size)? {
                let original = canonical_control_value(p, v[1], page_size)?;
                write_control(p, &original, p.ends_with("memory.high"))?;
                if read(p)?.trim() != original {
                    return Err(
                        format!("restored control read-back changed: {}", p.display()).into(),
                    );
                }
            }
            Ok(())
        })();
        if let Err(e) = result {
            errors.push(e.to_string());
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; ").into())
    }
}
fn pause_ksm_for_restore(text: &str, page_size: u64) -> Result<bool> {
    if !text
        .lines()
        .any(|line| line.split('\t').next() == Some(dedup::Knob::Run.path()))
    {
        return Ok(false);
    }
    let snapshot = dedup::read_ksm(Path::new(dedup::KSM_ROOT));
    if snapshot.available == Some(false) || matches!(snapshot.advisor, dedup::Advisor::External(_))
    {
        return Ok(false);
    }
    if !snapshot.complete || matches!(snapshot.advisor, dedup::Advisor::Unreadable) {
        return Err("KSM restoration observation incomplete; retaining recovery journal".into());
    }
    let path = Path::new(dedup::Knob::Run.path());
    for line in text.lines() {
        let values: Vec<_> = line.split('\t').collect();
        if ![5, 6].contains(&values.len()) || values[0] != dedup::Knob::Run.path() {
            continue;
        }
        if values[1] != "0" {
            return Ok(false);
        }
        let meta = match fs::metadata(path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if meta.dev().to_string() != values[3] || meta.ino().to_string() != values[4] {
            return Ok(false);
        }
        let actual = read(path)?;
        let pending = values.get(5).copied().filter(|value| !value.is_empty());
        // A previous restore may have paused successfully and then stopped
        // before restoring the remaining knobs. The original run=0 is an
        // idempotent completed pause; each knob still needs its own ownership
        // check below. Never resume scanning during recovery.
        if actual.trim() != "0"
            && !owned_control_value(path, values[2], pending, actual.trim(), page_size)?
        {
            return Ok(false);
        }
        if actual.trim() == "1" {
            write_control(path, "0", false)?;
        }
        if read(path)?.trim() != "0" {
            return Err("KSM pause readback changed during restoration".into());
        }
        return Ok(true);
    }
    Ok(false)
}
fn protect_sessions(c: &mut Controls, total: u64, errors: &mut Vec<String>) {
    c.protection_complete = false;
    let result = (|| -> Result<protection::ProtectionPlan> {
        let page_size = control_page_size()?;
        let scopes = discover_protection_scopes(
            Path::new("/proc"),
            Path::new(CGROOT),
            total,
            page_size,
            8192,
            Duration::from_millis(500),
        )?;
        protection::plan_protection(Path::new(CGROOT), total, page_size, &scopes)
            .map_err(Into::into)
    })();
    match result {
        Ok(plan) => apply_protection_plan(c, &plan, errors),
        Err(error) => errors.push(format!(
            "protection scan incomplete; previous controls retained: {error}"
        )),
    }
}

fn protection_read(path: &Path, limit: u64) -> std::io::Result<String> {
    let mut text = String::new();
    File::open(path)?
        .take(limit + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("protection input exceeds {limit} bytes: {}", path.display()),
        ));
    }
    Ok(text)
}

fn protection_process_path(root: &Path, cgroup: &str) -> Result<Option<PathBuf>> {
    let mut unified = cgroup.lines().filter_map(|line| line.strip_prefix("0::"));
    let Some(path) = unified.next() else {
        return Ok(None);
    };
    if unified.next().is_some() {
        return Err("multiple unified cgroup memberships".into());
    }
    let relative = path
        .strip_prefix('/')
        .ok_or("nonabsolute unified cgroup membership")?;
    let parts: Vec<_> = relative.split('/').collect();
    // Login sessions are accepted only when the planner also recognizes an
    // explicit Remote role. A shell or terminal alone cannot select the whole
    // login session. The planner validates the UID and session ID before reads.
    if parts.len() == 3
        && parts[0] == "user.slice"
        && parts[1].starts_with("user-")
        && parts[1].ends_with(".slice")
        && parts[2].starts_with("session-")
        && parts[2].ends_with(".scope")
    {
        return Ok(Some(root.join(relative)));
    }
    // Other automatic discovery is restricted to application/session units
    // owned by a user manager. System services are added explicitly below.
    if parts.len() < 5
        || parts[0] != "user.slice"
        || !parts[2].starts_with("user@")
        || !parts[2].ends_with(".service")
        || !matches!(parts[3], "app.slice" | "session.slice")
    {
        return Ok(None);
    }
    Ok(Some(root.join(relative)))
}

fn protection_role(comm: &str, path: &Path) -> Option<protection::Role> {
    use protection::Role;
    let name = comm.trim().to_ascii_lowercase();
    if matches!(
        name.as_str(),
        "hyprland"
            | "kwin_wayland"
            | "kwin_x11"
            | "gnome-shell"
            | "sway"
            | "niri"
            | "weston"
            | "cage"
            | "wayfire"
            | "labwc"
            | "mutter"
            | "cinnamon"
            | "plasmashell"
            | "xfwm4"
    ) {
        return Some(Role::Session);
    }
    if matches!(
        name.as_str(),
        "fcitx" | "fcitx5" | "ibus-daemon" | "ibus-x11" | "ibus-extension-"
    ) || name.starts_with("ibus-engine-")
    {
        return Some(Role::Input);
    }
    if matches!(
        name.as_str(),
        "ztty"
            | "kitty"
            | "kitty.bin"
            | "foot"
            | "footclient"
            | "alacritty"
            | "konsole"
            | "wezterm"
            | "wezterm-gui"
            | "ghostty"
            | "gnome-terminal"
            | "gnome-terminal-"
            | "xfce4-terminal"
            | "lxterminal"
            | "tilix"
            | "xterm"
            | "urxvt"
            | "st"
    ) {
        return Some(Role::Terminal);
    }
    if matches!(
        name.as_str(),
        "ssmcp" | "ssmcp-server" | "007" | "sshd" | "sshd-session" | "sshd-auth"
    ) {
        return Some(Role::Remote);
    }
    // Interpreter names alone do not imply an interactive task. A specifically
    // named remote-control unit can identify Python/Node-hosted remote tools.
    let unit = path.file_name()?.to_str()?.to_ascii_lowercase();
    let unit = unit.strip_prefix("app-").unwrap_or(&unit);
    ["ssmcp", "007"]
        .iter()
        .any(|prefix| {
            unit.strip_prefix(prefix).is_some_and(|tail| {
                tail.starts_with('.') || tail.starts_with('-') || tail.starts_with('@')
            })
        })
        .then_some(Role::Remote)
}

fn discover_protection_scopes(
    proc_root: &Path,
    cgroup_root: &Path,
    total: u64,
    page_size: u64,
    max_processes: usize,
    deadline: Duration,
) -> Result<Vec<protection::Scope>> {
    let start = Instant::now();
    let mut visited = 0;
    let mut selected: BTreeMap<PathBuf, Vec<protection::Role>> = BTreeMap::new();
    for entry in fs::read_dir(proc_root)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
            .is_none()
        {
            continue;
        }
        visited += 1;
        if visited > max_processes || start.elapsed() >= deadline || STOP.load(Ordering::Relaxed) {
            return Err("process discovery exceeded its limit or was interrupted".into());
        }
        let comm = match protection_read(&entry.path().join("comm"), 128) {
            Ok(text) => text,
            Err(error) if process_disappeared(&error) => continue,
            Err(error) => return Err(error.into()),
        };
        let cgroup = match protection_read(&entry.path().join("cgroup"), 16 * 1024) {
            Ok(text) => text,
            Err(error) if process_disappeared(&error) => continue,
            Err(error) => return Err(error.into()),
        };
        let Some(path) = protection_process_path(cgroup_root, &cgroup)? else {
            continue;
        };
        let Some(role) = protection_role(&comm, &path) else {
            continue;
        };
        let candidate = protection::Scope {
            path: path.clone(),
            role,
            current_bytes: page_size,
        };
        // Validate the complete lexical path before reading anything under it.
        // Unsupported units are outside automatic protection, not scan errors.
        if protection::plan_protection(cgroup_root, total, page_size, &[candidate]).is_err() {
            continue;
        }
        let roles = selected.entry(path).or_default();
        if !roles.contains(&role) {
            roles.push(role);
        }
        if selected.len() > 256 {
            return Err("too many critical scopes for one complete protection plan".into());
        }
    }
    for (unit, role) in [
        ("sshd.service", protection::Role::Remote),
        ("ssh.service", protection::Role::Remote),
        ("systemd-logind.service", protection::Role::Session),
    ] {
        selected.insert(cgroup_root.join("system.slice").join(unit), vec![role]);
    }
    let mut scopes = Vec::new();
    for (path, roles) in selected {
        if start.elapsed() >= deadline || STOP.load(Ordering::Relaxed) {
            return Err("scope discovery exceeded its limit or was interrupted".into());
        }
        let current = match protection_read(&path.join("memory.current"), 64) {
            Ok(text) => text.trim().parse::<u64>()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        for role in roles {
            scopes.push(protection::Scope {
                path: path.clone(),
                role,
                current_bytes: current,
            });
        }
    }
    Ok(scopes)
}

fn process_disappeared(error: &std::io::Error) -> bool {
    // A task can exit after its /proc directory was enumerated or opened.
    // procfs may report either ENOENT or ESRCH for that normal race.
    error.kind() == std::io::ErrorKind::NotFound || error.raw_os_error() == Some(3)
}

fn expired_protection_paths(
    entries: &BTreeMap<PathBuf, Control>,
    desired: &BTreeMap<PathBuf, u64>,
    complete: bool,
) -> Vec<PathBuf> {
    if !complete {
        return Vec::new();
    }
    entries
        .keys()
        .filter(|path| {
            path.file_name().and_then(|name| name.to_str()) == Some("memory.low")
                && !desired.contains_key(*path)
        })
        .cloned()
        .collect()
}

fn apply_protection_plan(
    c: &mut Controls,
    plan: &protection::ProtectionPlan,
    errors: &mut Vec<String>,
) {
    let mut complete = true;
    let mut satisfied = BTreeMap::new();
    for (path, bytes) in &plan.limits {
        let ok = match c.protect(path, *bytes) {
            Ok(true) => true,
            Ok(false) => {
                errors.push(format!(
                    "pre-existing protection preserved below requested value: {}",
                    path.display()
                ));
                false
            }
            Err(error) => {
                errors.push(error.to_string());
                false
            }
        };
        satisfied.insert(path.clone(), ok);
        complete &= ok;
    }
    c.protection_budget_bytes = plan.budget_bytes;
    c.protected_critical_scopes = plan
        .leaves
        .iter()
        .filter(|leaf| {
            leaf.limit_bytes > 0
                && leaf
                    .path
                    .ancestors()
                    .take_while(|path| *path != Path::new(CGROOT))
                    .all(|path| {
                        satisfied
                            .get(&path.join("memory.low"))
                            .copied()
                            .unwrap_or(false)
                    })
        })
        .count();
    // A partial discovery never calls this function, and a partial application
    // cannot retire previously established protection either.
    for path in expired_protection_paths(&c.entries, &plan.limits, complete) {
        if let Err(error) = c.retire_protection(&path) {
            errors.push(format!("protection restore incomplete: {error}"));
            complete = false;
        }
    }
    c.protection_complete = complete;
}
fn background_groups() -> Result<Vec<PathBuf>> {
    background::paths()
}
fn capped_high(original: &str, requested: &str) -> Result<String> {
    if original == "max" {
        return Ok(requested.into());
    }
    let original = original.parse::<u64>()?;
    if requested == "max" {
        return Ok(original.to_string());
    }
    Ok(original.min(requested.parse::<u64>()?).to_string())
}
fn swappiness_preference(
    settings: &Settings,
    t: &Telemetry,
    d: &Decision,
) -> swappiness::Preference {
    let resident: u64 = t
        .zram
        .iter()
        .fold(0u64, |sum, z| sum.saturating_add(z.resident));
    // Logical zram capacity is not free RAM. Require measured compression
    // savings, bounded physical residency and usable logical headroom.
    let zram_room = resident < t.sample.total_bytes / 2
        && t.zram.iter().any(|z| {
            z.logical_size > 0
                && z.logical_size.saturating_sub(z.logical_used) > z.logical_size / 10
                && (z.original == 0 || z.original >= z.resident.saturating_add(z.resident / 2))
                && (z.memory_limit == 0 || z.resident < z.memory_limit.saturating_mul(9) / 10)
        });
    swappiness::preference(swappiness::Input {
        rates_available: t.paging.rates_available,
        file_refault: t.paging.file_refault_bytes_per_sec,
        anon_refault: t.paging.anon_refault_bytes_per_sec,
        direct_scan: t.paging.direct_scan_bytes_per_sec,
        memory_full_bp: t.sample.memory_full_bp,
        swap_in: t.sample.swap_in_bytes_per_sec,
        disk_swap_in: t
            .paging
            .disk_swap_estimate_available
            .then_some(t.paging.estimated_disk_swap_in_bytes_per_sec),
        efficient_zram_room: zram_room,
        swap_or_io_busy: d.reason == zmem::Reason::SwapIoPressure,
        hint: d.recommended_swappiness,
        interactive: matches!(settings.profile.as_str(), "interactive" | "gaming"),
    })
}
#[cfg(test)]
fn effective_swappiness(settings: &Settings, t: &Telemetry, d: &Decision) -> u16 {
    swappiness_preference(settings, t, d).value
}

fn apply_swappiness_request(
    actual: Option<u16>,
    preference: swappiness::Preference,
    applied: &mut Applied,
    write: impl FnOnce(u16) -> Result<()>,
) -> Result<()> {
    applied.swappiness_reason = preference.reason;
    // A cached governor value is useful for reporting but is not permission
    // to overwrite a sysctl whose current value could not be observed.
    applied.swappiness_requested = actual.map(|_| preference.value);
    match applied.swappiness_requested {
        Some(value) => write(value),
        None => Ok(()),
    }
}

fn apply_controls(
    settings: &Settings,
    t: &mut Telemetry,
    d: &Decision,
    c: &mut Controls,
    protect: bool,
    now_secs: u64,
    errors: &mut Vec<String>,
) -> Vec<PathBuf> {
    // Terminal scopes and user managers disappear during normal session churn.
    // Keep recovery proportional to live controls rather than daemon uptime.
    let protect = protect || (settings.protect_sessions && c.protection_reset_pending());
    if protect && c.prune_stale() {
        if let Err(e) = c.save() {
            errors.push(e.to_string());
        }
    }
    if protect && settings.protect_sessions {
        let mut protection_errors = Vec::new();
        protect_sessions(c, t.sample.total_bytes, &mut protection_errors);
        c.protection_errors = protection_errors;
        c.protection_sampled_at = Some(now());
        errors.extend(c.protection_errors.iter().cloned());
    }
    t.applied.protection_budget_bytes = c.protection_budget_bytes;
    t.applied.protected_critical_scopes = c.protected_critical_scopes;
    t.applied.protection_complete = c.protection_complete;
    t.applied.protection_errors = c.protection_errors.clone();
    t.applied.protection_sampled_at = c.protection_sampled_at;
    if settings.manage_swappiness {
        let actual = observed_number("/proc/sys/vm/swappiness").and_then(|n| n.try_into().ok());
        let preference =
            c.swappiness_governor
                .observe(swappiness_preference(settings, t, d), actual, now_secs);
        if let Err(e) = apply_swappiness_request(actual, preference, &mut t.applied, |value| {
            c.set(
                Path::new("/proc/sys/vm/swappiness"),
                &value.to_string(),
                false,
            )
        }) {
            errors.push(e.to_string());
        }
        if !t.zram.is_empty() {
            if let Err(e) = c.set(Path::new("/proc/sys/vm/page-cluster"), "0", false) {
                errors.push(e.to_string());
            }
        }
    }
    background::apply(settings, t, d, c, errors)
}

pub fn ensure_zram(settings: &Settings, total: u64) -> Result<()> {
    if !settings.zram_enabled || swaps()?.iter().any(|s| s.path.starts_with("/dev/zram")) {
        return Ok(());
    }
    let lock = SwapMutationLock::acquire()?;
    if swaps()?.iter().any(|s| s.path.starts_with("/dev/zram")) {
        return Ok(());
    }
    command("modprobe", &["zram", "num_devices=1"], 10)?;
    let base = Path::new("/sys/block/zram0");
    if number(base.join("disksize")) != 0 {
        return Err("zram0 is initialized but not active as swap; preserving its contents".into());
    }
    let algos = read(base.join("comp_algorithm"))?;
    if algos
        .split_whitespace()
        .any(|a| a.trim_matches(['[', ']']) == "zstd")
    {
        fs::write(base.join("comp_algorithm"), "zstd")?;
    }
    let size = (total / 100 * settings.zram_percent).min(32 * GIB) / 4096 * 4096;
    fs::write(base.join("disksize"), size.to_string())?;
    command("mkswap", &["/dev/zram0"], 10)?;
    host_swap_command(
        Path::new("/dev/zram0"),
        SwapAction::Activate(100),
        10,
        &lock,
    )?;
    Ok(())
}
fn valid_swap_file(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_file()
        || m.file_type().is_symlink()
        || m.uid() != 0
        || m.mode() & 0o077 != 0
        || m.nlink() != 1
    {
        return Err(format!(
            "swap file must be root-owned private regular file with one link: {}",
            path.display()
        )
        .into());
    }
    Ok(())
}
fn owned_name(p: &Path) -> bool {
    let Some(n) = p.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    n.strip_prefix("zmem-")
        .and_then(|n| n.strip_suffix(".swap"))
        .is_some_and(|n| n.len() == 6 && n.chars().all(|c| c.is_ascii_digit()))
}
fn prepare_swap_dir(settings: &Settings) -> Result<()> {
    secure_dir(Path::new("/var/lib/zmem"), true)?;
    if !settings.swap_dir.exists() {
        let fs_type = command(
            "findmnt",
            &["-n", "-o", "FSTYPE", "--target", "/var/lib/zmem"],
            3,
        )?;
        if fs_type.trim() == "btrfs" {
            command(
                "btrfs",
                &[
                    "subvolume",
                    "create",
                    settings.swap_dir.to_str().ok_or("invalid swap_dir")?,
                ],
                10,
            )?;
        } else {
            fs::create_dir_all(&settings.swap_dir)?;
        }
        fs::set_permissions(&settings.swap_dir, fs::Permissions::from_mode(0o700))?;
    }
    secure_dir(&settings.swap_dir, false)?;
    let fs_type = command(
        "findmnt",
        &[
            "-n",
            "-o",
            "FSTYPE",
            "--target",
            settings.swap_dir.to_str().ok_or("invalid swap_dir")?,
        ],
        3,
    )?;
    if fs_type.trim() == "btrfs" && fs::metadata(&settings.swap_dir)?.ino() != 256 {
        return Err("Btrfs swap_dir must be a separate subvolume; choose a new directory so zmem can create it".into());
    }
    let marker = settings.swap_dir.join(".owner");
    if marker.exists() {
        valid_swap_file(&marker)?;
        if read(&marker)?.trim() != "zmem-swap-v1" {
            return Err("swap directory belongs to another manager".into());
        }
    } else {
        if fs::read_dir(&settings.swap_dir)?.next().is_some() {
            return Err("unowned nonempty swap directory; choose an empty zmem directory".into());
        }
        atomic_write(&marker, "zmem-swap-v1\n", 0o600)?;
    }
    Ok(())
}
pub fn ensure_disk_swaps(settings: &Settings) -> Result<Vec<String>> {
    let lock = SwapMutationLock::acquire()?;
    let mut errors = Vec::new();
    prepare_swap_dir(settings)?;
    let active = swaps()?;
    if let Ok(text) = read("/var/lib/zmem/adopted-swaps") {
        for line in text.lines() {
            let Some((prio, path)) = line.split_once('\t') else {
                continue;
            };
            let p = Path::new(path);
            if p.parent() != Some(Path::new("/var/lib/swapspace"))
                || p.file_name()
                    .is_none_or(|n| !n.to_string_lossy().chars().all(|c| c.is_ascii_digit()))
            {
                errors.push("invalid adopted swap path".into());
                continue;
            }
            if active.iter().any(|s| s.path == path) {
                continue;
            }
            let result = (|| -> Result<()> {
                valid_swap_file(p)?;
                secure_dir(p.parent().unwrap(), false)?;
                let priority = prio.parse::<i32>()?.clamp(-1, 32767);
                activate_existing_swap(p, priority, &lock)?;
                Ok(())
            })();
            if let Err(e) = result {
                errors.push(e.to_string());
            }
        }
    }
    for e in fs::read_dir(&settings.swap_dir)?.flatten() {
        let p = e.path();
        if !owned_name(&p) || active.iter().any(|s| Path::new(&s.path) == p) {
            continue;
        }
        let result = (|| -> Result<()> {
            valid_swap_file(&p)?;
            activate_existing_swap(&p, 10, &lock)?;
            Ok(())
        })();
        if let Err(e) = result {
            errors.push(e.to_string());
        }
    }
    Ok(errors)
}
fn disk_free(path: &Path) -> Result<u64> {
    let out = command(
        "df",
        &[
            "-B1",
            "--output=avail",
            path.to_str().ok_or("invalid disk path")?,
        ],
        5,
    )?;
    out.lines()
        .last()
        .ok_or("df returned no space")?
        .trim()
        .parse()
        .map_err(Into::into)
}

fn owned_swap_allocations(directory: &Path) -> Result<BTreeMap<PathBuf, u64>> {
    let mut owned = BTreeMap::new();
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if !owned_name(&path) {
            continue;
        }
        valid_swap_file(&path)?;
        let meta = fs::symlink_metadata(&path)?;
        // Incomplete allocation and activation attempts still consume the
        // configured budget. Count preallocated blocks as well as file length.
        let bytes = meta.len().max(meta.blocks().saturating_mul(512));
        owned.insert(fs::canonicalize(&path)?, bytes);
    }
    Ok(owned)
}

fn disk_swap_budget(owned: &BTreeMap<PathBuf, u64>, active: &[Swap]) -> u64 {
    let allocated = owned
        .values()
        .fold(0u64, |sum, bytes| sum.saturating_add(*bytes));
    active
        .iter()
        .filter(|swap| swap.kind == "file" || !swap.path.starts_with("/dev/zram"))
        .fold(allocated, |sum, swap| {
            // Only the complete canonical name is a match. Orphan mount names
            // stay separate; a basename or size match does not prove identity.
            let counted = owned.get(Path::new(&swap.path)).copied().unwrap_or(0);
            sum.saturating_add(swap.size.saturating_sub(counted))
        })
}

fn disk_swap_growth_bytes(requested: u64, chunk: u64, limit: u64, allocated: u64) -> u64 {
    requested.min(chunk).min(limit.saturating_sub(allocated)) / 4096 * 4096
}

fn can_remove_failed_swap(
    path: &Path,
    expected: SwapIdentity,
    observed: Option<SwapIdentity>,
    errno: Option<i32>,
    active: &[Swap],
) -> bool {
    errno.is_some_and(|errno| (1..=4095).contains(&errno) && errno != 16)
        && observed == Some(expected)
        && !active.iter().any(|swap| Path::new(&swap.path) == path)
}

pub fn grow_swap(settings: &Settings, requested: u64) -> Result<()> {
    root_required()?;
    let lock = SwapMutationLock::acquire()?;
    prepare_swap_dir(settings)?;
    let active = swaps()?;
    let owned = owned_swap_allocations(&settings.swap_dir)?;
    let disk = disk_swap_budget(&owned, &active);
    let size = disk_swap_growth_bytes(
        requested,
        settings.policy.chunk_bytes,
        settings.policy.max_disk_swap_bytes,
        disk,
    );
    if size < 64 * MIB {
        return Err("disk swap capacity limit reached".into());
    }
    if disk_free(&settings.swap_dir)? < settings.policy.disk_reserve_bytes.saturating_add(size) {
        return Err("disk reserve prevents swap growth".into());
    }
    let path = (0..1_000_000u32)
        .map(|i| settings.swap_dir.join(format!("zmem-{i:06}.swap")))
        .find(|p| !p.exists())
        .ok_or("no swap slot available")?;
    let name = path.to_str().ok_or("invalid swap file path")?;
    let size_string = size.to_string();
    let fs_type = command(
        "findmnt",
        &[
            "-n",
            "-o",
            "FSTYPE",
            "--target",
            settings.swap_dir.to_str().ok_or("invalid swap directory")?,
        ],
        5,
    )?;
    let mut activation_file = None;
    let result = (|| -> Result<()> {
        if fs_type.trim() == "btrfs" {
            command(
                "btrfs",
                &["filesystem", "mkswapfile", "--size", &size_string, name],
                100,
            )?;
        } else {
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&path)?;
            command("fallocate", &["-l", &size_string, name], 100)?;
            command("mkswap", &[name], 10)?;
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        valid_swap_file(&path)?;
        activation_file = Some(swap_target(&path)?);
        host_swap_pinned(
            &path,
            SwapAction::Activate(10),
            10,
            &lock,
            activation_file
                .as_ref()
                .ok_or("missing activation identity")?,
        )?;
        Ok(())
    })();
    if let (Err(error), Some(pinned)) = (&result, activation_file.as_ref()) {
        // Keep the pinned inode alive through this check. A timeout or an
        // unreadable inventory is not evidence that activation failed. Such
        // files remain on disk and are charged by owned_swap_allocations.
        if let (Ok(expected), Ok(current), Ok(active)) = (
            pinned.metadata(),
            swap_target(&path).and_then(|file| Ok(file.metadata()?)),
            swaps(),
        ) {
            if can_remove_failed_swap(
                &path,
                SwapIdentity::of(&expected),
                Some(SwapIdentity::of(&current)),
                swap_syscall_errno(error.as_ref()),
                &active,
            ) {
                let _ = fs::remove_file(&path);
            }
        }
    }
    result
}
fn validate_owned_swap_path(settings: &Settings, path: &Path) -> Result<()> {
    if path.parent() != Some(settings.swap_dir.as_path()) || !owned_name(path) {
        return Err("refusing to remove an unowned swap file".into());
    }
    Ok(())
}

fn log_swap_deferred(path: &Path, reason: &str, errno: Option<i32>) {
    println!(
        "{{\"event\":\"swap_maintenance_deferred\",\"path\":{},\"reason\":{},\"swapoff_errno\":{}}}",
        json::quote(&path.to_string_lossy()),
        json::quote(reason),
        errno.map_or_else(|| "null".into(), |n| n.to_string()),
    );
}

fn unlink_owned_swap(
    path: &Path,
    pinned: &File,
    lock: &SwapMutationLock,
    errno: Option<i32>,
    mode: swap_lifecycle::Mode,
) -> Result<()> {
    let meta = pinned.metadata()?;
    let identity = SwapIdentity::of(&meta);
    valid_swap_file(path)?;
    verify_swap_identity(&swap_target(path)?, identity)?;
    if swaps()?.iter().any(|s| Path::new(&s.path) == path) {
        log_swap_deferred(path, "still_listed", errno);
        return Ok(());
    }
    SwapMutationLock::validate(&lock.0)?;
    // Linux vfs_unlink rechecks IS_SWAPFILE under inode_lock and returns EPERM
    // for active/draining swaps; swapon holds that lock and rejects unlinked
    // dentries. Ordinary unlink is the final atomic gate, never a truncation,
    // hole-punch, reflink, rename-to-trash or overwrite of swap contents.
    // In cleanup mode no swapoff is issued and no pages are migrated. An orphan
    // active file is therefore preserved by the kernel, regardless of its name.
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if matches!(error.raw_os_error(), Some(1 | 16)) => {
            log_swap_deferred(path, "kernel_preserved_active_or_busy_file", errno);
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    }
    println!(
        "{{\"event\":\"swap_file_removed\",\"mode\":{},\"path\":{},\"file_bytes\":{},\"allocated_bytes\":{},\"swapoff_errno\":{},\"inactivity_verified_by\":\"kernel_guarded_unlink\"}}",
        json::quote(mode.as_str()),
        json::quote(&path.to_string_lossy()),
        meta.len(),
        meta.blocks().saturating_mul(512),
        errno.map_or_else(|| "null".into(), |n| n.to_string()),
    );
    Ok(())
}

fn retire_owned_swap(
    settings: &Settings,
    path: &Path,
    expected: Option<SwapIdentity>,
    lock: &SwapMutationLock,
) -> Result<()> {
    validate_owned_swap_path(settings, path)?;
    valid_swap_file(path)?;
    // Hold this inode through the syscall, fresh inventory and final unlink;
    // reject observed replacements under the shared mutation lock.
    let pinned = swap_target(path)?;
    if let Some(expected) = expected {
        verify_swap_identity(&pinned, expected)?;
    }
    let meta = pinned.metadata()?;
    let bytes = meta.len().max(meta.blocks().saturating_mul(512));
    let t = Sampler::new().sample()?;
    let reserve = settings
        .policy
        .effective_reserve_bytes(t.sample.total_bytes);
    if t.swaps
        .iter()
        .any(|swap| Path::new(&swap.path) == path && swap.used != 0)
    {
        log_swap_deferred(path, "in_use", None);
        return Ok(());
    }
    if let Some(reason) =
        swap_lifecycle::defer_reason(t.sample, reserve, settings.policy.grow_free_bytes, bytes)
    {
        log_swap_deferred(path, reason, None);
        return Ok(());
    }
    let result = host_swap_pinned(path, SwapAction::Deactivate, 20, lock, &pinned);
    let errno = match &result {
        Ok(()) => Some(0),
        Err(error) => swap_syscall_errno(error.as_ref()),
    };
    if !swap_lifecycle::cleanup_may_be_attempted(errno) {
        log_swap_deferred(path, "swapoff_unconfirmed_or_failed", errno);
        return result;
    }
    // EINVAL is not evidence of inactivity: a concurrent swapoff may have
    // removed its entry from swap_active_head while still draining pages.
    unlink_owned_swap(path, &pinned, lock, errno, swap_lifecycle::Mode::Shrink)
}

pub fn shrink_swap(settings: &Settings, path: &Path) -> Result<()> {
    root_required()?;
    validate_owned_swap_path(settings, path)?;
    let lock = SwapMutationLock::acquire()?;
    prepare_swap_dir(settings)?;
    retire_owned_swap(settings, path, None, &lock)
}

pub fn maintain_swap(settings: &Settings, round: u64, mode: &str) -> Result<()> {
    root_required()?;
    let mode = swap_lifecycle::Mode::parse(mode).ok_or("invalid swap maintenance mode")?;
    let lock = SwapMutationLock::acquire()?;
    prepare_swap_dir(settings)?;
    let t = Sampler::new().sample()?;
    let reserve = settings
        .policy
        .effective_reserve_bytes(t.sample.total_bytes);
    if let Some(reason) =
        swap_lifecycle::cleanup_defer_reason(t.sample, reserve, settings.policy.critical_full_bp)
    {
        println!(
            "{{\"event\":\"swap_maintenance_deferred\",\"reason\":{}}}",
            json::quote(reason)
        );
        return Ok(());
    }
    let mut candidates = Vec::new();
    for (index, entry) in fs::read_dir(&settings.swap_dir)?.enumerate() {
        if index >= swap_lifecycle::SCAN_LIMIT {
            return Err(
                "owned swap directory exceeds maintenance scan limit; files preserved".into(),
            );
        }
        let path = entry?.path();
        if !owned_name(&path) {
            continue;
        }
        valid_swap_file(&path)?;
        let meta = fs::symlink_metadata(&path)?;
        let bytes = meta.len().max(meta.blocks().saturating_mul(512));
        let active = t.swaps.iter().find(|swap| Path::new(&swap.path) == path);
        let listed = active.is_some();
        match mode {
            swap_lifecycle::Mode::Cleanup if listed => continue,
            swap_lifecycle::Mode::Shrink
                if active.is_some_and(|swap| swap.used != 0)
                    || swap_lifecycle::defer_reason(
                        t.sample,
                        reserve,
                        settings.policy.grow_free_bytes,
                        bytes,
                    )
                    .is_some() =>
            {
                continue;
            }
            _ => {}
        }
        candidates.push((listed, path, SwapIdentity::of(&meta)));
    }
    // Unknown/incomplete files participate even when /proc/swaps contains only
    // an old namespace alias. Never infer identity from a basename or size.
    // Rotate after every attempt so one deferred file cannot starve the rest.
    candidates.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    let Some(index) = swap_lifecycle::candidate_index(round, candidates.len()) else {
        println!("{{\"event\":\"swap_maintenance_idle\",\"eligible_files\":0}}");
        return Ok(());
    };
    let (_, path, identity) = &candidates[index];
    match mode {
        swap_lifecycle::Mode::Shrink => retire_owned_swap(settings, path, Some(*identity), &lock),
        swap_lifecycle::Mode::Cleanup => {
            let pinned = swap_target(path)?;
            verify_swap_identity(&pinned, *identity)?;
            unlink_owned_swap(path, &pinned, &lock, None, mode)
        }
    }
}
pub fn reclaim(path: &Path, bytes: u64) -> Result<()> {
    root_required()?;
    if !background_groups()?.iter().any(|p| p == path) {
        return Err("reclaim is limited to registered background groups".into());
    }
    let before = reclaim_sample(path)?;
    let expected = expected_reclaim_identity()?;
    if expected.is_some_and(|identity| identity != (before.dev, before.ino)) {
        println!("{{\"event\":\"reclaim_skipped\",\"reason\":\"cgroup_identity_changed\"}}");
        return Ok(());
    }
    // Revalidate population and the clean-file budget in the child. The
    // controller's earlier measurement may have changed before spawn.
    let mut validation = reclaim_policy::Tracker::new();
    let request = match validation.plan(before, bytes, control_page_size()?, 0) {
        Ok(request) => request,
        Err(reason) => {
            println!(
                "{{\"event\":\"reclaim_skipped\",\"reason\":{}}}",
                json::quote(reason.as_str())
            );
            return Ok(());
        }
    };
    let mut control = OpenOptions::new()
        .write(true)
        .open(path.join("memory.reclaim"))?;
    let identity = fs::metadata(path)?;
    if (identity.dev(), identity.ino()) != (request.before.dev, request.before.ino) {
        println!("{{\"event\":\"reclaim_skipped\",\"reason\":\"cgroup_identity_changed\"}}");
        return Ok(());
    }
    // This write can perform synchronous kernel reclaim. It remains confined
    // to this isolated, deadline-controlled worker, never the controller loop.
    let partial = match control.write_all(format!("{} swappiness=0", request.bytes).as_bytes()) {
        Ok(()) => false,
        Err(error) if error.raw_os_error() == Some(11) => true,
        Err(error) => return Err(error.into()),
    };
    println!(
        "{{\"event\":\"reclaim_write\",\"requested_bytes\":{},\"kernel_partial\":{partial}}}",
        request.bytes,
    );
    Ok(())
}

fn parse_reclaim_identity(dev: Option<&str>, ino: Option<&str>) -> Result<Option<(u64, u64)>> {
    match (dev, ino) {
        (None, None) => Ok(None),
        (Some(dev), Some(ino)) => {
            let identity = (dev.parse()?, ino.parse()?);
            if identity.1 == 0 {
                return Err("invalid expected reclaim cgroup inode".into());
            }
            Ok(Some(identity))
        }
        _ => Err("incomplete expected reclaim cgroup identity".into()),
    }
}

fn expected_reclaim_identity() -> Result<Option<(u64, u64)>> {
    let get = |name: &str| -> Result<Option<String>> {
        match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(error) => Err(error.into()),
        }
    };
    let dev = get("ZMEM_RECLAIM_DEV")?;
    let ino = get("ZMEM_RECLAIM_INO")?;
    parse_reclaim_identity(dev.as_deref(), ino.as_deref())
}

#[derive(Clone, Debug)]
struct ReclaimTarget {
    path: PathBuf,
    request: reclaim_policy::Request,
}

#[derive(Default)]
struct ReclaimSelection {
    first: Option<ReclaimTarget>,
    eligible: usize,
}

fn select_reclaim_candidate(
    tracker: &mut reclaim_policy::Tracker,
    candidates: impl IntoIterator<Item = (PathBuf, reclaim_policy::Sample)>,
    requested_bytes: u64,
    page_size: u64,
    now_secs: u64,
) -> ReclaimSelection {
    let mut selection = ReclaimSelection::default();
    if requested_bytes == 0 {
        return selection;
    }
    for (path, sample) in candidates {
        if let Ok(request) = tracker.plan(sample, requested_bytes, page_size, now_secs) {
            selection.eligible += 1;
            if selection.first.is_none() {
                selection.first = Some(ReclaimTarget { path, request });
            }
        }
    }
    selection
}

fn finish_pending_reclaim(
    pending: &mut Option<ReclaimTarget>,
    tracker: &mut reclaim_policy::Tracker,
    after: Option<reclaim_policy::Sample>,
    now_secs: u64,
) -> Option<(ReclaimTarget, reclaim_policy::Outcome)> {
    let target = pending.take()?;
    let outcome = tracker.finish(target.request, after, now_secs);
    Some((target, outcome))
}

fn log_reclaim_feedback(
    target: &ReclaimTarget,
    outcome: &reclaim_policy::Outcome,
    after: Option<reclaim_policy::Sample>,
    phase: &str,
    worker_pid: Option<u32>,
) {
    println!(
        "{{\"event\":\"reclaim_feedback\",\"path\":{},\"phase\":{},\"worker_pid\":{},\"requested_bytes\":{},\"before_bytes\":{},\"after_bytes\":{},\"progress\":{},\"made_progress\":{},\"reclaimed_pages\":{},\"reclaimed_bytes\":{},\"net_freed_bytes\":{},\"retry_at_uptime_secs\":{}}}",
        json::quote(&target.path.to_string_lossy()),
        json::quote(phase),
        optional_number(worker_pid.map(u64::from)),
        target.request.bytes,
        target.request.before.current_bytes,
        optional_number(after.filter(|s| (s.dev, s.ino) == (target.request.before.dev, target.request.before.ino)).map(|s| s.current_bytes)),
        json::quote(outcome.progress.as_str()),
        outcome.progress.made_progress(),
        optional_number(outcome.reclaimed_pages),
        optional_number(outcome.reclaimed_bytes),
        outcome.net_freed_bytes,
        outcome.retry_after_secs,
    );
}

struct Worker {
    child: Child,
    kind: String,
    start: Instant,
    termination_sent: bool,
    reclaim: Option<ReclaimTarget>,
}
impl Worker {
    fn reclaim_feedback(
        &mut self,
        tracker: &mut reclaim_policy::Tracker,
        now_secs: u64,
        phase: &str,
    ) {
        let after = self
            .reclaim
            .as_ref()
            .and_then(|target| reclaim_sample(&target.path).ok());
        if let Some((target, outcome)) =
            finish_pending_reclaim(&mut self.reclaim, tracker, after, now_secs)
        {
            log_reclaim_feedback(&target, &outcome, after, phase, Some(self.child.id()));
        }
    }
    fn deadline(&self) -> Duration {
        Duration::from_secs(if self.kind == "reclaim" { 5 } else { 125 })
    }
    fn terminate(&mut self) {
        if !self.termination_sent {
            // Every worker is the leader of its own freshly-created process
            // group. Its system-command children inherit that group, so they
            // cannot continue a swap mutation after their worker is stopped.
            unsafe {
                kill(-(self.child.id() as i32), 9);
            }
            self.termination_sent = true;
        }
    }
    fn finish(&mut self, complete: bool) -> Result<()> {
        if !complete {
            self.terminate();
        }
        let mut cleanup_started = None;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    return if complete && !status.success() {
                        Err(format!("{} worker failed ({status})", self.kind).into())
                    } else {
                        Ok(())
                    };
                }
                Ok(None) => {}
                Err(error) => {
                    self.terminate();
                    return Err(error.into());
                }
            }
            if STOP.load(Ordering::Relaxed) || self.start.elapsed() > self.deadline() {
                self.terminate();
            }
            if self.termination_sent {
                let since = cleanup_started.get_or_insert_with(Instant::now);
                // A process stuck in an uninterruptible kernel operation may
                // retain a pending SIGKILL. Do not block controller shutdown
                // on it; the entire isolated group has already been signalled.
                if since.elapsed() > Duration::from_secs(1) {
                    return if complete {
                        Err(format!(
                            "{} worker did not complete before cancellation/deadline",
                            self.kind
                        )
                        .into())
                    } else {
                        Ok(())
                    };
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}
fn worker_start(
    kind: &str,
    args: &[String],
    config: &Path,
    reclaim: Option<&ReclaimTarget>,
) -> Result<Worker> {
    if (kind == "reclaim") != reclaim.is_some() {
        return Err("reclaim workers require a measured request".into());
    }
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.process_group(0).arg(format!("__{kind}")).args(args);
    if let Some(target) = reclaim {
        cmd.env("ZMEM_RECLAIM_DEV", target.request.before.dev.to_string());
        cmd.env("ZMEM_RECLAIM_INO", target.request.before.ino.to_string());
    }
    if config.exists() {
        cmd.arg("--config").arg(config);
    }
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    Ok(Worker {
        child,
        kind: kind.into(),
        start: Instant::now(),
        termination_sent: false,
        reclaim: reclaim.cloned(),
    })
}
pub fn daemon(settings: Settings, config: PathBuf, observe: bool, once: bool) -> Result<()> {
    let _lock = if observe {
        None
    } else {
        Some(InstanceLock::acquire()?)
    };
    if !observe {
        restore_controls()?;
    }
    // POSIX calls only register a signal-safe handler. No process-wide mutable
    // state is accessed by the handler except the atomic STOP flag.
    unsafe {
        signal(15, stop_signal as *const () as usize);
        signal(2, stop_signal as *const () as usize);
    }
    let start = Instant::now();
    let mut sampler = Sampler::new();
    let mut policy = Policy::new(settings.policy);
    let mut controls = Controls::new();
    let mut dedup_state = dedup_adapter::State::new(settings.interval_ms);
    let mut reclaim_tracker = reclaim_policy::Tracker::new();
    let mut swap_maintenance = swap_lifecycle::Schedule::default();
    let reclaim_page_size = control_page_size()?;
    let mut worker: Option<Worker> = None;
    let mut loop_index = 0u64;
    let mut initial_errors = Vec::new();
    if !observe {
        let initial = sampler.sample()?;
        if let Err(e) = ensure_zram(&settings, initial.sample.total_bytes) {
            initial_errors.push(e.to_string());
        }
        match ensure_disk_swaps(&settings) {
            Ok(v) => initial_errors.extend(v),
            Err(e) => initial_errors.push(e.to_string()),
        };
    }
    let mut result = (|| -> Result<()> {
        loop {
            if STOP.load(Ordering::Relaxed) {
                break;
            }
            let cycle = Instant::now();
            let mut errors = std::mem::take(&mut initial_errors);
            if let Some(w) = worker.as_mut() {
                if let Some(status) = w.child.try_wait()? {
                    if !status.success() {
                        errors.push(format!("{} worker failed ({status})", w.kind));
                    }
                    println!(
                        "{{\"event\":\"worker_finished\",\"action\":{},\"exit_success\":{}}}",
                        json::quote(&w.kind),
                        status.success()
                    );
                    w.reclaim_feedback(
                        &mut reclaim_tracker,
                        start.elapsed().as_secs(),
                        if status.success() { "exited" } else { "failed" },
                    );
                    worker = None;
                } else if !w.termination_sent && w.start.elapsed() > w.deadline() {
                    w.terminate();
                    w.reclaim_feedback(&mut reclaim_tracker, start.elapsed().as_secs(), "deadline");
                    errors.push(format!("{} worker exceeded its deadline", w.kind));
                }
            }
            let mut t = match sampler.sample() {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("zmem sample failed: {e}");
                    if !observe {
                        if let Err(error) = dedup_state.pause_on_sample_error(&mut controls) {
                            eprintln!("cannot pause owned KSM after failed sample: {error}");
                        }
                    }
                    if once {
                        return Err(e);
                    }
                    thread::sleep(Duration::from_millis(settings.interval_ms));
                    continue;
                }
            };
            let elapsed = start.elapsed().as_secs();
            errors.extend(t.background.errors.iter().cloned());
            let d = policy.observe(t.sample, elapsed);
            let bg = if !observe {
                apply_controls(
                    &settings,
                    &mut t,
                    &d,
                    &mut controls,
                    loop_index % 15 == 0,
                    elapsed,
                    &mut errors,
                )
            } else {
                background::preview(&settings, &mut t, &d, &mut controls.background)
            };
            dedup_state.update(
                &settings,
                &mut t,
                &d,
                &mut controls,
                start.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
                observe,
                &mut errors,
            );
            let selection = select_reclaim_candidate(
                &mut reclaim_tracker,
                bg.into_iter()
                    .filter_map(|path| match reclaim_sample(&path) {
                        Ok(sample) => Some((path, sample)),
                        Err(error) => {
                            errors.push(format!("reclaim candidate {}: {error}", path.display()));
                            None
                        }
                    }),
                d.reclaim_bytes,
                reclaim_page_size,
                elapsed,
            );
            // Count candidates that pass this cycle's global reclaim request,
            // eligibility filters, and feedback backoff, before worker/priority
            // arbitration. With no global reclaim request this is zero.
            t.managed.reclaim_candidates = selection.eligible;
            if !observe && worker.is_none() {
                let maintenance_mode = if d.shrink_allowed {
                    Some(swap_lifecycle::Mode::Shrink)
                } else if d.pressure != zmem::Pressure::Critical
                    && !d.io_congested
                    && swap_lifecycle::cleanup_defer_reason(
                        t.sample,
                        settings
                            .policy
                            .effective_reserve_bytes(t.sample.total_bytes),
                        settings.policy.critical_full_bp,
                    )
                    .is_none()
                {
                    Some(swap_lifecycle::Mode::Cleanup)
                } else {
                    None
                };
                let action = if d.grow_swap_bytes > 0 {
                    Some(("grow", vec![d.grow_swap_bytes.to_string()], None))
                } else if let Some(target) = selection.first {
                    Some((
                        "reclaim",
                        vec![
                            target.path.to_string_lossy().into(),
                            target.request.bytes.to_string(),
                        ],
                        Some(target),
                    ))
                } else {
                    swap_maintenance
                        .next(maintenance_mode, elapsed)
                        .map(|(mode, round)| {
                            (
                                "maintain",
                                vec![round.to_string(), mode.as_str().into()],
                                None,
                            )
                        })
                };
                if let Some((kind, args, mut pending_reclaim)) = action {
                    match worker_start(kind, &args, &config, pending_reclaim.as_ref()) {
                        Ok(w) => worker = Some(w),
                        Err(error) => {
                            errors.push(error.to_string());
                            let after =
                                pending_reclaim.as_ref().map(|target| target.request.before);
                            if let Some((target, outcome)) = finish_pending_reclaim(
                                &mut pending_reclaim,
                                &mut reclaim_tracker,
                                after,
                                elapsed,
                            ) {
                                log_reclaim_feedback(
                                    &target,
                                    &outcome,
                                    after,
                                    "spawn_failed",
                                    None,
                                );
                            }
                        }
                    }
                }
            }
            let status = telemetry_json(
                &t,
                &d,
                &settings.profile,
                worker.as_ref().map(|w| w.kind.as_str()),
                &errors,
                false,
            );
            if observe || once {
                println!("{status}");
            } else {
                atomic_write(
                    &Path::new(RUNTIME).join("status.json"),
                    &(status + "\n"),
                    0o644,
                )?;
                if !errors.is_empty() {
                    eprintln!(
                        "{{\"event\":\"diagnostic\",\"errors\":{}}}",
                        json::strings(&errors)
                    );
                }
            }
            loop_index += 1;
            if once {
                break;
            }
            while !STOP.load(Ordering::Relaxed)
                && cycle.elapsed() < Duration::from_millis(settings.interval_ms)
            {
                thread::sleep(Duration::from_millis(100).min(
                    Duration::from_millis(settings.interval_ms).saturating_sub(cycle.elapsed()),
                ));
            }
        }
        Ok(())
    })();
    if let Some(mut w) = worker {
        let finished = w.finish(once);
        w.reclaim_feedback(
            &mut reclaim_tracker,
            start.elapsed().as_secs(),
            if once { "once_finished" } else { "shutdown" },
        );
        if result.is_ok() {
            result = finished;
        }
    }
    if !observe {
        let restored = restore_controls();
        if result.is_ok() {
            restored?;
        }
    }
    result
}

pub fn doctor_json() -> (String, bool) {
    let checks = [
        ("linux_proc", Path::new("/proc/meminfo").exists()),
        ("memory_psi", Path::new("/proc/pressure/memory").exists()),
        ("io_psi", Path::new("/proc/pressure/io").exists()),
        (
            "cgroup_v2",
            Path::new("/sys/fs/cgroup/cgroup.controllers").exists(),
        ),
        (
            "memory_controller",
            read("/sys/fs/cgroup/cgroup.controllers")
                .is_ok_and(|s| s.split_whitespace().any(|s| s == "memory")),
        ),
        ("systemd", Path::new("/run/systemd/system").exists()),
    ];
    let ok = checks.iter().all(|(_, v)| *v);
    let values = checks
        .iter()
        .map(|(k, v)| format!("{}:{v}", json::quote(k)))
        .collect::<Vec<_>>()
        .join(",");
    (
        format!(
            "{{\"schema\":\"zmem.doctor.v1\",\"ok\":{ok},\"checks\":{{{values}}},\"daemon_active\":{}}}",
            command("systemctl", &["is-active", "--quiet", "zmem.service"], 3).is_ok()
        ),
        ok,
    )
}

fn systemd_run_version(text: &str) -> Result<u32> {
    if text.len() > 16 * 1024 {
        return Err("systemd-run version response is too large".into());
    }
    let mut words = text.lines().next().unwrap_or_default().split_whitespace();
    if words.next() != Some("systemd") {
        return Err("cannot identify systemd-run version".into());
    }
    let version = words.next().ok_or("missing systemd-run version")?;
    if version
        .split('.')
        .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err("invalid systemd-run version".into());
    }
    let major = version
        .split('.')
        .next()
        .ok_or("invalid systemd-run version")?;
    Ok(major.parse()?)
}

fn workload_command(
    class: &str,
    maximum_mib: Option<u64>,
    dedup: bool,
    args: &[std::ffi::OsString],
    version: u32,
) -> Result<Command> {
    let marker = if dedup { "-dedup" } else { "" };
    let id = format!("zmem-{}{}-{}", class, marker, std::process::id());
    let mut cmd = Command::new("/usr/bin/systemd-run");
    if root_required().is_err() {
        cmd.arg("--user");
    }
    cmd.args(["--scope", "--quiet", "--collect", "--unit", &id]);
    // The option was introduced in 254; newer versions may expand by default.
    // Earlier versions preserve scope arguments and do not accept this flag.
    if version >= 254 {
        cmd.arg("--expand-environment=no");
    }
    cmd.arg(format!("--description=zmem {class} workload"));
    match class {
        "interactive" => {
            cmd.arg("--slice=app.slice")
                .arg("--property=MemoryLow=256M");
        }
        "background" => {
            cmd.arg("--slice=zmem-background.slice")
                .arg("--property=MemoryAccounting=yes")
                .arg("--property=CPUWeight=50")
                .arg("--property=IOWeight=50");
        }
        _ => {
            let max = maximum_mib.ok_or("disposable memory limit was not validated")?;
            cmd.arg("--slice=zmem-background.slice")
                .arg(format!("--property=MemoryMax={}M", max))
                .arg(format!(
                    "--property=MemoryHigh={}M",
                    max.saturating_mul(3) / 4
                ))
                .arg(format!("--property=MemorySwapMax={}M", max))
                .arg("--property=OOMPolicy=kill");
        }
    }
    cmd.arg("--");
    if dedup {
        cmd.arg(std::env::current_exe()?)
            .args(["__dedup-exec", "--"]);
    }
    cmd.args(args);
    Ok(cmd)
}

fn with_background_admission<T>(
    class: &str,
    acquire: impl FnOnce() -> Result<T>,
) -> Result<Option<T>> {
    if class == "interactive" {
        Ok(None)
    } else {
        acquire().map(Some)
    }
}

pub fn run_workload(
    settings: &Settings,
    class: &str,
    max_mib: Option<u64>,
    dedup: bool,
    timeout_secs: u64,
    args: &[std::ffi::OsString],
) -> Result<()> {
    if args.is_empty() {
        return Err("run requires a command after --".into());
    }
    if !["background", "interactive", "disposable"].contains(&class) {
        return Err("class must be background, interactive or disposable".into());
    }
    if max_mib.is_some() && class != "disposable" {
        return Err("--max-memory applies to explicit disposable workloads".into());
    }
    if dedup && class == "interactive" {
        return Err("--dedup is available only for trusted background/disposable workloads".into());
    }
    if timeout_secs > crate::admission::MAX_WAIT_TIMEOUT_SECS {
        return Err("--wait-timeout must be 0..3600 seconds".into());
    }
    if max_mib.is_some_and(|max| max < 16) {
        return Err("disposable --max-memory must be at least 16 MiB".into());
    }
    // Probe only the system launcher, never COMMAND. No memory or runtime
    // coordination files are inspected on the interactive bypass.
    let version = command("/usr/bin/systemd-run", &["--version"], 2)
        .and_then(|output| systemd_run_version(&output))
        .map_err(|error| {
            crate::admission::Failure::state(format!("cannot prepare scope launcher: {error}"))
        })?;
    let mut permit = with_background_admission(class, || {
        crate::admission::acquire(settings.policy, timeout_secs, |sample| {
            if class == "disposable" {
                let maximum = max_mib.unwrap_or((sample.total_bytes / 4 / MIB).max(128));
                if maximum < 16 || maximum > sample.total_bytes / MIB {
                    return Err("disposable --max-memory must be 16 MiB..physical RAM".into());
                }
            }
            Ok(())
        })
        .map_err(Into::into)
    })?;
    let maximum = if class == "disposable" {
        let total = permit
            .as_ref()
            .ok_or("missing disposable admission")?
            .sample
            .total_bytes;
        Some(max_mib.unwrap_or((total / 4 / MIB).max(128)))
    } else {
        None
    };
    let mut cmd = workload_command(class, maximum, dedup, args, version)?;
    if let Some(permit) = &mut permit {
        permit.before_exec()?;
    }
    use std::os::unix::process::CommandExt;
    Err(cmd.exec().into())
}

#[cfg(test)]
#[path = "scheduling_kernel_tests.rs"]
mod scheduling_kernel_tests;

#[cfg(test)]
mod reclaim_integration_tests {
    use super::*;

    fn sample(ino: u64) -> reclaim_policy::Sample {
        reclaim_policy::Sample {
            dev: 7,
            ino,
            populated: true,
            current_bytes: 512 * MIB,
            file_bytes: 256 * MIB,
            inactive_file_bytes: 128 * MIB,
            pgsteal_proactive: Some(100),
            ..reclaim_policy::Sample::default()
        }
    }

    #[test]
    fn selection_skips_the_largest_ineligible_group_and_counts_every_eligible_group() {
        let mut tracker = reclaim_policy::Tracker::new();
        let mut anonymous = sample(1);
        anonymous.current_bytes = 4 * GIB;
        anonymous.file_bytes = 0;
        let selected = select_reclaim_candidate(
            &mut tracker,
            [
                (PathBuf::from("/anonymous"), anonymous),
                (PathBuf::from("/usable-first"), sample(2)),
                (PathBuf::from("/usable-second"), sample(3)),
            ],
            64 * MIB,
            4096,
            0,
        );
        assert_eq!(selected.eligible, 2);
        let first = selected.first.unwrap();
        assert_eq!(first.path, Path::new("/usable-first"));
        assert_eq!(first.request.bytes, 64 * MIB);
    }

    #[test]
    fn no_global_reclaim_request_does_not_even_read_candidate_samples() {
        let reads = std::cell::Cell::new(0);
        let candidates = [1, 2].into_iter().map(|ino| {
            reads.set(reads.get() + 1);
            (PathBuf::from("/candidate"), sample(ino))
        });
        let selected =
            select_reclaim_candidate(&mut reclaim_policy::Tracker::new(), candidates, 0, 4096, 0);
        assert_eq!(reads.get(), 0);
        assert_eq!(selected.eligible, 0);
        assert!(selected.first.is_none());
    }

    #[test]
    fn no_progress_feedback_routes_the_next_attempt_to_another_group() {
        let mut tracker = reclaim_policy::Tracker::new();
        let rows = || {
            [
                (PathBuf::from("/first"), sample(1)),
                (PathBuf::from("/second"), sample(2)),
            ]
        };
        let first = select_reclaim_candidate(&mut tracker, rows(), 64 * MIB, 4096, 0);
        let mut pending = first.first;
        let (_, outcome) =
            finish_pending_reclaim(&mut pending, &mut tracker, Some(sample(1)), 1).unwrap();
        assert_eq!(outcome.progress, reclaim_policy::Progress::NoProgress);
        let next = select_reclaim_candidate(&mut tracker, rows(), 64 * MIB, 4096, 2);
        assert_eq!(next.eligible, 1);
        assert_eq!(next.first.unwrap().path, Path::new("/second"));
    }

    #[test]
    fn deadline_feedback_is_not_recorded_again_when_the_worker_is_later_reaped() {
        let mut tracker = reclaim_policy::Tracker::new();
        let before = sample(1);
        let request = tracker.plan(before, 64 * MIB, 4096, 0).unwrap();
        let mut pending = Some(ReclaimTarget {
            path: PathBuf::from("/worker"),
            request,
        });
        let (_, outcome) =
            finish_pending_reclaim(&mut pending, &mut tracker, Some(before), 5).unwrap();
        assert_eq!(outcome.retry_after_secs, 65);
        assert!(finish_pending_reclaim(&mut pending, &mut tracker, Some(before), 6).is_none());
        assert_eq!(
            tracker.plan(before, 64 * MIB, 4096, 64),
            Err(reclaim_policy::SkipReason::NoProgressBackoff)
        );
        assert!(tracker.plan(before, 64 * MIB, 4096, 65).is_ok());
    }

    #[test]
    fn worker_identity_requires_both_numeric_fields_when_present() {
        assert_eq!(parse_reclaim_identity(None, None).unwrap(), None);
        assert_eq!(
            parse_reclaim_identity(Some("7"), Some("101")).unwrap(),
            Some((7, 101))
        );
        assert!(parse_reclaim_identity(Some("7"), None).is_err());
        assert!(parse_reclaim_identity(None, Some("101")).is_err());
        assert!(parse_reclaim_identity(Some("7"), Some("0")).is_err());
        assert!(parse_reclaim_identity(Some("7"), Some("101x")).is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_interactive_bypass_does_not_touch_sampling_or_runtime_state() {
        let called = std::cell::Cell::new(false);
        let result: Result<Option<()>> = with_background_admission("interactive", || {
            called.set(true);
            Err("missing kernel/runtime data".into())
        });
        assert!(result.unwrap().is_none());
        assert!(!called.get());
        let result: Result<Option<()>> = with_background_admission("background", || {
            called.set(true);
            Err("missing kernel/runtime data".into())
        });
        assert!(result.is_err());
        assert!(called.get());
    }

    #[test]
    fn admission_systemd_version_capability_does_not_drop_older_scope_support() {
        assert_eq!(
            systemd_run_version("systemd 252 (252.26-1~deb12u2)\n+PAM").unwrap(),
            252
        );
        assert_eq!(
            systemd_run_version("systemd 255 (255.4-1ubuntu8)\n").unwrap(),
            255
        );
        assert_eq!(systemd_run_version("systemd 254.2\n").unwrap(), 254);
        for invalid in [
            "",
            "unknown 255",
            "systemd next",
            "systemd 254garbage",
            "systemd 99999999999999999999999999",
        ] {
            assert!(systemd_run_version(invalid).is_err());
        }
        for version in [252, 253, 254, 255] {
            let command =
                workload_command("background", None, false, &["program".into()], version).unwrap();
            assert_eq!(
                command
                    .get_args()
                    .any(|arg| arg == "--expand-environment=no"),
                version >= 254
            );
        }
    }

    #[test]
    fn admission_scope_preserves_literal_argument_bytes_and_dedup_chain_without_hard_background_limits()
     {
        use std::os::unix::ffi::OsStringExt;
        let args: Vec<std::ffi::OsString> = vec![
            "program".into(),
            "${SECRET}".into(),
            "--config".into(),
            "--wait-timeout".into(),
            std::ffi::OsString::from_vec(vec![0xff, b'$', b' ']),
        ];
        for dedup in [false, true] {
            let command = workload_command("background", None, dedup, &args, 255).unwrap();
            let arguments: Vec<_> = command.get_args().collect();
            assert_eq!(
                &arguments[arguments.len() - args.len()..],
                args.iter().map(|arg| arg.as_os_str()).collect::<Vec<_>>()
            );
            assert_eq!(command.get_program(), "/usr/bin/systemd-run");
            assert_eq!(command.get_envs().count(), 0, "inherit caller environment");
            assert!(command.get_current_dir().is_none(), "inherit caller cwd");
            assert!(!arguments.iter().any(|arg| {
                let arg = arg.to_string_lossy();
                arg.contains("MemoryHigh=")
                    || arg.contains("MemoryMax=")
                    || arg.contains("CPUQuota=")
                    || arg.contains("BandwidthMax=")
            }));
            assert_eq!(arguments.iter().any(|arg| *arg == "__dedup-exec"), dedup);
            assert!(
                arguments
                    .iter()
                    .any(|arg| *arg == "--description=zmem background workload")
            );
        }
    }

    #[test]
    fn process_exit_races_are_distinct_from_incomplete_access() {
        assert!(process_disappeared(&std::io::Error::from_raw_os_error(2)));
        assert!(process_disappeared(&std::io::Error::from_raw_os_error(3)));
        for code in [1, 5, 13, 22] {
            assert!(!process_disappeared(&std::io::Error::from_raw_os_error(
                code
            )));
        }
    }

    struct JournalFixture(PathBuf);
    impl JournalFixture {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "zmem-{name}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn control(&self, name: &str) -> Control {
            let path = self.0.join(name);
            fs::write(&path, "8192").unwrap();
            let meta = fs::metadata(&path).unwrap();
            Control {
                path,
                original: "4096".into(),
                last: "8192".into(),
                dev: meta.dev(),
                ino: meta.ino(),
                pending_previous: None,
            }
        }
    }
    impl Drop for JournalFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn swap_helper_has_fixed_host_namespace_arguments_and_valid_priorities() {
        let identity = SwapIdentity {
            dev: 12,
            ino: 34,
            rdev: 0,
        };
        let args = swap_helper_args(
            Path::new("/usr/local/bin/zmem"),
            Path::new("/var/lib/zmem/swap/zmem-000001.swap"),
            SwapAction::Activate(10),
            identity,
            9,
        )
        .unwrap();
        assert_eq!(
            args,
            [
                "--target",
                "1",
                "--mount",
                "--root",
                "--wdns=/",
                "--no-fork",
                "--",
                "/usr/local/bin/zmem",
                "__swap-on",
                "/var/lib/zmem/swap/zmem-000001.swap",
                "10",
                "12",
                "34",
                "0",
                "9",
            ]
            .map(str::to_owned)
        );
        let off = swap_helper_args(
            Path::new("/usr/local/bin/zmem"),
            Path::new("/var/lib/zmem/swap/zmem-000001.swap"),
            SwapAction::Deactivate,
            identity,
            9,
        )
        .unwrap();
        assert_eq!(off[8], "__swap-off");
        assert_eq!(off[10], "-1");
        assert_eq!(off[14], "9");
        assert!(
            swap_helper_args(
                Path::new("/usr/local/bin/zmem"),
                Path::new("/var/lib/zmem/swap/zmem-000001.swap"),
                SwapAction::Deactivate,
                identity,
                2,
            )
            .is_err()
        );
        assert_eq!(swap_flags(-1).unwrap(), 0);
        assert_eq!(swap_flags(0).unwrap(), 0x8000);
        assert_eq!(swap_flags(32767).unwrap(), 0xffff);
        assert!(swap_flags(-2).is_err());
        assert!(swap_flags(32768).is_err());
    }

    #[test]
    fn swap_mutation_lock_survives_parent_close_until_execd_child_exits() {
        let fixture = JournalFixture::new("swap-mutation-lock");
        let path = fixture.0.join("lock");
        let open = || {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap()
        };
        File::create(&path).unwrap();
        let lock = SwapMutationLock::lock(open()).unwrap();
        assert!(SwapMutationLock::lock(open()).is_err());
        let mut child = Command::new("/bin/sh")
            .args(["-c", "printf ready; IFS= read -r release"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = [0u8; 5];
        child.stdout.take().unwrap().read_exact(&mut ready).unwrap();
        assert_eq!(&ready, b"ready");
        // Simulate a worker exiting after a timed-out syscall helper. Closing
        // its own descriptor must not permit a new mutation while the helper
        // still owns the inherited open file description.
        drop(lock);
        let still_locked = SwapMutationLock::lock(open()).is_err();
        child.stdin.take().unwrap().write_all(b"release\n").unwrap();
        assert!(child.wait().unwrap().success());
        assert!(still_locked);
        let _after_child_exit = SwapMutationLock::lock(open()).unwrap();
    }

    #[test]
    fn maintenance_accepts_only_exact_owned_directory_and_slot_names() {
        let settings = Settings::default();
        assert!(
            validate_owned_swap_path(&settings, Path::new("/var/lib/zmem/swap/zmem-000001.swap"),)
                .is_ok()
        );
        for path in [
            "/swap/zmem-000001.swap",
            "/var/lib/zmem/swap/subdir/zmem-000001.swap",
            "/var/lib/zmem/swap/zmem-1.swap",
            "/var/lib/zmem/swap/zmem-000001.swap.bak",
            "/var/lib/zmem/swap/.owner",
            "/var/lib/zmem/swap/zmem-１２３４５６.swap",
        ] {
            assert!(validate_owned_swap_path(&settings, Path::new(path)).is_err());
        }
    }

    #[test]
    fn swap_helper_errno_protocol_never_infers_busy_from_error_text() {
        assert_eq!(swap_reply_errno("zmem-swap-v1\t0\n").unwrap(), 0);
        assert_eq!(swap_reply_errno("zmem-swap-v1\t16\n").unwrap(), 16);
        for invalid in [
            "Device or resource busy",
            "设备或资源忙",
            "zmem-swap-v1\t-16\n",
            "zmem-swap-v1\t4096\n",
            "zmem-swap-v1\t16\nextra\n",
            "zmem-swap-v1\t16\n\n",
            "zmem-swap-v1\t\n",
            "zmem-swap-v1\t16",
        ] {
            assert!(swap_reply_errno(invalid).is_err(), "{invalid:?}");
        }
        assert!(swap_busy(&SwapSyscallError(16)));
        assert!(!swap_busy(&SwapSyscallError(13)));
        assert!(!swap_busy(&std::io::Error::from_raw_os_error(16)));
        assert_eq!(swap_syscall_errno(&SwapSyscallError(1)), Some(1));
        assert_eq!(
            swap_syscall_errno(&std::io::Error::from_raw_os_error(1)),
            None
        );
        assert!(!swap_busy(&std::io::Error::other(
            "Device or resource busy"
        )));
    }

    #[test]
    fn swap_helper_identity_stays_pinned_when_a_path_is_replaced() {
        let fixture = JournalFixture::new("swap-pinned-identity");
        let path = fixture.0.join("swap");
        fs::write(&path, "original").unwrap();
        let original = OpenOptions::new()
            .read(true)
            .custom_flags(O_PATH | O_NOFOLLOW)
            .open(&path)
            .unwrap();
        let expected = SwapIdentity::of(&original.metadata().unwrap());
        fs::rename(&path, fixture.0.join("previous")).unwrap();
        fs::write(&path, "replacement").unwrap();
        let replacement = File::open(&path).unwrap();
        verify_swap_identity(&original, expected).unwrap();
        assert!(verify_swap_identity(&replacement, expected).is_err());
        let mut changed_device = expected;
        changed_device.rdev = 1;
        assert!(verify_swap_identity(&original, changed_device).is_err());
    }

    fn swap_row(path: &str, size: u64) -> Swap {
        Swap {
            path: path.into(),
            kind: "file".into(),
            size,
            used: 0,
            priority: 10,
        }
    }

    #[test]
    fn disk_swap_budget_counts_pending_and_foreign_without_double_counting_canonical() {
        let first = "/var/lib/zmem/swap/zmem-000001.swap";
        let pending = "/var/lib/zmem/swap/zmem-000002.swap";
        let owned = BTreeMap::from([
            (PathBuf::from(first), 2 * GIB),
            (PathBuf::from(pending), GIB),
        ]);
        let mut active = vec![
            swap_row(first, 2 * GIB - 4096),
            swap_row("/foreign/swap", GIB),
        ];
        active.push(Swap {
            kind: "partition".into(),
            ..swap_row("/dev/zram0", 8 * GIB)
        });
        active.push(Swap {
            kind: "partition".into(),
            ..swap_row("/dev/nvme0n1p3", GIB)
        });
        assert_eq!(disk_swap_budget(&owned, &active), 5 * GIB);
        // An inactive file is charged even when /proc/swaps is empty.
        assert_eq!(disk_swap_budget(&owned, &[]), 3 * GIB);
    }

    #[test]
    fn disk_swap_budget_does_not_claim_alias_identity_and_saturates() {
        let path = PathBuf::from("/var/lib/zmem/swap/zmem-000001.swap");
        let owned = BTreeMap::from([(path.clone(), GIB)]);
        assert_eq!(
            disk_swap_budget(&owned, &[swap_row("/swap/zmem-000001.swap", GIB)]),
            2 * GIB
        );
        assert_eq!(
            disk_swap_budget(&owned, &[swap_row(path.to_str().unwrap(), 2 * GIB)]),
            2 * GIB
        );
        let oversized = BTreeMap::from([(path, u64::MAX)]);
        assert_eq!(
            disk_swap_budget(&oversized, &[swap_row("/foreign/swap", GIB)]),
            u64::MAX
        );
    }

    #[test]
    fn disk_swap_growth_budget_handles_partial_capacity_and_saturation() {
        assert_eq!(
            disk_swap_growth_bytes(2 * GIB, 2 * GIB, 4 * GIB, 3 * GIB),
            GIB
        );
        assert_eq!(
            disk_swap_growth_bytes(2 * GIB, 2 * GIB, 4 * GIB, 4 * GIB),
            0
        );
        assert_eq!(
            disk_swap_growth_bytes(2 * GIB, 2 * GIB, 4 * GIB, u64::MAX),
            0
        );
        assert_eq!(
            disk_swap_growth_bytes(GIB, 2 * GIB, 4 * GIB, 4 * GIB - 4095),
            0
        );
        assert_eq!(disk_swap_growth_bytes(2 * GIB, GIB, 4 * GIB, 0), GIB);
    }

    #[test]
    fn disk_swap_cleanup_requires_confirmed_failure_unchanged_identity_and_inactive_path() {
        let path = Path::new("/var/lib/zmem/swap/zmem-000001.swap");
        let expected = SwapIdentity {
            dev: 1,
            ino: 2,
            rdev: 0,
        };
        assert!(can_remove_failed_swap(
            path,
            expected,
            Some(expected),
            Some(1),
            &[]
        ));
        for errno in [None, Some(0), Some(16), Some(-1), Some(4096)] {
            assert!(!can_remove_failed_swap(
                path,
                expected,
                Some(expected),
                errno,
                &[]
            ));
        }
        let replacement = SwapIdentity { ino: 3, ..expected };
        assert!(!can_remove_failed_swap(
            path,
            expected,
            Some(replacement),
            Some(1),
            &[]
        ));
        assert!(!can_remove_failed_swap(path, expected, None, Some(1), &[]));
        assert!(!can_remove_failed_swap(
            path,
            expected,
            Some(expected),
            Some(1),
            &[swap_row(path.to_str().unwrap(), GIB)]
        ));
    }

    #[test]
    fn protection_roles_select_actual_critical_processes_and_named_remote_units() {
        use protection::Role;
        let app = Path::new(
            "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice/app-graphical.slice/app.scope",
        );
        for (comm, expected) in [
            ("Hyprland\n", Some(Role::Session)),
            ("kwin_wayland", Some(Role::Session)),
            ("fcitx5", Some(Role::Input)),
            ("ibus-engine-lib", Some(Role::Input)),
            ("gnome-terminal-", Some(Role::Terminal)),
            ("kitty", Some(Role::Terminal)),
            ("ssmcp", Some(Role::Remote)),
            ("python3", None),
            ("node", None),
            ("terminal-bench", None),
            ("ChatGPT", None),
        ] {
            assert_eq!(protection_role(comm, app), expected, "{comm}");
        }
        let remote = app.with_file_name("app-ssmcp-worker.scope");
        assert_eq!(protection_role("python3", &remote), Some(Role::Remote));
        let unrelated = app.with_file_name("app-ssmcpunrelated.scope");
        assert_eq!(protection_role("node", &unrelated), None);
    }

    #[test]
    fn protection_ownership_preserves_external_nonzero_values_and_recreated_groups() {
        let path = Path::new("/sys/fs/cgroup/user.slice/memory.low");
        let mut entry = Control {
            path: path.into(),
            original: "0".into(),
            last: "8193".into(),
            dev: 10,
            ino: 20,
            pending_previous: None,
        };
        assert!(can_manage_protection(None, path, "0", 10, 20, 4096).unwrap());
        for value in ["4096", "8192", "max"] {
            assert!(!can_manage_protection(None, path, value, 10, 20, 4096).unwrap());
        }
        assert!(can_manage_protection(Some(&entry), path, "8192", 10, 20, 4096).unwrap());
        assert!(can_manage_protection(Some(&entry), path, "0", 10, 20, 4096).unwrap());
        for external in ["4096", "16384", "max"] {
            assert!(can_manage_protection(Some(&entry), path, external, 10, 20, 4096).is_err());
        }
        assert!(!can_manage_protection(Some(&entry), path, "8192", 10, 21, 4096).unwrap());
        assert!(can_manage_protection(Some(&entry), path, "0", 10, 21, 4096).unwrap());
        entry.last = "16384".into();
        entry.pending_previous = Some("8192".into());
        assert!(can_manage_protection(Some(&entry), path, "8192", 10, 20, 4096).unwrap());
    }

    #[test]
    fn protection_cleanup_requires_a_complete_plan_and_only_selects_obsolete_low_controls() {
        let mut entries = BTreeMap::new();
        for name in [
            "old/memory.low",
            "retained/memory.low",
            "background/memory.high",
        ] {
            let path = Path::new(CGROOT).join(name);
            entries.insert(
                path.clone(),
                Control {
                    path,
                    original: "0".into(),
                    last: "8192".into(),
                    dev: 1,
                    ino: 1,
                    pending_previous: None,
                },
            );
        }
        let mut desired = BTreeMap::new();
        desired.insert(Path::new(CGROOT).join("retained/memory.low"), 8192);
        assert!(expired_protection_paths(&entries, &desired, false).is_empty());
        assert_eq!(
            expired_protection_paths(&entries, &desired, true),
            vec![Path::new(CGROOT).join("old/memory.low")]
        );
        let controls = Controls::new();
        assert_eq!(controls.protection_budget_bytes, 0);
        assert_eq!(controls.protected_critical_scopes, 0);
        assert!(!controls.protection_complete);
    }

    #[test]
    fn protection_discovery_finds_nested_scopes_and_deduplicates_processes() {
        let fixture = JournalFixture::new("protection-discovery");
        let proc_root = fixture.0.join("proc");
        let cgroup_root = fixture.0.join("cgroup");
        fs::create_dir_all(&proc_root).unwrap();
        let terminal = "user.slice/user-1000.slice/user@1000.service/app.slice/app-graphical.slice/terminal.scope";
        let input = "user.slice/user-1000.slice/user@1000.service/app.slice/app-graphical.slice/input.scope";
        let remote =
            "user.slice/user-1000.slice/user@1000.service/app.slice/app-ssmcp-worker.scope";
        for (pid, comm, path) in [
            (1, "kitty", terminal),
            (2, "kitty", terminal),
            (3, "fcitx5", input),
            (4, "node", remote),
            (
                5,
                "node",
                "user.slice/user-1000.slice/user@1000.service/app.slice/unrelated.scope",
            ),
            (
                6,
                "kitty",
                "user.slice/user-1000.slice/user@1000.service/zmem.slice/background.scope",
            ),
            (
                7,
                "kitty",
                "user.slice/user-1000.slice/user@1001.service/app.slice/mismatched.scope",
            ),
        ] {
            let process = proc_root.join(pid.to_string());
            fs::create_dir(&process).unwrap();
            fs::write(process.join("comm"), comm).unwrap();
            fs::write(process.join("cgroup"), format!("0::/{path}\n")).unwrap();
        }
        for path in [terminal, input, remote, "system.slice/sshd.service"] {
            let group = cgroup_root.join(path);
            fs::create_dir_all(&group).unwrap();
            fs::write(group.join("memory.current"), (16 * MIB).to_string()).unwrap();
        }
        let scopes = discover_protection_scopes(
            &proc_root,
            &cgroup_root,
            16 * GIB,
            4096,
            64,
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(scopes.len(), 4);
        assert_eq!(
            scopes
                .iter()
                .filter(|scope| scope.path == cgroup_root.join(terminal))
                .count(),
            1
        );
        let p = protection::plan_protection(&cgroup_root, 16 * GIB, 4096, &scopes).unwrap();
        assert_eq!(p.leaves.len(), 4);
        assert!(p.limits.contains_key(&cgroup_root.join(
            "user.slice/user-1000.slice/user@1000.service/app.slice/app-graphical.slice/memory.low"
        )));
    }

    #[test]
    fn protection_discovery_selects_ssh_login_sessions_without_generic_session_protection() {
        let fixture = JournalFixture::new("protection-ssh-sessions");
        let proc_root = fixture.0.join("proc");
        let cgroup_root = fixture.0.join("cgroup");
        fs::create_dir_all(&proc_root).unwrap();
        let ssh = [
            "user.slice/user-1000.slice/session-3.scope",
            "user.slice/user-1000.slice/session-c1.scope",
            "user.slice/user-1000.slice/session-4.scope",
        ];
        for (pid, comm, path) in [
            (1, "sshd", ssh[0]),
            (2, "sshd-session", ssh[1]),
            (3, "sshd-auth", ssh[2]),
            (4, "bash", "user.slice/user-1000.slice/session-5.scope"),
            (5, "kitty", "user.slice/user-1000.slice/session-6.scope"),
            (6, "python3", "user.slice/user-1000.slice/session-7.scope"),
            (7, "sshd", "user.slice/user-01000.slice/session-8.scope"),
            (8, "sshd", "user.slice/user-1000.slice/unrelated.scope"),
            (9, "sshd", "user.slice/user-1000.slice/session-.scope"),
        ] {
            let process = proc_root.join(pid.to_string());
            fs::create_dir(&process).unwrap();
            fs::write(process.join("comm"), comm).unwrap();
            fs::write(process.join("cgroup"), format!("0::/{path}\n")).unwrap();
            let group = cgroup_root.join(path);
            fs::create_dir_all(&group).unwrap();
            fs::write(group.join("memory.current"), (16 * MIB).to_string()).unwrap();
        }
        let scopes = discover_protection_scopes(
            &proc_root,
            &cgroup_root,
            16 * GIB,
            4096,
            64,
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(scopes.len(), ssh.len());
        for path in ssh {
            assert!(scopes.iter().any(|scope| {
                scope.path == cgroup_root.join(path) && scope.role == protection::Role::Remote
            }));
        }
        let p = protection::plan_protection(&cgroup_root, 16 * GIB, 4096, &scopes).unwrap();
        assert_eq!(p.allocated_bytes, 48 * MIB);
        assert_eq!(
            p.limits[&cgroup_root.join("user.slice/user-1000.slice/memory.low")],
            48 * MIB
        );
        assert!(!p.limits.contains_key(
            &cgroup_root.join("user.slice/user-1000.slice/user@1000.service/memory.low")
        ));
    }

    #[test]
    fn protection_discovery_reports_partial_limits_and_invalid_input() {
        let fixture = JournalFixture::new("protection-incomplete");
        let proc_root = fixture.0.join("proc");
        let cgroup_root = fixture.0.join("cgroup");
        let process = proc_root.join("1");
        fs::create_dir_all(&process).unwrap();
        fs::write(process.join("comm"), "kitty").unwrap();
        fs::write(
            process.join("cgroup"),
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/term.scope\n",
        )
        .unwrap();
        assert!(
            discover_protection_scopes(
                &proc_root,
                &cgroup_root,
                16 * GIB,
                4096,
                0,
                Duration::from_secs(5)
            )
            .is_err()
        );
        assert!(
            discover_protection_scopes(
                &proc_root,
                &cgroup_root,
                16 * GIB,
                4096,
                64,
                Duration::ZERO
            )
            .is_err()
        );
        fs::write(process.join("comm"), "x".repeat(129)).unwrap();
        assert!(
            discover_protection_scopes(
                &proc_root,
                &cgroup_root,
                16 * GIB,
                4096,
                64,
                Duration::from_secs(5)
            )
            .is_err()
        );
        // A process that exited during discovery is an ordinary missing proc
        // entry; it does not turn the remaining snapshot into a truncated scan.
        fs::remove_file(process.join("comm")).unwrap();
        assert!(
            discover_protection_scopes(
                &proc_root,
                &cgroup_root,
                16 * GIB,
                4096,
                64,
                Duration::from_secs(5)
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn protection_process_membership_is_unified_and_user_scoped() {
        assert!(
            protection_process_path(Path::new(CGROOT), "0::/system.slice/sshd.service\n")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            protection_process_path(
                Path::new(CGROOT),
                "0::/user.slice/user-1000.slice/session-3.scope\n"
            )
            .unwrap(),
            Some(Path::new(CGROOT).join("user.slice/user-1000.slice/session-3.scope"))
        );
        assert!(protection_process_path(Path::new(CGROOT), "0::relative\n").is_err());
        assert!(protection_process_path(Path::new(CGROOT), "0::/\n0::/other\n").is_err());
    }

    #[test]
    fn oversized_control_recovery_retains_the_complete_journal() {
        let fixture = JournalFixture::new("journal-limit");
        let journal = fixture.0.join("controls.tsv");
        let limit = 1024 * 1024;
        let mut text = "x".repeat(limit);
        fs::write(&journal, &text).unwrap();
        assert_eq!(read(&journal).unwrap().len(), limit);
        text.push('x');
        fs::write(&journal, &text).unwrap();
        let error = restore_controls_from(&journal).unwrap_err();
        assert!(error.to_string().contains("read limit"));
        assert_eq!(fs::read_to_string(&journal).unwrap(), text);
        assert!(restore_controls_from(&fixture.0.join("absent.tsv")).is_ok());
    }

    #[test]
    fn control_pruning_keeps_live_baselines_and_removes_dead_identities() {
        let fixture = JournalFixture::new("journal-pruning");
        let live = fixture.control("live");
        let removed = fixture.control("removed");
        let replaced = fixture.control("replaced");
        fs::remove_file(&removed.path).unwrap();
        // Keep the old inode alive while creating its replacement.
        fs::rename(&replaced.path, fixture.0.join("previous-inode")).unwrap();
        fs::write(&replaced.path, "16384").unwrap();
        let mut controls = Controls::new();
        for entry in [live.clone(), removed, replaced] {
            controls.entries.insert(entry.path.clone(), entry);
        }
        assert!(controls.prune_stale());
        assert_eq!(controls.entries.len(), 1);
        let retained = controls.entries.get(&live.path).unwrap();
        assert_eq!(retained.original, "4096");
        assert_eq!(retained.last, "8192");
        assert_eq!((retained.dev, retained.ino), (live.dev, live.ino));
        assert!(
            !controls.prune_stale(),
            "unchanged controls need no journal rewrite"
        );
    }

    #[test]
    fn control_pruning_preserves_uncertain_metadata_errors() {
        let fixture = JournalFixture::new("journal-metadata");
        let entry = fixture.control("live");
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::Interrupted,
            std::io::ErrorKind::Other,
        ] {
            assert!(!stale_control(&entry, Err(std::io::Error::from(kind))));
        }
        assert!(stale_control(
            &entry,
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        ));
    }

    fn swappiness_fixture() -> Telemetry {
        Telemetry {
            sample: Snapshot {
                total_bytes: 16 * GIB,
                available_bytes: 8 * GIB,
                swap_total_bytes: 10 * GIB,
                swap_free_bytes: 5 * GIB,
                ..Snapshot::default()
            },
            swaps: vec![Swap {
                path: "/dev/zram0".into(),
                kind: "partition".into(),
                size: 10 * GIB,
                used: 5 * GIB,
                priority: 100,
            }],
            zram: vec![Zram {
                name: "zram0".into(),
                original: 5 * GIB,
                compressed: 2 * GIB,
                resident: 2 * GIB,
                same_pages: 0,
                logical_size: 10 * GIB,
                logical_used: 5 * GIB,
                memory_limit: 0,
                algorithm: "zstd".into(),
            }],
            paging: Paging {
                rates_available: true,
                zram_rates_available: true,
                disk_swap_estimate_available: true,
                zswap_enabled: Some(false),
                ..Paging::default()
            },
            ..Telemetry::default()
        }
    }

    #[test]
    fn swappiness_distinguishes_confirmed_disk_reads_from_io_only_pressure() {
        for profile in ["balanced", "interactive", "throughput", "ai", "gaming"] {
            let settings = Settings {
                profile: profile.into(),
                ..Settings::default()
            };
            for (io, input, output) in [(1000, 0, 0), (0, 64 * MIB, 0), (0, 16 * MIB, 64 * MIB)] {
                for used in [5 * GIB, 8 * GIB, 10 * GIB] {
                    let mut t = swappiness_fixture();
                    t.swaps[0].used = used;
                    t.zram[0].logical_used = used;
                    t.sample.swap_free_bytes = 10 * GIB - used;
                    t.sample.io_full_bp = io;
                    t.sample.swap_in_bytes_per_sec = input;
                    t.paging.estimated_disk_swap_in_bytes_per_sec = input;
                    t.sample.swap_out_bytes_per_sec = output;
                    let d = Policy::new(settings.policy).observe(t.sample, 0);
                    assert_eq!(d.reason, zmem::Reason::SwapIoPressure);
                    assert_eq!(
                        effective_swappiness(&settings, &t, &d),
                        if input >= 32 * MIB { 20 } else { 60 },
                        "{profile}"
                    );
                }
            }
        }
    }

    #[test]
    fn swappiness_uses_measured_zram_efficiency_and_residency() {
        for profile in ["balanced", "interactive", "throughput", "ai", "gaming"] {
            let settings = Settings {
                profile: profile.into(),
                ..Settings::default()
            };
            let mut t = swappiness_fixture();
            let d = Policy::new(settings.policy).observe(t.sample, 0);
            assert_eq!(
                effective_swappiness(&settings, &t, &d),
                if matches!(profile, "interactive" | "gaming") {
                    100
                } else {
                    120
                }
            );
            t.zram[0].resident = 4 * GIB; // Poor compression must not promote more swap.
            assert_eq!(effective_swappiness(&settings, &t, &d), 60);
            t.zram[0].resident = 2 * GIB;
            t.zram[0].logical_used = 10 * GIB; // Logical headroom is separately checked.
            assert_eq!(effective_swappiness(&settings, &t, &d), 60);
            t.zram.clear();
            assert_eq!(effective_swappiness(&settings, &t, &d), 60);
        }
    }

    #[test]
    fn file_refault_evidence_can_override_undifferentiated_io_hint() {
        let settings = Settings::default();
        let mut t = swappiness_fixture();
        t.zram[0].logical_used = 10 * GIB;
        t.sample.io_full_bp = 1000;
        t.sample.swap_in_bytes_per_sec = 5 * MIB;
        t.paging.file_refault_bytes_per_sec = 44 * MIB;
        t.paging.anon_refault_bytes_per_sec = 5 * MIB;
        t.paging.direct_scan_bytes_per_sec = 170 * MIB;
        t.paging.estimated_disk_swap_in_bytes_per_sec = MIB;
        let d = Policy::new(settings.policy).observe(t.sample, 0);
        assert_eq!(d.recommended_swappiness, 20);
        assert_eq!(effective_swappiness(&settings, &t, &d), 80);
        t.paging.rates_available = false;
        assert_eq!(effective_swappiness(&settings, &t, &d), 60);
    }

    #[test]
    fn swappiness_reclaim_guard_uses_measured_stalls_even_when_pressure_is_healthy() {
        let settings = Settings::default();
        let mut t = swappiness_fixture();
        t.sample.available_bytes = 2_950_000_000;
        t.sample.memory_full_bp = 123;
        t.sample.io_full_bp = 429;
        t.paging.file_refault_bytes_per_sec = 79 * MIB;
        t.paging.anon_refault_bytes_per_sec = 57 * MIB / 10;
        t.paging.direct_scan_bytes_per_sec = 156 * MIB;
        for logical_used in [5 * GIB, 10 * GIB] {
            t.zram[0].logical_used = logical_used;
            let d = Policy::new(settings.policy).observe(t.sample, 0);
            assert_eq!(d.pressure, zmem::Pressure::Healthy);
            let preference = swappiness_preference(&settings, &t, &d);
            assert_eq!(preference.value, 60);
            assert_eq!(preference.reason, "reclaim_latency_guard");
            assert!(preference.ceiling_only);
        }
        t.sample.memory_full_bp = 99;
        let d = Policy::new(settings.policy).observe(t.sample, 0);
        assert_eq!(effective_swappiness(&settings, &t, &d), 80);
    }

    #[test]
    fn swappiness_missing_readback_cannot_write_a_cached_higher_value() {
        let desired = swappiness::preference(swappiness::Input {
            rates_available: true,
            direct_scan: 156 * MIB,
            anon_refault: 57 * MIB / 10,
            memory_full_bp: 123,
            ..swappiness::Input::default()
        });
        let mut governor = SwappinessGovernor::new();
        governor.observe(desired, Some(120), 0);
        let unavailable = governor.observe(desired, None, 2);
        assert_eq!(unavailable.value, 120);
        assert_eq!(unavailable.reason, "readback_unavailable");
        let mut kernel_value = 40;
        let mut writes = Vec::new();
        let mut applied = Applied {
            swappiness_requested: Some(120),
            ..Applied::default()
        };
        apply_swappiness_request(None, unavailable, &mut applied, |value| {
            kernel_value = value;
            writes.push(value);
            Ok(())
        })
        .unwrap();
        assert_eq!(kernel_value, 40);
        assert!(writes.is_empty());
        assert_eq!(applied.swappiness_requested, None);
        assert_eq!(applied.swappiness_reason, "readback_unavailable");

        let observed = governor.observe(desired, Some(kernel_value), 4);
        apply_swappiness_request(Some(kernel_value), observed, &mut applied, |value| {
            kernel_value = value;
            writes.push(value);
            Ok(())
        })
        .unwrap();
        assert_eq!(kernel_value, 40);
        assert_eq!(writes, [40]);
        assert_eq!(applied.swappiness_requested, Some(40));
    }

    #[test]
    fn proc_parsers_keep_units_and_paths() {
        let s = parse_meminfo(
            "MemTotal: 16000000 kB\nMemAvailable: 1234 kB\nSwapTotal: 456 kB\nSwapFree: 12 kB\n",
        )
        .unwrap();
        assert_eq!(s.available_bytes, 1234 * 1024);
        let v = parse_swaps("Filename Type Size Used Priority\n/var/lib/a\\040b file 100 10 -2\n")
            .unwrap();
        assert_eq!(v[0].path, "/var/lib/a b");
        assert_eq!(v[0].size, 102400);
        assert!(parse_meminfo("MemTotal: 1 kB").is_err());
        assert!(parse_swaps("Filename\nfile x broken 0 0").is_err());
    }
    #[test]
    fn cgroup_write_scope_is_bounded() {
        assert!(valid_control(Path::new(
            "/sys/fs/cgroup/user.slice/memory.high"
        )));
        for p in [
            "/etc/passwd",
            "/sys/fs/cgroup/../kernel/mm/memory.low",
            "/sys/fs/cgroup/cgroup.kill",
            "/proc/sys/vm/drop_caches",
        ] {
            assert!(!valid_control(Path::new(p)));
        }
        assert!(!owned_name(Path::new("/var/lib/zmem/swap/other.swap")));
        assert!(owned_name(Path::new("zmem-000001.swap")));
    }
    #[test]
    fn psi_converts_percent_to_basis_points() {
        assert_eq!(
            psi(
                "some avg10=12.34 avg60=0.00 total=9876\nfull avg10=2.00 total=456\n",
                "some "
            ),
            (1234, 9876)
        );
    }
    #[test]
    fn existing_background_limit_is_never_relaxed() {
        assert_eq!(capped_high("1234", "max").unwrap(), "1234");
        assert_eq!(capped_high("1234", "4096").unwrap(), "1234");
        assert_eq!(capped_high("1234", "512").unwrap(), "512");
        assert_eq!(capped_high("max", "4096").unwrap(), "4096");
        assert!(capped_high("broken", "4096").is_err());
    }
    #[test]
    fn memory_controls_use_the_kernels_page_granularity() {
        let high = Path::new("/sys/fs/cgroup/zmem.slice/zmem-background.slice/memory.high");
        let low = Path::new("/sys/fs/cgroup/user.slice/memory.low");
        for (path, requested, actual) in [
            (high, "11533690420", "11533688832"),
            (low, "1544690688", "1544687616"),
            (low, "514896896", "514895872"),
            (high, "4095", "0"),
            (high, "4096", "4096"),
        ] {
            assert_eq!(
                canonical_control_value(path, requested, 4096).unwrap(),
                actual
            );
        }
        assert_eq!(
            canonical_control_value(high, "131071", 65536).unwrap(),
            "65536"
        );
        assert_eq!(
            canonical_control_value(low, "131073", 65536).unwrap(),
            "131072"
        );
        assert_eq!(canonical_control_value(high, "max", 65536).unwrap(), "max");
        assert_eq!(
            canonical_control_value(Path::new("/proc/sys/vm/swappiness"), "120", 65536).unwrap(),
            "120"
        );
        assert!(canonical_control_value(high, "123", 0).is_err());
        let ksm_run = Path::new(dedup::Knob::Run.path());
        assert_eq!(canonical_control_value(ksm_run, "0", 4096).unwrap(), "0");
        assert_eq!(canonical_control_value(ksm_run, "1", 4096).unwrap(), "1");
        assert!(canonical_control_value(ksm_run, "2", 4096).is_err());
    }

    #[test]
    fn protection_default_reset_is_reapplied_only_for_an_owned_zero_baseline() {
        let path = Path::new("/sys/fs/cgroup/user.slice/memory.low");
        let entry = Control {
            path: path.into(),
            original: "0".into(),
            last: "4096".into(),
            dev: 7,
            ino: 9,
            pending_previous: None,
        };
        assert!(can_manage_protection(Some(&entry), path, "0", 7, 9, 4096).unwrap());
        assert!(can_manage_protection(Some(&entry), path, "8192", 7, 9, 4096).is_err());
        let external_baseline = Control {
            original: "1024".into(),
            ..entry.clone()
        };
        assert!(can_manage_protection(Some(&external_baseline), path, "0", 7, 9, 4096).is_err());
        assert!(!owned_protection_default_reset(
            Path::new("/proc/sys/vm/swappiness"),
            "0",
            "0"
        ));
        assert!(!owned_protection_default_reset(
            Path::new("/sys/fs/cgroup/user.slice/memory.high"),
            "0",
            "0"
        ));
    }

    #[test]
    fn status_retains_protection_failures_between_reconciliation_cycles() {
        let mut t = Telemetry::default();
        t.applied.protection_sampled_at = Some(123);
        t.applied.protection_errors = vec!["protection ancestor unreadable".into()];
        let d = Policy::new(zmem::Config::default()).observe(t.sample, 0);
        let value =
            crate::json::parse(&telemetry_json(&t, &d, "balanced", None, &[], false)).unwrap();
        let errors = value
            .get("errors")
            .and_then(crate::json::Value::as_array)
            .unwrap();
        assert!(
            errors
                .iter()
                .any(|value| value.as_str() == Some("protection ancestor unreadable"))
        );
        assert_eq!(
            value
                .get("controls")
                .unwrap()
                .get("protection_sampled_at_unix")
                .unwrap()
                .as_u64(),
            Some(123)
        );
        assert_eq!(
            value.get("ksm").unwrap().get("general_profit_bytes"),
            Some(&crate::json::Value::Null)
        );
        t.ksm.general_profit_bytes = Some(-16384);
        let value =
            crate::json::parse(&telemetry_json(&t, &d, "balanced", None, &[], false)).unwrap();
        assert_eq!(
            value.get("ksm").unwrap().get("general_profit_bytes"),
            Some(&crate::json::Value::Number("-16384".into()))
        );
    }
    #[test]
    fn legacy_journal_rounding_is_owned_but_external_changes_are_not() {
        let high = Path::new("/sys/fs/cgroup/zmem.slice/zmem-background.slice/memory.high");
        assert!(owned_control_value(high, "11533690420", None, "11533688832", 4096).unwrap());
        assert!(!owned_control_value(high, "11533690420", None, "11533684736", 4096).unwrap());
        assert!(!owned_control_value(high, "11533690420", None, "max", 4096).unwrap());
        assert!(owned_control_value(high, "131071", None, "65536", 65536).unwrap());
        assert!(!owned_control_value(high, "131071", None, "131072", 65536).unwrap());
    }
    #[test]
    fn pending_write_recovery_recognizes_only_its_two_owned_states() {
        let high = Path::new("/sys/fs/cgroup/zmem.slice/zmem-background.slice/memory.high");
        // A crash before the write leaves the previous value; after the write
        // it leaves the target. Both recover the same original baseline.
        assert!(owned_control_value(high, "12288", Some("8192"), "8192", 4096).unwrap());
        assert!(owned_control_value(high, "12288", Some("8192"), "12288", 4096).unwrap());
        assert!(!owned_control_value(high, "12288", Some("8192"), "16384", 4096).unwrap());
        // Once the write is confirmed, a later change back to the prior value
        // is external and must be retained.
        assert!(!owned_control_value(high, "12288", None, "8192", 4096).unwrap());
    }
    #[test]
    fn process_membership_recognizes_only_registered_background_roots() {
        for path in [
            "/zmem.slice/zmem-background.slice",
            "/zmem.slice/zmem-background.slice/job.scope",
            "/user.slice/user-1000.slice/user@1000.service/zmem.slice/zmem-background.slice/job.scope",
        ] {
            assert!(is_managed_background_path(path), "{path}");
        }
        for path in [
            "/",
            "zmem.slice/zmem-background.slice/job.scope",
            "/other.slice/zmem.slice/zmem-background.slice/job.scope",
            "/zmem.slice/zmem-background.slice-impostor/job.scope",
            "/user.slice/user-1000.slice/user@1001.service/zmem.slice/zmem-background.slice/job.scope",
            "/user.slice/user-name.slice/user@name.service/zmem.slice/zmem-background.slice/job.scope",
        ] {
            assert!(!is_managed_background_path(path), "{path}");
        }
    }

    #[test]
    fn only_owned_background_slices_may_reapply_an_unlimited_reset() {
        for path in [
            "/sys/fs/cgroup/zmem.slice/zmem-background.slice/memory.high",
            "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/zmem.slice/zmem-background.slice/memory.high",
        ] {
            let path = Path::new(path);
            assert!(managed_background_high_reset(path, "max", "max"));
            assert!(
                !managed_background_high_reset(path, "4096", "max"),
                "preserve finite original limits"
            );
            assert!(
                !managed_background_high_reset(path, "max", "8192"),
                "preserve finite external limits"
            );
        }
        for path in [
            "/sys/fs/cgroup/zmem.slice/zmem-background.slice/memory.low",
            "/sys/fs/cgroup/zmem.slice/zmem-background.slice/job.scope/memory.high",
            "/sys/fs/cgroup/other.slice/zmem-background.slice/memory.high",
            "/sys/fs/cgroup/zmem.slice/zmem-background-other.slice/memory.high",
            "/sys/fs/cgroup/user.slice/user-1000.slice/user@1001.service/zmem.slice/zmem-background.slice/memory.high",
            "/sys/fs/cgroup/user.slice/user-.slice/user@.service/zmem.slice/zmem-background.slice/memory.high",
            "/sys/fs/cgroup/user.slice/user-name.slice/user@name.service/zmem.slice/zmem-background.slice/memory.high",
            "/proc/sys/vm/swappiness",
        ] {
            assert!(
                !managed_background_high_reset(Path::new(path), "max", "max"),
                "{path}"
            );
        }
    }
    #[test]
    #[ignore = "requires root, cgroup v2, and a stopped zmem daemon"]
    fn kernel_cgroup_rounding_and_restore() {
        unsafe extern "C" {
            fn geteuid() -> u32;
        }
        assert_eq!(
            unsafe { geteuid() },
            0,
            "run this ignored regression as root"
        );
        assert!(
            !Path::new(CONTROL_FILE).exists(),
            "stop zmem and recover its control journal before this regression"
        );
        let _lock = InstanceLock::acquire().expect("the real zmem daemon must be stopped");
        assert!(
            !Path::new(CONTROL_FILE).exists(),
            "a control journal appeared before the regression acquired the lock"
        );
        let fixture = Path::new(CGROOT).join(format!("zmem-regression-{}", std::process::id()));
        fs::create_dir(&fixture).expect("create a fresh empty regression cgroup");
        eprintln!(
            "zmem regression fixture: {}; on failure retain it and {} for inspection",
            fixture.display(),
            CONTROL_FILE
        );
        assert!(
            read(fixture.join("cgroup.procs"))
                .unwrap()
                .trim()
                .is_empty(),
            "regression cgroup must contain no processes"
        );
        let high = fixture.join("memory.high");
        let low = fixture.join("memory.low");
        assert!(
            high.exists() && low.exists(),
            "the root cgroup must already delegate the memory controller; fixture: {}",
            fixture.display()
        );
        let original_high = read(&high).unwrap().trim().to_owned();
        let original_low = read(&low).unwrap().trim().to_owned();
        let page = control_page_size().unwrap();
        let verify = |controls: &Controls, path: &Path, expected: u64| {
            let expected = expected.to_string();
            assert_eq!(
                read(path).unwrap().trim(),
                expected,
                "kernel value at {}",
                path.display()
            );
            let entry = controls.entries.get(path).expect("control ownership entry");
            assert_eq!(
                entry.last, expected,
                "in-memory ownership must use the actual kernel value"
            );
            assert!(
                entry.pending_previous.is_none(),
                "a verified write must be committed"
            );
            let journal = read(CONTROL_FILE).unwrap();
            let row = journal
                .lines()
                .find(|line| line.split('\t').next() == path.to_str())
                .expect("persisted control ownership entry");
            let fields: Vec<_> = row.split('\t').collect();
            assert_eq!(fields.len(), 6);
            assert_eq!(
                fields[2], expected,
                "journal last must equal the actual kernel value"
            );
            assert!(
                fields[5].is_empty(),
                "the committed journal must not retain a pending value"
            );
        };
        let mut controls = Controls::new();
        controls
            .set(&high, &(page * 3 + 17).to_string(), true)
            .unwrap();
        controls.set(&low, &(page + 31).to_string(), false).unwrap();
        verify(&controls, &high, page * 3);
        verify(&controls, &low, page);
        controls
            .set(&high, &(page * 7 + 123).to_string(), true)
            .expect("second nonaligned high write must remain controllable");
        controls
            .set(&low, &(page * 2 + 127).to_string(), false)
            .expect("second nonaligned low write must remain controllable");
        verify(&controls, &high, page * 7);
        verify(&controls, &low, page * 2);
        assert_eq!(controls.entries.get(&high).unwrap().original, original_high);
        assert_eq!(controls.entries.get(&low).unwrap().original, original_low);
        restore_controls().unwrap();
        assert_eq!(read(&high).unwrap().trim(), original_high);
        assert_eq!(read(&low).unwrap().trim(), original_low);
        assert!(!Path::new(CONTROL_FILE).exists());

        // This direct write represents another controller changing only the
        // empty fixture. Neither a later policy update nor rollback may undo it.
        let mut controls = Controls::new();
        controls
            .set(&high, &(page * 3 + 17).to_string(), true)
            .unwrap();
        let external = (page * 9).to_string();
        fs::write(&high, &external).unwrap();
        let error = controls
            .set(&high, &(page * 4 + 19).to_string(), true)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("external control change preserved")
        );
        assert_eq!(read(&high).unwrap().trim(), external);
        restore_controls().unwrap();
        assert_eq!(read(&high).unwrap().trim(), external);
        assert!(!Path::new(CONTROL_FILE).exists());
        fs::write(&high, &original_high).unwrap();

        // Exercise the actual kernel rounding with the old five-column
        // write-ahead format, then restore via the upgrade snapshot interface.
        let mut legacy = String::new();
        for (path, original, requested) in [
            (&high, &original_high, page * 5 + 73),
            (&low, &original_low, page * 3 + 41),
        ] {
            fs::write(path, requested.to_string()).unwrap();
            assert_eq!(
                read(path).unwrap().trim(),
                (requested / page * page).to_string()
            );
            let metadata = fs::metadata(path).unwrap();
            legacy.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\n",
                path.display(),
                original,
                requested,
                metadata.dev(),
                metadata.ino()
            ));
        }
        restore_control_snapshot(&legacy).unwrap();
        assert_eq!(read(&high).unwrap().trim(), original_high);
        assert_eq!(read(&low).unwrap().trim(), original_low);
        assert!(!Path::new(CONTROL_FILE).exists());
        assert!(
            read(fixture.join("cgroup.procs"))
                .unwrap()
                .trim()
                .is_empty()
        );
        fs::remove_dir(&fixture).expect("remove the successful empty regression cgroup");
    }
    #[test]
    #[ignore = "requires root, a stopped zmem daemon, and no system zmem.slice cgroup"]
    fn kernel_managed_background_reset_and_restore() {
        unsafe extern "C" {
            fn geteuid() -> u32;
        }
        assert_eq!(
            unsafe { geteuid() },
            0,
            "run this ignored regression as root"
        );
        let parent = Path::new(CGROOT).join("zmem.slice");
        assert!(
            !parent.exists(),
            "refusing to reuse an existing system zmem.slice"
        );
        assert!(
            !Path::new(CONTROL_FILE).exists(),
            "stop zmem and recover its control journal first"
        );
        let _lock = InstanceLock::acquire().expect("the real zmem daemon must be stopped");
        assert!(
            !parent.exists(),
            "system zmem.slice appeared before the regression acquired the lock"
        );
        assert!(
            !Path::new(CONTROL_FILE).exists(),
            "a control journal appeared before the regression acquired the lock"
        );
        fs::create_dir(&parent).expect("create a fresh empty system zmem.slice fixture");
        eprintln!(
            "zmem managed-slice regression fixture: {}; on failure retain it and {} for inspection",
            parent.display(),
            CONTROL_FILE
        );
        assert!(read(parent.join("cgroup.procs")).unwrap().trim().is_empty());
        assert!(
            read(parent.join("cgroup.controllers"))
                .unwrap()
                .split_whitespace()
                .any(|name| name == "memory"),
            "the root cgroup must already delegate the memory controller"
        );
        // Enable memory only in the fresh, empty fixture. No root controller
        // configuration or real workload cgroup is changed by this regression.
        fs::write(parent.join("cgroup.subtree_control"), "+memory").unwrap();
        let child = parent.join("zmem-background.slice");
        fs::create_dir(&child).unwrap();
        assert!(read(child.join("cgroup.procs")).unwrap().trim().is_empty());
        let high = child.join("memory.high");
        assert_eq!(read(&high).unwrap().trim(), "max");
        let page = control_page_size().unwrap();
        let mut controls = Controls::new();
        controls
            .set(&high, &(page * 32 + 23).to_string(), true)
            .unwrap();
        assert_eq!(read(&high).unwrap().trim(), (page * 32).to_string());
        let identity = fs::metadata(&high).unwrap();
        fs::write(&high, "max").unwrap();
        controls
            .set(&high, &(page * 64 + 17).to_string(), true)
            .expect("an unlimited reset of the owned aggregate must permit reapplying policy");
        let after = fs::metadata(&high).unwrap();
        assert_eq!((after.dev(), after.ino()), (identity.dev(), identity.ino()));
        let expected = (page * 64).to_string();
        assert_eq!(read(&high).unwrap().trim(), expected);
        let entry = controls.entries.get(&high).unwrap();
        assert_eq!(entry.original, "max");
        assert_eq!(entry.last, expected);
        assert!(entry.pending_previous.is_none());
        let journal = read(CONTROL_FILE).unwrap();
        let row = journal
            .lines()
            .find(|line| line.split('\t').next() == high.to_str())
            .unwrap();
        let fields: Vec<_> = row.split('\t').collect();
        assert_eq!(fields.len(), 6);
        assert_eq!(fields[1], "max");
        assert_eq!(fields[2], expected);
        assert!(fields[5].is_empty());
        restore_controls().unwrap();
        assert_eq!(read(&high).unwrap().trim(), "max");
        assert!(!Path::new(CONTROL_FILE).exists());

        let mut controls = Controls::new();
        controls
            .set(&high, &(page * 32 + 23).to_string(), true)
            .unwrap();
        let external = (page * 96).to_string();
        fs::write(&high, &external).unwrap();
        let error = controls
            .set(&high, &(page * 64 + 17).to_string(), true)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("external control change preserved")
        );
        assert_eq!(read(&high).unwrap().trim(), external);
        restore_controls().unwrap();
        assert_eq!(read(&high).unwrap().trim(), external);
        assert!(!Path::new(CONTROL_FILE).exists());
        assert!(read(child.join("cgroup.procs")).unwrap().trim().is_empty());
        assert!(read(parent.join("cgroup.procs")).unwrap().trim().is_empty());
        fs::remove_dir(&child).expect("remove the successful empty background fixture");
        fs::remove_dir(&parent).expect("remove the successful empty parent fixture");
    }
}
