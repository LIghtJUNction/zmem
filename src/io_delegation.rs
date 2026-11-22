//! Additive user I/O delegation, separate from the immutable v1 install baseline.
//!
//! Only a newly created drop-in is owned. The write-ahead receipt pins the
//! prepared file's identity before a create-only hard link publishes it.
//! A rollback compares both identity and exact bytes before removing anything.

use crate::{Result, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const DROPIN: &str = "/etc/systemd/system/user@.service.d/50-zmem-io.conf";
const JOURNAL: &str = "/var/lib/zmem/io-delegation.json";
const CONTENT: &str = "# Managed by zmem: append I/O delegation without resetting other controllers.\n[Service]\nDelegate=io\nIOAccounting=yes\n";
const SCHEMA: &str = "zmem.io-delegation.v1";

#[derive(Clone, Debug, Eq, PartialEq)]
struct Receipt {
    dev: u64,
    ino: u64,
    staging_name: String,
    installed_bytes: String,
}

impl Receipt {
    fn encode(&self) -> String {
        format!(
            "{}\n",
            json::object([
                ("schema", SCHEMA.into()),
                ("dev", self.dev.into()),
                ("ino", self.ino.into()),
                ("staging_name", self.staging_name.clone().into()),
                ("installed_bytes", self.installed_bytes.clone().into()),
            ])
        )
    }

    fn decode(text: &str) -> Result<Self> {
        let value = json::parse(text)?;
        if value.get("schema").and_then(json::Value::as_str) != Some(SCHEMA) {
            return Err("unrecognized I/O delegation ownership receipt; preserving files".into());
        }
        let staging_name = value
            .get("staging_name")
            .and_then(json::Value::as_str)
            .ok_or("missing I/O staging name")?;
        if !staging_name
            .strip_prefix(".zmem-io-")
            .and_then(|name| name.strip_suffix(".tmp"))
            .is_some_and(|name| {
                !name.is_empty()
                    && name.len() <= 80
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || byte == b'-')
            })
        {
            return Err("invalid I/O delegation staging name; preserving files".into());
        }
        let receipt = Self {
            dev: value
                .get("dev")
                .and_then(json::Value::as_u64)
                .ok_or("missing I/O receipt device")?,
            ino: value
                .get("ino")
                .and_then(json::Value::as_u64)
                .ok_or("missing I/O receipt inode")?,
            staging_name: staging_name.into(),
            installed_bytes: value
                .get("installed_bytes")
                .and_then(json::Value::as_str)
                .ok_or("missing I/O receipt contents")?
                .into(),
        };
        if receipt.installed_bytes != CONTENT || receipt.encode() != text {
            return Err("changed I/O delegation ownership receipt; preserving files".into());
        }
        Ok(receipt)
    }
}

pub struct Change {
    created: Option<Receipt>,
    state: &'static str,
}

pub struct Withdrawal {
    removed: bool,
    preserved_external: bool,
}

impl Withdrawal {
    fn unchanged() -> Self {
        Self {
            removed: false,
            preserved_external: false,
        }
    }

    pub fn report(&self) {
        println!(
            "{{\"event\":\"user_io_delegation_withdrawal\",\"dropin\":{},\"removed\":{},\"preserved_external\":{}}}",
            json::quote(DROPIN),
            self.removed,
            self.preserved_external,
        );
    }
}

struct Layout {
    dropin: PathBuf,
    journal: PathBuf,
    trusted_root: PathBuf,
    owner: u32,
}

struct OpenReceipt {
    value: Receipt,
    file: File,
}

impl Layout {
    fn system() -> Self {
        Self {
            dropin: DROPIN.into(),
            journal: JOURNAL.into(),
            trusted_root: "/".into(),
            owner: 0,
        }
    }

    fn secure_parent(&self, path: &Path, create: bool) -> Result<()> {
        let parent = path.parent().ok_or("missing I/O delegation parent")?;
        let mut current = self.trusted_root.clone();
        self.check_directory(&current)?;
        for component in parent.strip_prefix(&current)?.components() {
            if !matches!(component, Component::Normal(_)) {
                return Err("invalid I/O delegation directory".into());
            }
            current.push(component);
            if create && !current.try_exists()? {
                fs::DirBuilder::new().mode(0o755).create(&current)?;
            }
            self.check_directory(&current)?;
        }
        Ok(())
    }

    fn check_directory(&self, path: &Path) -> Result<()> {
        let meta = fs::symlink_metadata(path)?;
        if !meta.is_dir() || meta.uid() != self.owner || meta.mode() & 0o022 != 0 {
            return Err(format!("unsafe I/O delegation directory: {}", path.display()).into());
        }
        Ok(())
    }

    fn staging(&self, receipt: &Receipt) -> PathBuf {
        self.dropin.parent().unwrap().join(&receipt.staging_name)
    }

    fn exists(path: &Path) -> Result<bool> {
        match fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn read_receipt(&self) -> Result<Option<OpenReceipt>> {
        if !Self::exists(&self.journal)? {
            return Ok(None);
        }
        self.secure_parent(&self.journal, false)?;
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(0o400000)
            .open(&self.journal)?;
        let meta = file.metadata()?;
        if !meta.is_file()
            || meta.uid() != self.owner
            || meta.mode() & 0o7777 != 0o600
            || meta.nlink() != 1
        {
            return Err("unsafe I/O delegation ownership receipt; preserving files".into());
        }
        let mut text = String::new();
        (&mut file).take(4097).read_to_string(&mut text)?;
        if text.len() > 4096 {
            return Err("oversized I/O delegation ownership receipt".into());
        }
        Ok(Some(OpenReceipt {
            value: Receipt::decode(&text)?,
            file,
        }))
    }

    fn owned(&self, path: &Path, receipt: &Receipt, links: u64) -> Result<bool> {
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if !meta.is_file()
            || meta.uid() != self.owner
            || meta.mode() & 0o7777 != 0o644
            || meta.nlink() != links
            || (meta.dev(), meta.ino()) != (receipt.dev, receipt.ino)
        {
            return Ok(false);
        }
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(0o400000)
            .open(path)?;
        let opened = file.metadata()?;
        if (opened.dev(), opened.ino()) != (receipt.dev, receipt.ino) {
            return Ok(false);
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(CONTENT.len() as u64 + 1)
            .read_to_end(&mut bytes)?;
        Ok(bytes == receipt.installed_bytes.as_bytes())
    }

    fn remove_stage(&self, receipt: &Receipt) -> Result<bool> {
        let staging = self.staging(receipt);
        let own = self.owned(&staging, receipt, 1)?
            || (self.owned(&staging, receipt, 2)? && self.owned(&self.dropin, receipt, 2)?);
        if own {
            fs::remove_file(&staging)?;
            File::open(staging.parent().unwrap())?.sync_all()?;
            Ok(false)
        } else {
            Self::exists(&staging)
        }
    }

    fn remove_receipt(&self, opened: &OpenReceipt) -> Result<()> {
        let current = self
            .read_receipt()?
            .ok_or("I/O receipt disappeared during withdrawal")?;
        let before = opened.file.metadata()?;
        let after = current.file.metadata()?;
        if (before.dev(), before.ino()) != (after.dev(), after.ino())
            || current.value != opened.value
        {
            return Err("I/O receipt changed during withdrawal; preserving it".into());
        }
        fs::remove_file(&self.journal)?;
        File::open(self.journal.parent().unwrap())?.sync_all()?;
        Ok(())
    }

    fn withdraw(&self, expected: Option<&Receipt>) -> Result<Withdrawal> {
        let Some(opened) = self.read_receipt()? else {
            return Ok(Withdrawal::unchanged());
        };
        if expected.is_some_and(|expected| *expected != opened.value) {
            return Err(
                "I/O delegation owner changed after this transaction; preserving files".into(),
            );
        }
        self.secure_parent(&self.dropin, false)?;
        let mut preserved_external = self.remove_stage(&opened.value)?;
        let removed = if self.owned(&self.dropin, &opened.value, 1)? {
            fs::remove_file(&self.dropin)?;
            File::open(self.dropin.parent().unwrap())?.sync_all()?;
            true
        } else {
            preserved_external |= Self::exists(&self.dropin)?;
            false
        };
        // Relinquish ownership of any externally changed file. A later install
        // will see a pre-existing unowned drop-in and leave it untouched.
        self.remove_receipt(&opened)?;
        Ok(Withdrawal {
            removed,
            preserved_external,
        })
    }

    fn publish(&self, receipt: &Receipt) -> Result<()> {
        let staging = self.staging(receipt);
        if !self.owned(&staging, receipt, 1)? {
            return Err("prepared I/O delegation file changed before publication".into());
        }
        // Unlike rename(), hard_link() is create-only. The temporary name is in
        // the same directory/filesystem and does not end in .conf, so systemd
        // cannot consume a partial file. The receipt already identifies it.
        fs::hard_link(&staging, &self.dropin)?;
        File::open(self.dropin.parent().unwrap())?.sync_all()?;
        if self.remove_stage(receipt)? {
            return Err("I/O delegation staging file was externally replaced".into());
        }
        Ok(())
    }

    fn apply(&self) -> Result<Change> {
        self.apply_with(|| Ok(()))
    }

    fn apply_with(&self, before_publish: impl FnOnce() -> Result<()>) -> Result<Change> {
        self.secure_parent(&self.journal, true)?;
        self.secure_parent(&self.dropin, true)?;
        if let Some(opened) = self.read_receipt()? {
            if self.owned(&self.dropin, &opened.value, 1)?
                || (self.owned(&self.dropin, &opened.value, 2)?
                    && self.owned(&self.staging(&opened.value), &opened.value, 2)?)
            {
                self.remove_stage(&opened.value)?;
                return Ok(Change {
                    created: None,
                    state: "already_owned",
                });
            }
            if Self::exists(&self.dropin)? {
                return Ok(Change {
                    created: None,
                    state: "external_file_preserved",
                });
            }
            if self.owned(&self.staging(&opened.value), &opened.value, 1)? {
                if let Err(error) = before_publish().and_then(|()| self.publish(&opened.value)) {
                    return match self.withdraw(Some(&opened.value)) {
                        Ok(_) => Err(error),
                        Err(cleanup) => Err(format!(
                            "{error}; I/O delegation cleanup needs attention: {cleanup}"
                        )
                        .into()),
                    };
                }
                return Ok(Change {
                    created: Some(opened.value),
                    state: "recovered_prepared_file",
                });
            }
            if Self::exists(&self.staging(&opened.value))? {
                return Ok(Change {
                    created: None,
                    state: "external_file_preserved",
                });
            }
            self.remove_receipt(&opened)?;
        }
        if Self::exists(&self.dropin)? {
            return Ok(Change {
                created: None,
                state: "external_file_preserved",
            });
        }
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let staging_name = format!(".zmem-io-{}-{stamp}.tmp", std::process::id());
        let staging = self.dropin.parent().unwrap().join(&staging_name);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&staging)?;
        let meta = file.metadata()?;
        let receipt = Receipt {
            dev: meta.dev(),
            ino: meta.ino(),
            staging_name,
            installed_bytes: CONTENT.into(),
        };
        let mut journal_file: Option<File> = None;
        let operation = (|| -> Result<()> {
            file.set_permissions(fs::Permissions::from_mode(0o644))?;
            file.write_all(CONTENT.as_bytes())?;
            file.sync_all()?;
            let mut journal = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&self.journal)?;
            journal_file = Some(journal.try_clone()?);
            journal.set_permissions(fs::Permissions::from_mode(0o600))?;
            journal.write_all(receipt.encode().as_bytes())?;
            journal.sync_all()?;
            File::open(self.journal.parent().unwrap())?.sync_all()?;
            before_publish()?;
            self.publish(&receipt)
        })();
        if let Err(error) = operation {
            // If publication started, the complete receipt is authoritative.
            // Otherwise clean only the inode created in this call; an external
            // pre-existing receipt is never removed.
            let cleanup = if self
                .read_receipt()
                .is_ok_and(|opened| opened.is_some_and(|opened| opened.value == receipt))
            {
                self.withdraw(Some(&receipt)).map(|_| ())
            } else {
                (|| -> Result<()> {
                    let current = fs::symlink_metadata(&staging)?;
                    if (current.dev(), current.ino()) != (receipt.dev, receipt.ino)
                        || current.nlink() != 1
                    {
                        return Err("changed I/O staging file retained".into());
                    }
                    fs::remove_file(&staging)?;
                    if let Some(journal) = &journal_file {
                        let expected = journal.metadata()?;
                        let current = fs::symlink_metadata(&self.journal)?;
                        if (expected.dev(), expected.ino()) != (current.dev(), current.ino()) {
                            return Err("changed partial I/O receipt retained".into());
                        }
                        fs::remove_file(&self.journal)?;
                    }
                    Ok(())
                })()
            };
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(format!(
                    "{error}; I/O delegation cleanup needs attention: {cleanup}"
                )
                .into()),
            };
        }
        Ok(Change {
            created: Some(receipt),
            state: "installed",
        })
    }

    fn undo_created(&self, change: Option<&Change>) -> Result<Withdrawal> {
        match change.and_then(|change| change.created.as_ref()) {
            Some(receipt) => self.withdraw(Some(receipt)),
            None => Ok(Withdrawal::unchanged()),
        }
    }
}

pub fn install() -> Result<Change> {
    Layout::system().apply()
}

pub fn undo_created(change: Option<&Change>) -> Result<Withdrawal> {
    Layout::system().undo_created(change)
}

pub fn rollback() -> Result<Withdrawal> {
    Layout::system().withdraw(None)
}

pub fn report_install(change: &Change) {
    println!(
        "{{\"event\":\"user_io_delegation\",\"dropin\":{},\"state\":{},\"existing_user_managers\":\"needs_refresh_unless_runtime_verified\",\"automatic_user_manager_refresh\":false,\"runtime\":{}}}",
        json::quote(DROPIN),
        json::quote(change.state),
        diagnostics(),
    );
}

pub fn diagnostics() -> json::Value {
    let effective_uid = fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|line| line.starts_with("Uid:"))?
                .split_whitespace()
                .nth(2)?
                .parse::<u32>()
                .ok()
        });
    let uid = match effective_uid {
        Some(0) => std::env::var("SUDO_UID")
            .ok()
            .and_then(|uid| uid.parse::<u32>().ok())
            .filter(|uid| *uid != 0),
        uid => uid,
    };
    let Some(uid) = uid else {
        return json::object([
            ("state", "not_checked_no_caller_user".into()),
            ("automatic_refresh", false.into()),
            ("io_weight_effective", json::Value::Null),
        ]);
    };
    let path = PathBuf::from(format!(
        "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service"
    ));
    let available = fs::read_to_string(path.join("cgroup.controllers"))
        .ok()
        .map(|text| text.split_whitespace().any(|controller| controller == "io"));
    let enabled = fs::read_to_string(path.join("cgroup.subtree_control"))
        .ok()
        .map(|text| text.split_whitespace().any(|controller| controller == "io"));
    let state = if !path.exists() {
        "no_active_manager"
    } else if available == Some(true) && enabled == Some(true) && path.join("io.stat").exists() {
        "controller_available"
    } else {
        "needs_refresh"
    };
    json::object([
        ("uid", u64::from(uid).into()),
        ("state", state.into()),
        ("manager_cgroup", path.to_string_lossy().into_owned().into()),
        (
            "io_available",
            available.map_or(json::Value::Null, Into::into),
        ),
        (
            "io_enabled_for_children",
            enabled.map_or(json::Value::Null, Into::into),
        ),
        ("automatic_refresh", false.into()),
        ("io_weight_effective", json::Value::Null),
        (
            "weight_backend",
            "requires_device_iocost_or_bfq_support".into(),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct Fixture {
        root: PathBuf,
        layout: Layout,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root =
                std::env::temp_dir().join(format!("zmem-io-{name}-{}-{stamp}", std::process::id()));
            fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
            let layout = Layout {
                dropin: root.join("system/user@.service.d/50-zmem-io.conf"),
                journal: root.join("state/io-delegation.json"),
                owner: fs::metadata(&root).unwrap().uid(),
                trusted_root: root.clone(),
            };
            layout.secure_parent(&layout.dropin, true).unwrap();
            layout.secure_parent(&layout.journal, true).unwrap();
            Self { root, layout }
        }

        fn pending(&self, published: bool) -> Receipt {
            let staging_name = ".zmem-io-42-123.tmp".to_string();
            let staging = self.layout.dropin.parent().unwrap().join(&staging_name);
            fs::write(&staging, CONTENT).unwrap();
            fs::set_permissions(&staging, fs::Permissions::from_mode(0o644)).unwrap();
            let meta = fs::metadata(&staging).unwrap();
            let receipt = Receipt {
                dev: meta.dev(),
                ino: meta.ino(),
                staging_name,
                installed_bytes: CONTENT.into(),
            };
            fs::write(&self.layout.journal, receipt.encode()).unwrap();
            fs::set_permissions(&self.layout.journal, fs::Permissions::from_mode(0o600)).unwrap();
            if published {
                fs::hard_link(&staging, &self.layout.dropin).unwrap();
            }
            receipt
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn creates_additive_configuration_and_explicit_rollback_removes_only_owned_file() {
        let fixture = Fixture::new("install");
        let layout = &fixture.layout;
        let change = layout.apply().unwrap();
        assert_eq!(change.state, "installed");
        assert!(change.created.is_some());
        assert_eq!(fs::read_to_string(&layout.dropin).unwrap(), CONTENT);
        assert!(CONTENT.contains("\nDelegate=io\nIOAccounting=yes\n"));
        assert!(!CONTENT.contains("\nDelegate=\n"));
        assert_eq!(
            fs::metadata(&layout.journal).unwrap().mode() & 0o7777,
            0o600
        );
        let external = layout.dropin.parent().unwrap().join("90-user.conf");
        fs::write(&external, "[Service]\nDelegate=cpu memory pids\n").unwrap();
        let result = layout.withdraw(None).unwrap();
        assert!(result.removed);
        assert!(!result.preserved_external);
        assert!(!layout.dropin.exists());
        assert!(!layout.journal.exists());
        assert_eq!(
            fs::read_to_string(external).unwrap(),
            "[Service]\nDelegate=cpu memory pids\n"
        );
    }

    #[test]
    fn preexisting_identical_file_or_symlink_is_not_claimed_or_overwritten() {
        for link in [false, true] {
            let fixture = Fixture::new("preexisting");
            let layout = &fixture.layout;
            let external = fixture.root.join("external.conf");
            fs::write(&external, CONTENT).unwrap();
            if link {
                symlink(&external, &layout.dropin).unwrap();
            } else {
                fs::write(&layout.dropin, CONTENT).unwrap();
            }
            let before = fs::symlink_metadata(&layout.dropin).unwrap();
            let change = layout.apply().unwrap();
            assert_eq!(change.state, "external_file_preserved");
            assert!(change.created.is_none());
            assert!(!layout.journal.exists());
            assert!(!layout.withdraw(None).unwrap().removed);
            assert_eq!(
                fs::symlink_metadata(&layout.dropin).unwrap().ino(),
                before.ino()
            );
            assert_eq!(fs::read_to_string(&layout.dropin).unwrap(), CONTENT);
        }
    }

    #[test]
    fn external_edit_replacement_or_permission_change_survives_rollback() {
        for alteration in ["contents", "inode", "mode"] {
            let fixture = Fixture::new(alteration);
            let layout = &fixture.layout;
            layout.apply().unwrap();
            match alteration {
                "contents" => fs::write(&layout.dropin, "[Service]\nDelegate=memory\n").unwrap(),
                "inode" => {
                    fs::rename(&layout.dropin, fixture.root.join("previous-inode")).unwrap();
                    fs::write(&layout.dropin, CONTENT).unwrap();
                    fs::set_permissions(&layout.dropin, fs::Permissions::from_mode(0o644)).unwrap();
                }
                _ => {
                    fs::set_permissions(&layout.dropin, fs::Permissions::from_mode(0o600)).unwrap()
                }
            }
            let expected = fs::read(&layout.dropin).unwrap();
            let observed = fs::metadata(&layout.dropin).unwrap();
            let result = layout.withdraw(None).unwrap();
            assert!(result.preserved_external);
            assert!(!result.removed);
            assert_eq!(fs::read(&layout.dropin).unwrap(), expected);
            assert_eq!(fs::metadata(&layout.dropin).unwrap().ino(), observed.ino());
            assert!(!layout.journal.exists());
            assert_eq!(layout.apply().unwrap().state, "external_file_preserved");
        }
    }

    #[test]
    fn failed_upgrade_withdraws_new_creation_but_keeps_previous_owned_configuration() {
        let fixture = Fixture::new("upgrade");
        let layout = &fixture.layout;
        let first = layout.apply().unwrap();
        let baseline = fs::read(&layout.journal).unwrap();
        let upgrade = layout.apply().unwrap();
        assert_eq!(upgrade.state, "already_owned");
        assert!(!layout.undo_created(Some(&upgrade)).unwrap().removed);
        assert_eq!(fs::read(&layout.journal).unwrap(), baseline);
        assert_eq!(fs::read_to_string(&layout.dropin).unwrap(), CONTENT);
        assert!(layout.undo_created(Some(&first)).unwrap().removed);
        assert!(!layout.dropin.exists());
        assert!(!layout.journal.exists());
    }

    #[test]
    fn failed_publication_removes_prepared_state_and_preserves_a_racing_external_file() {
        for external in [false, true] {
            let fixture = Fixture::new("publish-failure");
            let layout = &fixture.layout;
            let result = layout.apply_with(|| {
                if external {
                    fs::write(&layout.dropin, "external administrator configuration")?;
                    Ok(())
                } else {
                    Err("injected interruption after durable ownership receipt".into())
                }
            });
            assert!(result.is_err());
            assert!(!layout.journal.exists());
            if external {
                assert_eq!(
                    fs::read_to_string(&layout.dropin).unwrap(),
                    "external administrator configuration"
                );
            } else {
                assert!(!layout.dropin.exists());
            }
            let files: Vec<_> = fs::read_dir(layout.dropin.parent().unwrap())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(files.len(), usize::from(external));
        }
    }

    #[test]
    fn write_ahead_identity_recovers_both_sides_of_atomic_publication() {
        for published in [false, true] {
            let fixture = Fixture::new("crash-recovery");
            let layout = &fixture.layout;
            let receipt = fixture.pending(published);
            let result = layout.apply().unwrap();
            assert_eq!(result.created.is_some(), !published);
            assert!(layout.owned(&layout.dropin, &receipt, 1).unwrap());
            assert!(!layout.staging(&receipt).exists());
            assert_eq!(layout.read_receipt().unwrap().unwrap().value, receipt);
            assert!(layout.withdraw(None).unwrap().removed);
        }
    }

    #[test]
    fn corrupt_receipt_or_additional_hard_link_cannot_authorize_deletion() {
        let fixture = Fixture::new("receipt-changed");
        let layout = &fixture.layout;
        layout.apply().unwrap();
        let record = fs::read_to_string(&layout.journal).unwrap();
        fs::write(&layout.journal, record.trim_end()).unwrap();
        assert!(layout.withdraw(None).is_err());
        assert!(layout.dropin.exists());
        fs::write(&layout.journal, record).unwrap();
        fs::hard_link(&layout.dropin, fixture.root.join("external-link")).unwrap();
        let result = layout.withdraw(None).unwrap();
        assert!(!result.removed);
        assert!(result.preserved_external);
        assert!(layout.dropin.exists());
    }
}
