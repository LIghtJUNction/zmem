//! Installation and reversible takeover of existing userspace controllers.
//!
//! Live swap devices are adopted without swapoff, mkswap, or a zram reset.
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::Result;

const BASELINE: &str = "/var/lib/zmem/backup";
const BINARY: &str = "/usr/local/bin/zmem";
const CONFIG: &str = "/etc/zmem/zmem.conf";
const SERVICE: &str = "/etc/systemd/system/zmem.service";
const GENERATOR: &str = "/etc/systemd/system-generators/zram-generator";
const ADOPTED: &str = "/var/lib/zmem/adopted-swaps";
const STATUS: &str = "/run/zmem/status.json";
const CONTROLS: &str = "/run/zmem/controls.tsv";
const CONTROL_RECOVERY: &str = "/var/lib/zmem/upgrade-controls.tsv";
const LEGACY_DROPIN_DIR: &str = "/run/systemd/system/zmem.service.d";
const LEGACY_DROPIN: &[u8] = b"[Service]\nKillSignal=SIGKILL\n";
const UNIT: &str = "zmem.service";
// Stop the socket before the service so socket activation cannot restart oomd.
const LEGACY: &[&str] = &[
    "systemd-oomd.socket",
    "systemd-oomd.service",
    "swapspace.service",
    "nohang.service",
    "nohang-desktop.service",
    "earlyoom.service",
];
const SAVED_CONFIGS: &[&str] = &[
    "/etc/swapspace.conf",
    "/etc/systemd/zram-generator.conf",
    "/etc/nohang/nohang.conf",
    "/etc/nohang/nohang-desktop.conf",
    "/etc/default/earlyoom",
    "/etc/systemd/oomd.conf",
];
const SYSCTLS: &[&str] = &[
    "/proc/sys/vm/swappiness",
    "/proc/sys/vm/page-cluster",
    "/proc/sys/vm/watermark_scale_factor",
];
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
enum FileKind {
    Missing,
    Regular,
    Link(PathBuf),
}

#[derive(Clone, Debug)]
struct SavedFile {
    path: PathBuf,
    kind: FileKind,
    mode: u32,
    uid: u32,
    gid: u32,
    index: usize,
}

#[derive(Clone, Debug)]
struct SavedService {
    name: String,
    loaded: bool,
    active: bool,
    enabled: String,
}

#[derive(Debug)]
struct Backup {
    files: Vec<SavedFile>,
    services: Vec<SavedService>,
    sysctls: Vec<(PathBuf, String)>,
}

#[derive(Debug)]
struct Swap {
    path: PathBuf,
    is_file: bool,
    priority: i32,
}

/// Install the running binary and service, optionally taking over legacy
/// userspace controllers. Existing zmem configuration is preserved.
pub fn install(source: Option<&Path>, replace: bool) -> Result<()> {
    require_root()?;
    let systemctl = executable("systemctl")?;
    reject_symlink(Path::new(BINARY))?;
    let current = std::env::current_exe()?;
    if !fs::metadata(&current)?.is_file() {
        return fail("the running zmem executable is not a regular file");
    }
    let mut services = Vec::new();
    for name in LEGACY.iter().copied().chain([UNIT]) {
        services.push(service_state(&systemctl, name)?);
    }
    let conflicts: Vec<_> = services
        .iter()
        .filter(|s| s.name != UNIT && (s.active || s.enabled.starts_with("enabled")))
        .map(|s| s.name.as_str())
        .collect();
    if !replace && !conflicts.is_empty() {
        return fail(format!(
            "existing memory controllers are active or enabled: {}; use zmem install --replace to take them over",
            conflicts.join(", ")
        ));
    }
    if replace {
        verify_swapspace_stop(&systemctl)?;
    }
    // Read the requested source before changing any system file.
    let new_config = if !Path::new(CONFIG).exists() {
        Some(match source {
            Some(path) => read_limited(path, 64 * 1024)?,
            None => include_str!("../packaging/zmem.conf").to_owned(),
        })
    } else {
        None
    };
    for dir in [
        "/usr/local/bin",
        "/etc/zmem",
        "/etc/systemd/system",
        "/etc/systemd/system-generators",
        "/var/lib/zmem",
        "/run/zmem",
    ] {
        ensure_secure_dir(Path::new(dir), 0o755)?;
    }
    let backup = if Path::new(BASELINE).exists() {
        Backup::load()?
    } else {
        Backup::capture(services)?
    };
    if replace {
        for name in LEGACY {
            backup.ensure_mask_safe(&unit_path(name))?;
        }
        backup.ensure_mask_safe(Path::new(GENERATOR))?;
    }
    // Include every live swap before stopping a controller or changing units.
    let swaps_before: BTreeSet<_> = swaps()?.into_iter().map(|s| s.path).collect();
    // This is a separate preflight boundary: a failed stop or restoration must
    // not trigger baseline rollback and start a competing controller.
    stop_manager(&systemctl)?;
    let migration = (|| -> Result<()> {
        copy_atomic(&current, Path::new(BINARY), 0o755)?;
        if let Some(text) = new_config {
            write_atomic(Path::new(CONFIG), text.as_bytes(), 0o644)?;
        }
        write_atomic(
            Path::new(SERVICE),
            include_bytes!("../packaging/zmem.service"),
            0o644,
        )?;
        record_adopted_swaps()?;
        if replace {
            for name in LEGACY {
                replace_with_mask(&unit_path(name))?;
            }
            // Mask future generation only. The active setup/swap units and
            // their zram device are never stopped or reset during takeover.
            replace_with_mask(Path::new(GENERATOR))?;
        }
        checked(&systemctl, &["daemon-reload"])?;
        if replace {
            for name in LEGACY {
                if service_state(&systemctl, name)?.active {
                    checked(&systemctl, &["stop", name])?;
                }
            }
        }
        // Capture any final swapspace allocation made between the initial
        // inventory and its exit, so the next boot can restore every file.
        record_adopted_swaps()?;
        checked(&systemctl, &["enable", UNIT])?;
        checked(&systemctl, &["restart", UNIT])?;
        wait_ready(&systemctl)?;
        let swaps_after: BTreeSet<_> = swaps()?.into_iter().map(|s| s.path).collect();
        let missing: Vec<_> = swaps_before.difference(&swaps_after).collect();
        if !missing.is_empty() {
            return fail(format!(
                "takeover changed active swap devices: {}",
                missing
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        Ok(())
    })();
    if let Err(error) = migration {
        let recovery = restore(&backup, &systemctl);
        return match recovery {
            Ok(()) => fail(format!(
                "zmem installation failed: {error}; original configuration and controller states restored"
            )),
            Err(recovery) => fail(format!(
                "zmem installation failed: {error}; rollback needs attention: {recovery}"
            )),
        };
    }
    println!("zmem is active; configuration: {CONFIG}");
    println!("Existing swap was retained. Restore the original setup with: sudo zmem rollback");
    Ok(())
}

/// Restore the first-install baseline without disabling any active swap.
pub fn rollback() -> Result<()> {
    require_root()?;
    let backup = Backup::load()?;
    let systemctl = executable("systemctl")?;
    restore(&backup, &systemctl)?;
    println!("Original memory-controller configuration restored; active swap retained.");
    Ok(())
}

fn stop_manager(systemctl: &Path) -> Result<()> {
    let recovery = Path::new(CONTROL_RECOVERY);
    let controls = Path::new(CONTROLS);
    let live = if controls.exists() {
        Some(read_limited(controls, 4 * 1024 * 1024)?)
    } else {
        None
    };
    let snapshot = if recovery.exists() {
        Some(read_limited(recovery, 4 * 1024 * 1024)?)
    } else {
        live.clone()
    };
    let state = service_state(systemctl, UNIT)?;
    if !state.loaded {
        if snapshot.is_some() {
            return fail(
                "a zmem control journal exists without a managed service; stop or recover the standalone daemon before installing",
            );
        }
        return Ok(());
    }
    // Version 0.1.0 may delete its journal while skipping page-rounded limits.
    // Persist the captured journal before stopping that writer. A failed
    // recovery retains this file, so a subsequent install cannot lose it.
    if !recovery.exists() {
        if let Some(text) = &snapshot {
            write_atomic(recovery, text.as_bytes(), 0o600)?;
        }
    }
    // Only the live journal identifies the running writer's format. A saved
    // five-column journal may coexist with a newer daemon on a recovery retry.
    let legacy = live
        .as_deref()
        .map(legacy_control_journal)
        .transpose()?
        .unwrap_or(false);
    if legacy {
        stop_legacy_manager(systemctl)?;
    } else {
        stop_unit_and_confirm(systemctl)?;
    }
    // Read after the writer has exited. Keep the final journal until both
    // recoveries succeed; it may contain controls added after our first copy.
    let final_snapshot = if controls.exists() {
        Some(read_limited(controls, 4 * 1024 * 1024)?)
    } else {
        None
    };
    if legacy && final_snapshot.is_none() {
        return fail(format!(
            "the legacy daemon's final control journal is missing; recovery evidence retained at {CONTROL_RECOVERY}"
        ));
    }
    if let Some(text) = &final_snapshot {
        crate::linux::restore_control_snapshot(text)?;
    }
    if let Some(text) = &snapshot {
        crate::linux::restore_control_snapshot(text)?;
    }
    // Five-column logs do not record both sides of an in-flight write. Do not
    // discard their only baseline when ownership cannot be confirmed: a safe
    // skipped restore is not proof that a legacy control reached its baseline.
    for text in final_snapshot.iter().chain(snapshot.iter()) {
        if legacy_control_journal(text)? {
            verify_legacy_controls_restored(text)?;
        }
    }
    if final_snapshot.is_some() {
        fs::remove_file(controls)?;
    }
    if snapshot.is_some() {
        fs::remove_file(recovery)?;
    }
    Ok(())
}

fn legacy_control_journal(text: &str) -> Result<bool> {
    let mut legacy = false;
    for line in text.lines().filter(|line| !line.is_empty()) {
        match line.split('\t').count() {
            5 => legacy = true,
            6 => {}
            _ => return fail("invalid control journal column count; refusing to stop its writer"),
        }
    }
    Ok(legacy)
}

fn stop_unit_and_confirm(systemctl: &Path) -> Result<()> {
    if let Err(error) = checked(systemctl, &["stop", UNIT]) {
        return fail(format!(
            "cannot stop zmem: {error}; no other memory controller was started"
        ));
    }
    let stopped = checked_output(
        systemctl,
        &["show", UNIT, "--property=ActiveState,MainPID", "--no-pager"],
    )?;
    let values = properties(&String::from_utf8(stopped.stdout)?);
    if !matches!(
        values.get("ActiveState").map(String::as_str),
        Some("inactive" | "failed")
    ) || values
        .get("MainPID")
        .and_then(|value| value.parse::<u64>().ok())
        != Some(0)
    {
        return fail(
            "zmem has not fully stopped; the current configuration and other controllers were preserved",
        );
    }
    Ok(())
}

fn legacy_dropin_path() -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Path::new(LEGACY_DROPIN_DIR).join(format!(
        "zzzz-zmem-upgrade-{}-{stamp}.conf",
        std::process::id()
    ))
}

fn stop_legacy_manager(systemctl: &Path) -> Result<()> {
    let directory = Path::new(LEGACY_DROPIN_DIR);
    ensure_secure_dir(directory, 0o755)?;
    let path = legacy_dropin_path();
    // Never replace another drop-in, even if a generated name already exists.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    let stop = (|| -> Result<()> {
        file.write_all(LEGACY_DROPIN)?;
        file.sync_all()?;
        File::open(directory)?.sync_all()?;
        checked(systemctl, &["daemon-reload"])?;
        let output = checked_output(
            systemctl,
            &["show", UNIT, "--property=KillSignal,KillMode", "--no-pager"],
        )?;
        let values = properties(&String::from_utf8(output.stdout)?);
        // Lexically later user overrides can win. Verify the effective value
        // rather than overwriting or renaming any of the user's drop-ins.
        if values.get("KillSignal").map(String::as_str) != Some("9")
            || values.get("KillMode").map(String::as_str) != Some("control-group")
        {
            return fail(
                "legacy zmem shutdown requires effective KillSignal=9 and KillMode=control-group; the daemon was not stopped",
            );
        }
        stop_unit_and_confirm(systemctl)
    })();
    drop(file);
    // Always remove only this invocation's file, including failed writes,
    // reloads, validation, stops and stop-state checks. Reload after removal
    // even when syncing the directory fails, and report every cleanup error.
    let mut cleanup = Vec::new();
    collect(&mut cleanup, remove_file_if_present(&path));
    collect(
        &mut cleanup,
        File::open(directory)
            .and_then(|dir| dir.sync_all())
            .map_err(Into::into),
    );
    collect(&mut cleanup, checked(systemctl, &["daemon-reload"]));
    if !cleanup.is_empty() {
        let original = stop
            .err()
            .map(|error| format!("{error}; "))
            .unwrap_or_default();
        return fail(format!(
            "{original}cannot clean up legacy shutdown override {}: {}; control journals retained",
            path.display(),
            cleanup.join("; ")
        ));
    }
    stop
}

fn verify_legacy_controls_restored(text: &str) -> Result<()> {
    let mut unresolved = BTreeSet::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let fields: Vec<_> = line.split('\t').collect();
        if fields.len() != 5 {
            continue;
        }
        let path = Path::new(fields[0]);
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if metadata.dev().to_string() != fields[3] || metadata.ino().to_string() != fields[4] {
            continue;
        }
        // The original was read from the kernel, so it is already canonical.
        if read_limited(path, 128)?.trim() != fields[1].trim() {
            unresolved.insert(path.display().to_string());
        }
    }
    if !unresolved.is_empty() {
        return fail(format!(
            "legacy controls changed externally or could not be confirmed restored: {}; journals retained at {CONTROLS} and {CONTROL_RECOVERY}",
            unresolved.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    Ok(())
}

fn restore(backup: &Backup, systemctl: &Path) -> Result<()> {
    let mut errors = Vec::new();
    let state = service_state(systemctl, UNIT)?;
    stop_manager(systemctl)?;
    if state.loaded {
        collect(&mut errors, checked(systemctl, &["disable", UNIT]));
    }
    for file in backup.files.iter().rev() {
        collect(&mut errors, file.restore());
    }
    for (path, value) in &backup.sysctls {
        collect(&mut errors, fs::write(path, value).map_err(Into::into));
    }
    collect(&mut errors, checked(systemctl, &["daemon-reload"]));
    for service in &backup.services {
        if !service.loaded {
            continue;
        }
        match service.enabled.as_str() {
            "enabled" => collect(&mut errors, checked(systemctl, &["enable", &service.name])),
            "enabled-runtime" => collect(
                &mut errors,
                checked(systemctl, &["enable", "--runtime", &service.name]),
            ),
            "disabled" => collect(&mut errors, checked(systemctl, &["disable", &service.name])),
            // Static, generated, linked, and already-masked unit definitions
            // are restored by their file records; preserve their link state.
            _ => {}
        }
        if service.active {
            collect(&mut errors, checked(systemctl, &["start", &service.name]));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        fail(errors.join("; "))
    }
}

impl Backup {
    fn capture(services: Vec<SavedService>) -> Result<Self> {
        let parent = Path::new("/var/lib/zmem");
        let staging = unique_path(parent, "backup");
        fs::create_dir(&staging)?;
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))?;
        let result = (|| -> Result<Self> {
            fs::create_dir(staging.join("files"))?;
            fs::set_permissions(staging.join("files"), fs::Permissions::from_mode(0o700))?;
            let mut files = Vec::new();
            for (index, path) in tracked_paths().into_iter().enumerate() {
                files.push(SavedFile::capture(path, index, &staging)?);
            }
            let mut sysctls = Vec::new();
            for name in SYSCTLS {
                let path = PathBuf::from(name);
                if path.exists() {
                    let value = read_limited(&path, 128)?;
                    if value.trim().parse::<u64>().is_err() {
                        return fail(format!("unexpected sysctl contents: {name}"));
                    }
                    sysctls.push((path, value));
                }
            }
            let backup = Self {
                files,
                services,
                sysctls,
            };
            write_atomic(&staging.join("manifest"), backup.encode().as_bytes(), 0o600)?;
            fs::rename(&staging, BASELINE)?;
            File::open(parent)?.sync_all()?;
            Ok(backup)
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&staging);
        }
        result
    }

    fn load() -> Result<Self> {
        ensure_secure_existing_dir(Path::new(BASELINE))?;
        let path = Path::new(BASELINE).join("manifest");
        reject_symlink(&path)?;
        let text = read_limited(&path, 128 * 1024)?;
        let backup = Self::decode(&text)?;
        for file in &backup.files {
            if matches!(file.kind, FileKind::Regular) {
                let data = file.backup_path();
                reject_symlink(&data)?;
                let metadata = fs::metadata(&data)?;
                if !metadata.is_file() || metadata.uid() != 0 {
                    return fail(format!("invalid baseline data: {}", data.display()));
                }
            }
        }
        Ok(backup)
    }

    fn encode(&self) -> String {
        let mut text = String::from("zmem-backup-v1\n");
        for file in &self.files {
            let (kind, target) = match &file.kind {
                FileKind::Missing => ("missing", String::new()),
                FileKind::Regular => ("file", String::new()),
                FileKind::Link(target) => ("link", encode_path(target)),
            };
            text.push_str(&format!(
                "F\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                file.index,
                encode_path(&file.path),
                kind,
                file.mode,
                file.uid,
                file.gid,
                target,
                "1"
            ));
        }
        for service in &self.services {
            text.push_str(&format!(
                "S\t{}\t{}\t{}\t{}\n",
                service.name,
                u8::from(service.loaded),
                u8::from(service.active),
                service.enabled
            ));
        }
        for (path, value) in &self.sysctls {
            text.push_str(&format!("V\t{}\t{}\n", encode_path(path), value.trim()));
        }
        text.push_str("END\n");
        text
    }

    fn decode(text: &str) -> Result<Self> {
        let mut lines = text.lines();
        if lines.next() != Some("zmem-backup-v1") || !text.ends_with("END\n") {
            return fail("zmem backup manifest is missing or incomplete");
        }
        let allowed = tracked_paths();
        let mut files = Vec::new();
        let mut services = Vec::new();
        let mut sysctls = Vec::new();
        let mut seen_paths = BTreeSet::new();
        let mut seen_indices = BTreeSet::new();
        let mut seen_services = BTreeSet::new();
        for line in lines {
            let fields: Vec<_> = line.split('\t').collect();
            match fields.first().copied() {
                Some("F") if fields.len() == 9 && fields[8] == "1" => {
                    let index: usize = fields[1].parse()?;
                    let path = decode_path(fields[2])?;
                    if !allowed.contains(&path)
                        || !seen_paths.insert(path.clone())
                        || !seen_indices.insert(index)
                        || index >= allowed.len()
                    {
                        return fail("unexpected or duplicate file in zmem backup manifest");
                    }
                    let kind = match fields[3] {
                        "missing" => FileKind::Missing,
                        "file" => FileKind::Regular,
                        "link" => FileKind::Link(decode_path(fields[7])?),
                        _ => return fail("invalid file kind in zmem backup manifest"),
                    };
                    files.push(SavedFile {
                        path,
                        kind,
                        index,
                        mode: fields[4].parse()?,
                        uid: fields[5].parse()?,
                        gid: fields[6].parse()?,
                    });
                }
                Some("S") if fields.len() == 5 => {
                    if !(LEGACY.contains(&fields[1]) || fields[1] == UNIT)
                        || !seen_services.insert(fields[1].to_owned())
                        || !matches!(fields[2], "0" | "1")
                        || !matches!(fields[3], "0" | "1")
                    {
                        return fail("invalid service in zmem backup manifest");
                    }
                    services.push(SavedService {
                        name: fields[1].to_owned(),
                        loaded: fields[2] == "1",
                        active: fields[3] == "1",
                        enabled: fields[4].to_owned(),
                    });
                }
                Some("V") if fields.len() == 3 => {
                    let path = decode_path(fields[1])?;
                    if !SYSCTLS.iter().any(|p| path == Path::new(p)) {
                        return fail("unexpected sysctl in zmem backup manifest");
                    }
                    fields[2].parse::<u64>()?;
                    sysctls.push((path, format!("{}\n", fields[2])));
                }
                Some("END") if fields.len() == 1 => {}
                _ => return fail("invalid record in zmem backup manifest"),
            }
        }
        if files.len() != allowed.len() || services.len() != LEGACY.len() + 1 {
            return fail("zmem backup manifest does not contain the complete baseline");
        }
        Ok(Self {
            files,
            services,
            sysctls,
        })
    }

    fn ensure_mask_safe(&self, path: &Path) -> Result<()> {
        let saved = self
            .files
            .iter()
            .find(|f| f.path == path)
            .ok_or_else(|| std::io::Error::other("mask target is absent from baseline"))?;
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let safe = if metadata.file_type().is_symlink() {
            let target = fs::read_link(path)?;
            target == Path::new("/dev/null")
                || matches!(&saved.kind, FileKind::Link(original) if *original == target)
        } else if metadata.is_file() && matches!(saved.kind, FileKind::Regular) {
            read_limited_bytes(path, 1024 * 1024)?
                == read_limited_bytes(&saved.backup_path(), 1024 * 1024)?
        } else {
            false
        };
        if safe {
            Ok(())
        } else {
            fail(format!(
                "refusing to overwrite a changed, unbacked mask target: {}",
                path.display()
            ))
        }
    }
}

impl SavedFile {
    fn capture(path: PathBuf, index: usize, staging: &Path) -> Result<Self> {
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    path,
                    kind: FileKind::Missing,
                    mode: 0,
                    uid: 0,
                    gid: 0,
                    index,
                });
            }
            Err(error) => return Err(error.into()),
        };
        let kind = if metadata.file_type().is_symlink() {
            FileKind::Link(fs::read_link(&path)?)
        } else if metadata.is_file() {
            copy_atomic(&path, &staging.join("files").join(index.to_string()), 0o600)?;
            FileKind::Regular
        } else {
            return fail(format!(
                "refusing to replace a special file or directory: {}",
                path.display()
            ));
        };
        Ok(Self {
            path,
            kind,
            index,
            mode: metadata.mode() & 0o7777,
            uid: metadata.uid(),
            gid: metadata.gid(),
        })
    }

    fn backup_path(&self) -> PathBuf {
        Path::new(BASELINE)
            .join("files")
            .join(self.index.to_string())
    }

    fn restore(&self) -> Result<()> {
        match &self.kind {
            FileKind::Missing => remove_file_if_present(&self.path),
            FileKind::Regular => {
                remove_symlink_if_present(&self.path)?;
                copy_atomic(&self.backup_path(), &self.path, self.mode)?;
                set_owner(&self.path, self.uid, self.gid)?;
                fs::set_permissions(&self.path, fs::Permissions::from_mode(self.mode))?;
                Ok(())
            }
            FileKind::Link(target) => {
                symlink_atomic(target, &self.path)?;
                set_owner(&self.path, self.uid, self.gid)
            }
        }
    }
}

fn tracked_paths() -> Vec<PathBuf> {
    [BINARY, CONFIG, SERVICE, GENERATOR, ADOPTED]
        .into_iter()
        .map(PathBuf::from)
        .chain(LEGACY.iter().map(|name| unit_path(name)))
        .chain(SAVED_CONFIGS.iter().map(PathBuf::from))
        .collect()
}

fn unit_path(name: &str) -> PathBuf {
    Path::new("/etc/systemd/system").join(name)
}

fn service_state(systemctl: &Path, name: &str) -> Result<SavedService> {
    let output = checked_output(
        systemctl,
        &[
            "show",
            name,
            "--property=LoadState,ActiveState,UnitFileState",
            "--no-pager",
        ],
    )?;
    let values = properties(&String::from_utf8(output.stdout)?);
    let loaded = values.get("LoadState").is_some_and(|s| s != "not-found");
    let active = matches!(
        values.get("ActiveState").map(String::as_str),
        Some("active" | "activating" | "reloading")
    );
    Ok(SavedService {
        name: name.to_owned(),
        loaded,
        active,
        enabled: values.get("UnitFileState").cloned().unwrap_or_default(),
    })
}

fn properties(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

fn verify_swapspace_stop(systemctl: &Path) -> Result<()> {
    let output = checked_output(
        systemctl,
        &[
            "show",
            "swapspace.service",
            "--property=MainPID,ExecStart",
            "--no-pager",
        ],
    )?;
    let values = properties(&String::from_utf8(output.stdout)?);
    if values
        .get("ExecStart")
        .is_some_and(|s| has_paranoid_argument(s.split_whitespace()))
    {
        return fail(
            "swapspace has a paranoid/erase option; stopping it could drain active swap, so takeover was not started",
        );
    }
    if let Some(pid) = values
        .get("MainPID")
        .and_then(|p| p.parse::<u32>().ok())
        .filter(|p| *p != 0)
    {
        let cmdline = fs::read(format!("/proc/{pid}/cmdline"))?;
        let arguments: Vec<_> = cmdline
            .split(|byte| *byte == 0)
            .map(String::from_utf8_lossy)
            .collect();
        if has_paranoid_argument(arguments.iter().map(|s| s.as_ref())) {
            return fail("running swapspace has a paranoid/erase option; takeover was not started");
        }
    }
    let config = Path::new("/etc/swapspace.conf");
    if config.exists() && paranoid_config(&read_limited(config, 256 * 1024)?) {
        return fail(
            "swapspace.conf enables paranoid cleanup; takeover was not started because active swap must be retained",
        );
    }
    Ok(())
}

fn has_paranoid_argument<'a>(arguments: impl Iterator<Item = &'a str>) -> bool {
    arguments.into_iter().any(|arg| {
        let arg = arg.trim_matches(|c: char| matches!(c, ';' | '}' | '\'' | '"'));
        matches!(arg, "--paranoid" | "--erase")
            || arg.starts_with("--paranoid=")
            || arg.starts_with("--erase=")
            || (arg.starts_with('-')
                && !arg.starts_with("--")
                && arg[1..].bytes().any(|b| b == b'P' || b == b'e'))
    })
}

fn paranoid_config(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((key, value)) = line.split_once('=') else {
            return false;
        };
        key.trim().eq_ignore_ascii_case("paranoid")
            && matches!(
                value
                    .trim()
                    .trim_matches(['\'', '"'])
                    .to_ascii_lowercase()
                    .as_str(),
                "true" | "yes" | "on" | "1"
            )
    })
}

fn record_adopted_swaps() -> Result<()> {
    let mut adopted = BTreeMap::new();
    if Path::new(ADOPTED).exists() {
        reject_symlink(Path::new(ADOPTED))?;
        for line in read_limited(Path::new(ADOPTED), 128 * 1024)?.lines() {
            let (priority, path) = line
                .split_once('\t')
                .ok_or_else(|| std::io::Error::other("invalid adopted-swaps record"))?;
            let path = PathBuf::from(path);
            if path.exists() {
                if !supported_legacy_swap(&path) {
                    eprintln!(
                        "retaining {} as an unmanaged swap; boot adoption supports direct numeric swapspace files",
                        path.display()
                    );
                    continue;
                }
                validate_legacy_swap(&path)?;
                adopted.insert(path, priority.parse::<i32>()?);
            }
        }
    }
    for swap in swaps()? {
        if swap.is_file && swap.path.starts_with("/var/lib/swapspace/") {
            if !supported_legacy_swap(&swap.path) {
                eprintln!(
                    "retaining active {} as an unmanaged swap; boot adoption supports direct numeric swapspace files",
                    swap.path.display()
                );
                continue;
            }
            validate_legacy_swap(&swap.path)?;
            adopted.insert(swap.path, swap.priority);
        }
    }
    let mut text = String::new();
    for (path, priority) in adopted {
        let path = path
            .to_str()
            .ok_or_else(|| std::io::Error::other("legacy swap path is not UTF-8"))?;
        if path.contains(['\n', '\r', '\t']) {
            return fail("legacy swap path cannot be stored in the adoption manifest");
        }
        text.push_str(&format!("{priority}\t{path}\n"));
    }
    write_atomic(Path::new(ADOPTED), text.as_bytes(), 0o600)
}

fn validate_legacy_swap(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || !supported_legacy_swap(path)
        || path.components().any(|c| matches!(c, Component::ParentDir))
    {
        return fail("adopted swap must be a direct numeric file under /var/lib/swapspace");
    }
    let canonical = fs::canonicalize(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if canonical != path || !metadata.is_file() || metadata.uid() != 0 {
        return fail(format!(
            "legacy swap must be a root-owned regular file without symlinks: {}",
            path.display()
        ));
    }
    Ok(())
}

fn supported_legacy_swap(path: &Path) -> bool {
    path.parent() == Some(Path::new("/var/lib/swapspace"))
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()))
}

fn swaps() -> Result<Vec<Swap>> {
    let text = read_limited(Path::new("/proc/swaps"), 1024 * 1024)?;
    text.lines()
        .skip(1)
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 5 {
                return fail("malformed /proc/swaps entry");
            }
            Ok(Swap {
                path: decode_proc_path(fields[0])?,
                is_file: fields[1] == "file",
                priority: fields[4].parse()?,
            })
        })
        .collect()
}

fn decode_proc_path(text: &str) -> Result<PathBuf> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\'
            && index + 3 < bytes.len()
            && bytes[index + 1..index + 4]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            let value = ((bytes[index + 1] - b'0') as u16) * 64
                + ((bytes[index + 2] - b'0') as u16) * 8
                + (bytes[index + 3] - b'0') as u16;
            if value == 0 || value > 255 {
                return fail("invalid escaped swap path");
            }
            decoded.push(value as u8);
            index += 4;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let path = PathBuf::from(OsString::from_vec(decoded));
    if !path.is_absolute() {
        return fail("swap path must be absolute");
    }
    Ok(path)
}

fn wait_ready(systemctl: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut observed = String::new();
    while Instant::now() < deadline {
        let output = checked_output(
            systemctl,
            &["show", UNIT, "--property=ActiveState,MainPID", "--no-pager"],
        )?;
        let values = properties(&String::from_utf8(output.stdout)?);
        observed = values.get("ActiveState").cloned().unwrap_or_default();
        let pid = values
            .get("MainPID")
            .and_then(|p| p.parse::<u64>().ok())
            .unwrap_or(0);
        if observed == "active" && pid != 0 {
            if let Ok(metadata) = fs::symlink_metadata(STATUS) {
                if metadata.is_file() && metadata.uid() == 0 {
                    if let Ok(status) = read_limited(Path::new(STATUS), 256 * 1024) {
                        if json_u64(&status, "pid") == Some(pid) {
                            return Ok(());
                        }
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(250));
    }
    fail(format!(
        "zmem did not produce status for its active PID within 20 seconds (state={observed}); inspect journalctl -u zmem"
    ))
}

fn json_u64(text: &str, key: &str) -> Option<u64> {
    let marker = format!("\"{key}\"");
    let rest = text.get(text.find(&marker)? + marker.len()..)?.trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let end = rest.bytes().take_while(u8::is_ascii_digit).count();
    if end == 0 {
        return None;
    }
    let tail = rest[end..].trim_start();
    if !tail.starts_with([',', '}']) {
        return None;
    }
    rest[..end].parse().ok()
}

fn require_root() -> Result<()> {
    let status = read_limited(Path::new("/proc/self/status"), 64 * 1024)?;
    let effective = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|line| line.split_whitespace().nth(1));
    if effective == Some("0") {
        Ok(())
    } else {
        fail("installation requires root; run sudo zmem install --replace")
    }
}

fn executable(name: &str) -> Result<PathBuf> {
    for directory in ["/usr/bin", "/usr/sbin", "/bin", "/sbin"] {
        let path = Path::new(directory).join(name);
        if path.is_file() {
            return Ok(path);
        }
    }
    fail(format!("required system executable is missing: {name}"))
}

fn checked(program: &Path, args: &[&str]) -> Result<()> {
    checked_output(program, args).map(|_| ())
}

fn checked_output(program: &Path, args: &[&str]) -> Result<Output> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir("/")
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .env("LC_ALL", "C")
        .env("SYSTEMD_COLORS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        if child.try_wait()?.is_some() {
            let output = child.wait_with_output()?;
            if output.status.success() {
                return Ok(output);
            }
            return fail(format!(
                "{} {} failed: {}",
                program.display(),
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output()?;
            return fail(format!(
                "{} {} timed out after 30 seconds: {}",
                program.display(),
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn ensure_secure_dir(path: &Path, mode: u32) -> Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        if !current.exists() {
            fs::create_dir(&current)?;
            fs::set_permissions(&current, fs::Permissions::from_mode(0o755))?;
        }
        ensure_secure_existing_dir(&current)?;
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn ensure_secure_existing_dir(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return fail(format!(
            "expected a root-owned directory without writable group/other permissions: {}",
            path.display()
        ));
    }
    Ok(())
}

fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            fail(format!("refusing symlink target: {}", path.display()))
        }
        Ok(metadata) if !metadata.is_file() => {
            fail(format!("expected regular file: {}", path.display()))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn unique_path(parent: &Path, name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    parent.join(format!(".{name}-{}-{stamp}", std::process::id()))
}

fn copy_atomic(source: &Path, target: &Path, mode: u32) -> Result<()> {
    reject_symlink(target)?;
    let parent = target
        .parent()
        .ok_or_else(|| std::io::Error::other("missing target directory"))?;
    let temporary = unique_path(parent, "zmem-file");
    let result = (|| -> Result<()> {
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temporary)?;
        std::io::copy(&mut File::open(source)?, &mut output)?;
        output.set_permissions(fs::Permissions::from_mode(mode))?;
        output.sync_all()?;
        reject_symlink(target)?;
        fs::rename(&temporary, target)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn write_atomic(target: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    reject_symlink(target)?;
    let parent = target
        .parent()
        .ok_or_else(|| std::io::Error::other("missing target directory"))?;
    let temporary = unique_path(parent, "zmem-file");
    let result = (|| -> Result<()> {
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temporary)?;
        output.write_all(bytes)?;
        output.set_permissions(fs::Permissions::from_mode(mode))?;
        output.sync_all()?;
        reject_symlink(target)?;
        fs::rename(&temporary, target)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn replace_with_mask(path: &Path) -> Result<()> {
    symlink_atomic(Path::new("/dev/null"), path)
}

fn symlink_atomic(target: &Path, path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("missing link directory"))?;
    let temporary = unique_path(parent, "zmem-link");
    let result = (|| -> Result<()> {
        symlink(target, &temporary)?;
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn remove_file_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn remove_symlink_if_present(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn set_owner(path: &Path, uid: u32, gid: u32) -> Result<()> {
    let owner = format!("{uid}:{gid}");
    let path = path
        .to_str()
        .ok_or_else(|| std::io::Error::other("non-UTF8 system path"))?;
    checked(&executable("chown")?, &["-h", &owner, "--", path])
}

fn read_limited_bytes(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return fail(format!("file exceeds size limit: {}", path.display()));
    }
    Ok(bytes)
}

fn read_limited(path: &Path, limit: u64) -> Result<String> {
    Ok(String::from_utf8(read_limited_bytes(path, limit)?)?)
}

fn encode_path(path: &Path) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut text = String::with_capacity(path.as_os_str().as_bytes().len() * 2);
    for byte in path.as_os_str().as_bytes() {
        text.push(HEX[(byte >> 4) as usize] as char);
        text.push(HEX[(byte & 15) as usize] as char);
    }
    text
}

fn decode_path(text: &str) -> Result<PathBuf> {
    if text.len() % 2 != 0 {
        return fail("invalid path encoding in zmem backup");
    }
    let bytes: std::result::Result<Vec<_>, _> = text
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap_or("!"), 16))
        .collect();
    let bytes = bytes?;
    if bytes.contains(&0) {
        return fail("NUL in zmem backup path");
    }
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

fn collect(errors: &mut Vec<String>, result: Result<()>) {
    if let Err(error) = result {
        errors.push(error.to_string());
    }
}

fn fail<T>(message: impl Into<String>) -> Result<T> {
    Err(std::io::Error::other(message.into()).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_format_preserves_empty_pending_column_and_rejects_unknown_rows() {
        let legacy = "/sys/fs/cgroup/example/memory.high\tmax\t12345\t10\t20\n";
        let committed = "/sys/fs/cgroup/example/memory.high\tmax\t12288\t10\t20\t\n";
        let pending = "/sys/fs/cgroup/example/memory.high\tmax\t12288\t10\t20\t8192\n";
        assert!(legacy_control_journal(legacy).unwrap());
        assert!(!legacy_control_journal(committed).unwrap());
        assert!(!legacy_control_journal(pending).unwrap());
        assert!(!legacy_control_journal("").unwrap());
        assert!(legacy_control_journal(&format!("{committed}{legacy}")).unwrap());
        assert!(legacy_control_journal("/sys/fs/cgroup/example/memory.high\tmax\n").is_err());
    }

    #[test]
    fn paranoid_stop_detection_handles_combined_flags_and_ignores_comments() {
        assert!(has_paranoid_argument(
            ["/usr/bin/swapspace", "-vP"].into_iter()
        ));
        assert!(has_paranoid_argument(["--paranoid"].into_iter()));
        assert!(has_paranoid_argument(["--erase"].into_iter()));
        assert!(!has_paranoid_argument(
            ["/usr/bin/swapspace", "-v"].into_iter()
        ));
        assert!(paranoid_config("paranoid = 'yes' # clean on exit"));
        assert!(!paranoid_config("# paranoid=true\nparanoid=false"));
    }

    #[test]
    fn proc_swap_paths_are_unescaped_before_identity_comparison() {
        assert_eq!(
            decode_proc_path("/var/lib/swap\\040files/a\\134b").unwrap(),
            Path::new("/var/lib/swap files/a\\b")
        );
        assert!(decode_proc_path("relative/swap").is_err());
        assert!(decode_proc_path("/bad\\000path").is_err());
    }

    #[test]
    fn status_pid_requires_numeric_json_value_with_a_delimiter() {
        assert_eq!(
            json_u64("{\"pid\": 123, \"state\":\"healthy\"}", "pid"),
            Some(123)
        );
        assert_eq!(json_u64("{\"pid\":123}", "pid"), Some(123));
        assert_eq!(json_u64("{\"pid\":123x}", "pid"), None);
        assert_eq!(json_u64("{\"pid\":\"123\"}", "pid"), None);
    }

    #[test]
    fn backup_path_encoding_preserves_whitespace_and_symlink_targets() {
        for path in [
            Path::new("/etc/a b"),
            Path::new("../usr/lib/example"),
            Path::new("/dev/null"),
        ] {
            assert_eq!(decode_path(&encode_path(path)).unwrap(), path);
        }
        assert!(decode_path("0").is_err());
        assert!(decode_path("zz").is_err());
        assert!(decode_path("00").is_err());
    }

    #[test]
    fn partial_backup_cannot_be_used_for_rollback() {
        assert!(Backup::decode("zmem-backup-v1\nEND\n").is_err());
        assert!(Backup::decode("zmem-backup-v1\n").is_err());
    }

    #[test]
    fn persistent_baseline_retains_restore_targets_and_service_states() {
        let files = tracked_paths()
            .into_iter()
            .enumerate()
            .map(|(index, path)| SavedFile {
                kind: if path == Path::new(GENERATOR) {
                    FileKind::Link(PathBuf::from(
                        "/usr/lib/systemd/system-generators/zram-generator",
                    ))
                } else {
                    FileKind::Missing
                },
                path,
                index,
                mode: 0o644,
                uid: 0,
                gid: 0,
            })
            .collect();
        let services = LEGACY
            .iter()
            .copied()
            .chain([UNIT])
            .map(|name| SavedService {
                name: name.to_owned(),
                loaded: true,
                active: name == "swapspace.service",
                enabled: if name == "swapspace.service" {
                    "enabled"
                } else {
                    "disabled"
                }
                .to_owned(),
            })
            .collect();
        let original = Backup {
            files,
            services,
            sysctls: vec![(PathBuf::from(SYSCTLS[0]), "150\n".to_owned())],
        };
        let restored = Backup::decode(&original.encode()).unwrap();
        assert_eq!(restored.files.len(), tracked_paths().len());
        assert!(
            restored
                .services
                .iter()
                .any(|s| s.name == "swapspace.service" && s.active && s.enabled == "enabled")
        );
        assert_eq!(restored.sysctls[0].1, "150\n");
        assert!(
            restored
                .files
                .iter()
                .any(|f| f.path == Path::new(GENERATOR) && matches!(f.kind, FileKind::Link(_)))
        );
        let redirected = original.encode().replace(
            &encode_path(Path::new(BINARY)),
            &encode_path(Path::new("/etc/passwd")),
        );
        assert!(Backup::decode(&redirected).is_err());
    }

    #[test]
    fn legacy_adoption_matches_the_boot_restorer_contract() {
        assert!(supported_legacy_swap(Path::new("/var/lib/swapspace/123")));
        assert!(!supported_legacy_swap(Path::new(
            "/var/lib/swapspace/named"
        )));
        assert!(!supported_legacy_swap(Path::new(
            "/var/lib/swapspace/sub/123"
        )));
        assert!(!supported_legacy_swap(Path::new(
            "/var/lib/swapspace/../123"
        )));
    }
}
