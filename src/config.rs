use crate::Result;
use std::{
    fs,
    path::{Path, PathBuf},
};
use zmem::{Config, MIB};

#[derive(Clone, Debug)]
pub struct Settings {
    pub policy: Config,
    pub interval_ms: u64,
    pub swap_dir: PathBuf,
    pub zram_enabled: bool,
    pub zram_percent: u64,
    pub protect_sessions: bool,
    pub manage_swappiness: bool,
    pub dedup_enabled: bool,
    pub profile: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            policy: Config::default(),
            interval_ms: 2000,
            swap_dir: PathBuf::from("/var/lib/zmem/swap"),
            zram_enabled: true,
            zram_percent: 50,
            protect_sessions: true,
            manage_swappiness: true,
            dedup_enabled: true,
            profile: "balanced".into(),
        }
    }
}

impl Settings {
    pub fn read(path: &Path) -> Result<Self> {
        Self::parse(&fs::read_to_string(path)?)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut s = Self::default();
        let mut seen = std::collections::BTreeSet::new();
        for (i, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let (k, v) = line
                .split_once('=')
                .ok_or_else(|| format!("config line {}: expected key = value", i + 1))?;
            let (k, v) = (k.trim(), v.trim().trim_matches('"'));
            if !seen.insert(k) {
                return Err(format!("duplicate config key: {k}").into());
            }
            let n = || -> Result<u64> {
                v.parse::<u64>()
                    .map_err(|_| format!("invalid nonnegative integer for {k}").into())
            };
            let bytes = || -> Result<u64> {
                n()?.checked_mul(MIB)
                    .ok_or_else(|| format!("{k} overflows byte count").into())
            };
            let boolean = || -> Result<bool> {
                match v {
                    "true" => Ok(true),
                    "false" => Ok(false),
                    _ => Err(format!("{k} must be true or false").into()),
                }
            };
            match k {
                "interval_ms" => s.interval_ms = n()?,
                "reserve_mib" => s.policy.reserve_bytes = bytes()?,
                "swap_chunk_mib" => s.policy.chunk_bytes = bytes()?,
                "max_disk_swap_mib" => s.policy.max_disk_swap_bytes = bytes()?,
                "disk_reserve_mib" => s.policy.disk_reserve_bytes = bytes()?,
                "swap_free_target_mib" => s.policy.grow_free_bytes = bytes()?,
                "pressure_some_bp" => s.policy.pressure_some_bp = u16::try_from(n()?)?,
                "critical_full_bp" => s.policy.critical_full_bp = u16::try_from(n()?)?,
                "recovery_samples" => s.policy.recovery_samples = u32::try_from(n()?)?,
                "grow_cooldown_secs" => s.policy.grow_cooldown_secs = n()?,
                "shrink_cooldown_secs" => s.policy.shrink_cooldown_secs = n()?,
                "zram_percent" => s.zram_percent = n()?,
                "zram_enabled" => s.zram_enabled = boolean()?,
                "protect_sessions" => s.protect_sessions = boolean()?,
                "manage_swappiness" => s.manage_swappiness = boolean()?,
                "dedup_enabled" => s.dedup_enabled = boolean()?,
                "swap_dir" => s.swap_dir = PathBuf::from(v),
                "profile" => s.profile = v.to_owned(),
                _ => return Err(format!("unknown config key: {k}").into()),
            }
        }
        if !(250..=60_000).contains(&s.interval_ms) {
            return Err("interval_ms must be 250..60000".into());
        }
        if !(1..=200).contains(&s.zram_percent) {
            return Err("zram_percent must be 1..200".into());
        }
        if s.policy.chunk_bytes != 0 && !(64 * MIB..=8192 * MIB).contains(&s.policy.chunk_bytes) {
            return Err("swap_chunk_mib must be 0 or 64..8192".into());
        }
        if s.policy.disk_reserve_bytes < 256 * MIB {
            return Err("disk_reserve_mib must be at least 256".into());
        }
        if s.policy.grow_cooldown_secs < 5 || s.policy.shrink_cooldown_secs < 60 {
            return Err("growth cooldown must be >=5s and shrink cooldown >=60s".into());
        }
        if !(1..=10_000).contains(&s.policy.pressure_some_bp)
            || !(1..=10_000).contains(&s.policy.critical_full_bp)
        {
            return Err("PSI thresholds must be 1..10000 basis points".into());
        }
        if !["balanced", "interactive", "throughput", "ai", "gaming"].contains(&s.profile.as_str())
        {
            return Err("profile must be balanced, interactive, throughput, ai or gaming".into());
        }
        // Root-owned persistent state only; do not permit a user-controlled swap parent.
        if !s.swap_dir.starts_with("/var/lib/zmem/")
            || s.swap_dir
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            || s.swap_dir == Path::new("/var/lib/zmem")
        {
            return Err("swap_dir must be a child of /var/lib/zmem without '..'".into());
        }
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bad_config_fails_closed() {
        for input in [
            "typo=1",
            "interval_ms=0",
            "swap_dir=/tmp/swap",
            "swap_dir=/var/lib/zmem/../x",
            "reserve_mib=18446744073709551615",
            "zram_enabled=yes",
            "dedup_enabled=yes",
            "profile=unknown",
            "interval_ms=2000\ninterval_ms=3000",
        ] {
            assert!(Settings::parse(input).is_err(), "{input}");
        }
    }
    #[test]
    fn config_units_are_explicit() {
        let s = Settings::parse("swap_chunk_mib=1024\n# notes\nprofile=ai").unwrap();
        assert_eq!(s.policy.chunk_bytes, 1024 * MIB);
        assert_eq!(s.profile, "ai");
        assert!(s.dedup_enabled);
        assert!(
            !Settings::parse("dedup_enabled=false")
                .unwrap()
                .dedup_enabled
        );
    }
}
