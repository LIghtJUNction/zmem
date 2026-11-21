use crate::{Result, config::Settings, json};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use zmem::{Decision, GIB, MIB, Policy, Snapshot};

const RUNTIME: &str = "/run/zmem";
const CONTROL_FILE: &str = "/run/zmem/controls.tsv";
const CGROOT: &str = "/sys/fs/cgroup";
static STOP: AtomicBool = AtomicBool::new(false);
// These Unix interfaces are used only for advisory locking and signal delivery.
// The signal handler touches one lock-free atomic and performs no allocation.
unsafe extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
    fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
    fn kill(pid: i32, signal: i32) -> i32;
}
extern "C" fn stop_signal(_: i32) {
    STOP.store(true, Ordering::Relaxed);
}

pub fn read(path: impl AsRef<Path>) -> Result<String> {
    let mut out = String::new();
    File::open(path)?
        .take(1024 * 1024)
        .read_to_string(&mut out)?;
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
}
#[derive(Clone, Debug)]
pub struct Telemetry {
    pub sample: Snapshot,
    pub swaps: Vec<Swap>,
    pub zram: Vec<Zram>,
    pub ksm_profit: i64,
    pub ksm_shared: u64,
    pub sampled_at: u64,
}
pub struct Sampler {
    last: Option<(Instant, [u64; 5])>,
    page_size: u64,
}
impl Sampler {
    pub fn new() -> Self {
        Self {
            last: None,
            page_size: command("getconf", &["PAGESIZE"], 2)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(4096),
        }
    }
    pub fn sample(&mut self) -> Result<Telemetry> {
        let mut sample = parse_meminfo(&read("/proc/meminfo")?)?;
        let memory = read("/proc/pressure/memory")?;
        let io = read("/proc/pressure/io")?;
        let (some, ts) = psi(&memory, "some ");
        let (full, tf) = psi(&memory, "full ");
        let (ioavg, ti) = psi(&io, "full ");
        let (pin, pout) = vmstats(&read("/proc/vmstat")?);
        let nowi = Instant::now();
        sample.memory_some_bp = some;
        sample.memory_full_bp = full;
        sample.io_full_bp = ioavg;
        if let Some((last, counters)) = self.last {
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
        }
        self.last = Some((nowi, [ts, tf, ti, pin, pout]));
        let swaps = swaps()?;
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
                    });
                }
            }
        }
        Ok(Telemetry {
            sample,
            swaps,
            zram,
            ksm_profit: read("/sys/kernel/mm/ksm/general_profit")
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0),
            ksm_shared: number("/sys/kernel/mm/ksm/pages_sharing"),
            sampled_at: now(),
        })
    }
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
    format!(
        "{{\"schema\":\"zmem.status.v1\",\"version\":{},\"pid\":{},\"sampled_at_unix\":{},\"profile\":{},\"pressure\":{},\"reason\":{},\"memory\":{{\"total_bytes\":{},\"available_bytes\":{}}},\"swap\":{{\"total_bytes\":{},\"free_bytes\":{},\"in_bytes_per_sec\":{},\"out_bytes_per_sec\":{},\"devices\":[{}]}},\"psi\":{{\"memory_some_bp\":{},\"memory_full_bp\":{},\"io_full_bp\":{}}},\"zram\":[{}],\"ksm\":{{\"general_profit_bytes\":{},\"pages_sharing\":{}}},\"plan\":{{\"grow_swap_bytes\":{},\"shrink_allowed\":{},\"background_high_percent\":{},\"reclaim_request_bytes\":{},\"swappiness_hint\":{}}},\"worker\":{},\"errors\":{},\"processes\":{}}}",
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
        s.swap_in_bytes_per_sec,
        s.swap_out_bytes_per_sec,
        swap_rows,
        s.memory_some_bp,
        s.memory_full_bp,
        s.io_full_bp,
        zram,
        t.ksm_profit,
        t.ksm_shared,
        d.grow_swap_bytes,
        d.shrink_allowed,
        d.background_high_percent,
        d.reclaim_bytes,
        d.recommended_swappiness,
        worker.map(json::quote).unwrap_or_else(|| "null".into()),
        json::strings(errors),
        if processes {
            top_processes(20)
        } else {
            "[]".into()
        }
    )
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
            .map(|(_, pid, name, rss, swap)| format!(
                "{{\"pid\":{pid},\"name\":{},\"rss_bytes\":{rss},\"swap_bytes\":{swap}}}",
                json::quote(name)
            ))
            .collect::<Vec<_>>()
            .join(",")
    )
}

#[derive(Clone)]
struct Control {
    path: PathBuf,
    original: String,
    last: String,
    dev: u64,
    ino: u64,
}
struct Controls {
    entries: BTreeMap<PathBuf, Control>,
}
impl Controls {
    fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }
    fn save(&self) -> Result<()> {
        let text = self
            .entries
            .values()
            .map(|e| {
                format!(
                    "{}\t{}\t{}\t{}\t{}\n",
                    e.path.display(),
                    e.original,
                    e.last,
                    e.dev,
                    e.ino
                )
            })
            .collect::<String>();
        atomic_write(Path::new(CONTROL_FILE), &text, 0o600)
    }
    fn set(&mut self, path: &Path, value: &str, nonblock: bool) -> Result<()> {
        if !valid_control(path) {
            return Err("invalid control path".into());
        }
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
        if let Some(entry) = self.entries.get(path) {
            if old != entry.last {
                return Err(
                    format!("external control change preserved: {}", path.display()).into(),
                );
            }
        }
        self.entries
            .entry(path.to_path_buf())
            .or_insert_with(|| Control {
                path: path.into(),
                original: old.clone(),
                last: old.clone(),
                dev: meta.dev(),
                ino: meta.ino(),
            })
            .last = value.into();
        self.save()?;
        let result = write_control(path, value, nonblock);
        if result.is_err() {
            if let Some(e) = self.entries.get_mut(path) {
                e.last = old;
            }
            self.save()?;
        }
        result
    }
    fn protect(&mut self, path: &Path, bytes: u64) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        let current = read(path)?;
        if current.trim() == "max" {
            return Ok(());
        }
        let current = current.trim().parse::<u64>()?;
        if current < bytes {
            self.set(path, &bytes.to_string(), false)?;
        }
        Ok(())
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
        || (path.starts_with(CGROOT)
            && matches!(
                path.file_name().and_then(|s| s.to_str()),
                Some("memory.low" | "memory.high")
            ))
}
fn write_control(path: &Path, value: &str, nonblock: bool) -> Result<()> {
    let mut o = OpenOptions::new();
    o.write(true);
    if nonblock {
        o.custom_flags(0o4000);
    }
    o.open(path)?.write_all(value.as_bytes())?;
    Ok(())
}
pub fn restore_controls() -> Result<()> {
    let text = match read(CONTROL_FILE) {
        Ok(t) => t,
        Err(_) if !Path::new(CONTROL_FILE).exists() => return Ok(()),
        Err(e) => return Err(e),
    };
    let mut errors = Vec::new();
    for line in text.lines().rev() {
        let v: Vec<_> = line.split('\t').collect();
        if v.len() != 5 {
            continue;
        }
        let p = Path::new(v[0]);
        if !valid_control(p) {
            continue;
        }
        let Ok(m) = fs::metadata(p) else { continue };
        if m.dev().to_string() != v[3] || m.ino().to_string() != v[4] {
            continue;
        }
        if read(p).is_ok_and(|s| s.trim() == v[2]) {
            if let Err(e) = write_control(p, v[1], p.ends_with("memory.high")) {
                errors.push(e.to_string());
            }
        }
    }
    if errors.is_empty() {
        fs::remove_file(CONTROL_FILE)?;
        Ok(())
    } else {
        Err(errors.join("; ").into())
    }
}
fn child_dirs(path: &Path) -> Vec<PathBuf> {
    fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .take(256)
        .collect()
}
fn user_managers() -> Vec<PathBuf> {
    child_dirs(Path::new(CGROOT).join("user.slice").as_path())
        .into_iter()
        .filter_map(|p| {
            let uid = p
                .file_name()?
                .to_str()?
                .strip_prefix("user-")?
                .strip_suffix(".slice")?;
            if !uid.chars().all(|x| x.is_ascii_digit()) {
                return None;
            }
            let manager = p.join(format!("user@{uid}.service"));
            manager.exists().then_some(manager)
        })
        .collect()
}
fn protect_sessions(c: &mut Controls, total: u64, errors: &mut Vec<String>) {
    let reserve = (total / 8).clamp(256 * MIB, 2 * GIB);
    let users = user_managers();
    let mut paths = vec![
        (Path::new(CGROOT).join("user.slice/memory.low"), reserve),
        (
            Path::new(CGROOT).join("system.slice/memory.low"),
            (256 * MIB).min(total / 32),
        ),
    ];
    for manager in &users {
        let share = reserve / users.len().max(1) as u64;
        if let Some(parent) = manager.parent() {
            paths.push((parent.join("memory.low"), share));
        }
        paths.push((manager.join("memory.low"), share));
        paths.push((manager.join("session.slice/memory.low"), share * 3 / 4));
        paths.push((manager.join("app.slice/memory.low"), share / 4));
        for app in child_dirs(&manager.join("app.slice")) {
            let name = app
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase();
            if [
                "ztty",
                "kitty",
                "foot",
                "ssmcp",
                "alacritty",
                "konsole",
                "terminal",
            ]
            .iter()
            .any(|x| name.contains(x))
            {
                paths.push((app.join("memory.low"), (128 * MIB).min(share / 4)));
            }
        }
    }
    for unit in ["sshd.service", "ssh.service", "systemd-logind.service"] {
        paths.push((
            Path::new(CGROOT)
                .join("system.slice")
                .join(unit)
                .join("memory.low"),
            64 * MIB,
        ));
    }
    for (p, n) in paths {
        if let Err(e) = c.protect(&p, n) {
            errors.push(e.to_string());
        }
    }
}
fn background_groups() -> Vec<PathBuf> {
    let mut groups = Vec::new();
    let root = Path::new(CGROOT).join("zmem.slice/zmem-background.slice");
    if root.join("memory.current").exists() {
        groups.push(root);
    }
    for p in user_managers() {
        let p = p.join("zmem.slice/zmem-background.slice");
        if p.join("memory.current").exists() {
            groups.push(p);
        }
    }
    groups
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
fn apply_controls(
    settings: &Settings,
    t: &Telemetry,
    d: &Decision,
    c: &mut Controls,
    protect: bool,
    errors: &mut Vec<String>,
) -> Option<PathBuf> {
    if protect && settings.protect_sessions {
        protect_sessions(c, t.sample.total_bytes, errors);
    }
    if settings.manage_swappiness {
        // Prefer RAM compression while it has space; avoid pushing more data
        // into a saturated compressed device or an already-stalled disk.
        let zram_room = t
            .swaps
            .iter()
            .any(|s| s.path.starts_with("/dev/zram") && s.size.saturating_sub(s.used) > s.size / 5);
        let value = if t.sample.io_full_bp >= 1000 {
            40
        } else if zram_room {
            if settings.profile == "interactive" || settings.profile == "gaming" {
                100
            } else {
                120
            }
        } else {
            d.recommended_swappiness.min(80)
        };
        if let Err(e) = c.set(
            Path::new("/proc/sys/vm/swappiness"),
            &value.to_string(),
            false,
        ) {
            errors.push(e.to_string());
        }
        if !t.zram.is_empty() {
            if let Err(e) = c.set(Path::new("/proc/sys/vm/page-cluster"), "0", false) {
                errors.push(e.to_string());
            }
        }
    }
    let mut groups = background_groups();
    let count = groups.len().max(1) as u64;
    for p in &groups {
        let high = p.join("memory.high");
        let value = if d.background_high_percent == 100 {
            "max".into()
        } else {
            (t.sample.total_bytes / 100 * d.background_high_percent as u64 / count)
                .max(256 * MIB)
                .to_string()
        };
        let result = (|| -> Result<()> {
            let meta = fs::metadata(&high)?;
            let original = match c
                .entries
                .get(&high)
                .filter(|entry| entry.dev == meta.dev() && entry.ino == meta.ino())
            {
                Some(entry) => entry.original.clone(),
                None => read(&high)?.trim().to_owned(),
            };
            c.set(&high, &capped_high(&original, &value)?, true)
        })();
        if let Err(e) = result {
            errors.push(e.to_string());
        }
    }
    groups.sort_unstable_by_key(|p| std::cmp::Reverse(number(p.join("memory.current"))));
    groups.into_iter().next()
}

pub fn ensure_zram(settings: &Settings, total: u64) -> Result<()> {
    if !settings.zram_enabled || swaps()?.iter().any(|s| s.path.starts_with("/dev/zram")) {
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
    command("swapon", &["--priority", "100", "/dev/zram0"], 10)?;
    Ok(())
}
fn valid_swap_file(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_file() || m.file_type().is_symlink() || m.uid() != 0 || m.mode() & 0o077 != 0 {
        return Err(format!(
            "swap file must be root-owned regular mode 0600: {}",
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
                let priority = prio.parse::<i32>()?.clamp(-1, 32767).to_string();
                command("swapon", &["--priority", &priority, path], 10)?;
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
            command(
                "swapon",
                &["--priority", "10", p.to_str().ok_or("invalid path")?],
                10,
            )?;
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
pub fn grow_swap(settings: &Settings, requested: u64) -> Result<()> {
    root_required()?;
    prepare_swap_dir(settings)?;
    let active = swaps()?;
    let disk = active
        .iter()
        .filter(|s| s.kind == "file" || !s.path.starts_with("/dev/zram"))
        .fold(0u64, |n, s| n.saturating_add(s.size));
    let size = requested
        .min(settings.policy.chunk_bytes)
        .min(settings.policy.max_disk_swap_bytes.saturating_sub(disk))
        / 4096
        * 4096;
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
        command("swapon", &["--priority", "10", name], 10)?;
        Ok(())
    })();
    if result.is_err() && swaps().is_ok_and(|v| !v.iter().any(|s| s.path == name)) && path.exists()
    {
        let _ = fs::remove_file(&path);
    }
    result
}
fn shrink_candidate(settings: &Settings, t: &Telemetry) -> Option<String> {
    t.swaps
        .iter()
        .filter(|s| {
            s.kind == "file"
                && s.used == 0
                && Path::new(&s.path).parent() == Some(settings.swap_dir.as_path())
                && owned_name(Path::new(&s.path))
                && t.sample.swap_free_bytes.saturating_sub(s.size)
                    >= settings.policy.grow_free_bytes
        })
        .max_by_key(|s| s.size)
        .map(|s| s.path.clone())
}
pub fn shrink_swap(settings: &Settings, path: &Path) -> Result<()> {
    root_required()?;
    if path.parent() != Some(settings.swap_dir.as_path()) || !owned_name(path) {
        return Err("refusing to remove an unowned swap file".into());
    }
    prepare_swap_dir(settings)?;
    valid_swap_file(path)?;
    let t = Sampler::new().sample()?;
    let reserve = settings
        .policy
        .effective_reserve_bytes(t.sample.total_bytes);
    let s = t
        .swaps
        .iter()
        .find(|s| Path::new(&s.path) == path)
        .ok_or("swap file is not active")?;
    if s.used != 0
        || t.sample.available_bytes < reserve.saturating_mul(2)
        || t.sample.memory_some_bp > 200
        || t.sample.memory_full_bp > 100
        || t.sample.swap_free_bytes.saturating_sub(s.size) < settings.policy.grow_free_bytes
    {
        return Err("swap shrink deferred until unused with ample memory and swap".into());
    }
    command("swapoff", &[path.to_str().ok_or("invalid swap path")?], 20)?;
    if swaps()?.iter().any(|s| Path::new(&s.path) == path) {
        return Err("swap file remains active; preserving it".into());
    }
    fs::remove_file(path)?;
    Ok(())
}
pub fn reclaim(path: &Path, bytes: u64) -> Result<()> {
    root_required()?;
    if !background_groups().iter().any(|p| p == path) {
        return Err("reclaim is limited to registered background groups".into());
    }
    let before = number(path.join("memory.current"));
    let amount = bytes.min(128 * MIB).min(before / 8);
    if amount == 0 {
        return Ok(());
    }
    let result = fs::write(
        path.join("memory.reclaim"),
        format!("{amount} swappiness=0"),
    );
    if let Err(e) = result {
        if e.raw_os_error() != Some(11) {
            return Err(e.into());
        }
    }
    let after = number(path.join("memory.current"));
    println!(
        "{{\"event\":\"reclaim_observed\",\"requested_bytes\":{amount},\"before_bytes\":{before},\"after_bytes\":{after}}}"
    );
    Ok(())
}

struct Worker {
    child: Child,
    kind: String,
    start: Instant,
    termination_sent: bool,
}
impl Worker {
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
fn worker_start(kind: &str, args: &[String], config: &Path) -> Result<Worker> {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.process_group(0).arg(format!("__{kind}")).args(args);
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
        signal(15, stop_signal);
        signal(2, stop_signal);
    }
    let start = Instant::now();
    let mut sampler = Sampler::new();
    let mut policy = Policy::new(settings.policy);
    let mut controls = Controls::new();
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
                        "{{\"event\":\"worker_finished\",\"action\":{},\"success\":{}}}",
                        json::quote(&w.kind),
                        status.success()
                    );
                    worker = None;
                } else if !w.termination_sent && w.start.elapsed() > w.deadline() {
                    w.terminate();
                    errors.push(format!("{} worker exceeded its deadline", w.kind));
                }
            }
            let t = match sampler.sample() {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("zmem sample failed: {e}");
                    if once {
                        return Err(e);
                    }
                    thread::sleep(Duration::from_millis(settings.interval_ms));
                    continue;
                }
            };
            let d = policy.observe(t.sample, start.elapsed().as_secs());
            if !observe {
                let bg = apply_controls(
                    &settings,
                    &t,
                    &d,
                    &mut controls,
                    loop_index % 15 == 0,
                    &mut errors,
                );
                if worker.is_none() {
                    let action = if d.grow_swap_bytes > 0 {
                        Some(("grow", vec![d.grow_swap_bytes.to_string()]))
                    } else if d.shrink_allowed {
                        shrink_candidate(&settings, &t).map(|p| ("shrink", vec![p]))
                    } else if d.reclaim_bytes > 0 {
                        bg.map(|p| {
                            (
                                "reclaim",
                                vec![p.to_string_lossy().into(), d.reclaim_bytes.to_string()],
                            )
                        })
                    } else {
                        None
                    };
                    if let Some((kind, args)) = action {
                        match worker_start(kind, &args, &config) {
                            Ok(w) => worker = Some(w),
                            Err(e) => errors.push(e.to_string()),
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

pub fn run_workload(class: &str, max_mib: Option<u64>, args: &[String]) -> Result<()> {
    if args.is_empty() {
        return Err("run requires a command after --".into());
    }
    if !["background", "interactive", "disposable"].contains(&class) {
        return Err("class must be background, interactive or disposable".into());
    }
    if max_mib.is_some() && class != "disposable" {
        return Err("--max-memory applies to explicit disposable workloads".into());
    }
    let t = Sampler::new().sample()?;
    let total = t.sample.total_bytes;
    let id = format!("zmem-{}-{}", class, std::process::id());
    let mut cmd = Command::new("/usr/bin/systemd-run");
    if root_required().is_err() {
        cmd.arg("--user");
    }
    cmd.args(["--scope", "--quiet", "--collect", "--unit", &id]);
    match class {
        "interactive" => {
            cmd.arg("--slice=app.slice")
                .arg("--property=MemoryLow=256M");
        }
        "background" => {
            cmd.arg("--slice=zmem-background.slice")
                .arg(format!("--property=MemoryHigh={}", total / 2))
                .arg("--property=CPUWeight=50")
                .arg("--property=IOWeight=50");
        }
        _ => {
            let max = max_mib.unwrap_or((total / 4 / MIB).max(128));
            if max < 16 || max > total / MIB {
                return Err("disposable --max-memory must be 16 MiB..physical RAM".into());
            }
            cmd.arg("--slice=zmem-background.slice")
                .arg(format!("--property=MemoryMax={}M", max))
                .arg(format!("--property=MemoryHigh={}M", max * 3 / 4))
                .arg(format!("--property=MemorySwapMax={}M", max))
                .arg("--property=OOMPolicy=kill");
        }
    }
    use std::os::unix::process::CommandExt;
    Err(cmd.arg("--").args(args).exec().into())
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
