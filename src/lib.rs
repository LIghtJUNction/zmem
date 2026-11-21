//! Allocation-free policy for a memory and swap controller.
//!
//! The policy consumes measurements and emits bounded requests. Its caller owns
//! operating-system effects: checking free disk space, limiting total disk swap,
//! selecting explicitly managed cgroups, and validating swap-file ownership.
//! There is deliberately no process-kill, freeze, or global cache-drop action.
#![no_std]
#![forbid(unsafe_code)]

/// One mebibyte, in bytes.
pub const MIB: u64 = 1024 * 1024;
/// One gibibyte, in bytes.
pub const GIB: u64 = 1024 * MIB;

const IO_BUSY_BP: u16 = 1000;
const SWAP_IO_BUSY_BYTES: u64 = 64 * MIB;
const RECLAIM_COOLDOWN_SECS: u64 = 10;

/// A sample taken over a consistent interval.
///
/// PSI values are basis points of wall time: 100 is 1%, and 10,000 is 100%.
/// Missing PSI or rate measurements may be zero; `total_bytes` must be nonzero.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Snapshot {
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_free_bytes: u64,
    pub memory_some_bp: u16,
    pub memory_full_bp: u16,
    pub io_full_bp: u16,
    pub swap_in_bytes_per_sec: u64,
    pub swap_out_bytes_per_sec: u64,
}

/// Controller limits. All byte counts are binary bytes, not decimal units.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
    /// Zero selects max(1 GiB, physical memory / 10), capped at physical memory.
    pub reserve_bytes: u64,
    /// Desired unused swap headroom when memory is becoming constrained.
    pub grow_free_bytes: u64,
    /// Maximum bytes requested by one growth decision; zero disables growth.
    pub chunk_bytes: u64,
    /// The adapter enforces this limit across all owned disk swap.
    pub max_disk_swap_bytes: u64,
    /// The adapter keeps at least this much filesystem space available.
    pub disk_reserve_bytes: u64,
    pub pressure_some_bp: u16,
    pub critical_full_bp: u16,
    /// Consecutive samples with comfortable headroom needed to clear pressure.
    pub recovery_samples: u32,
    pub grow_cooldown_secs: u64,
    /// Stable healthy time before an unused, owned swap file may be removed.
    pub shrink_cooldown_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            reserve_bytes: 0,
            grow_free_bytes: 4 * GIB,
            chunk_bytes: 2 * GIB,
            max_disk_swap_bytes: 32 * GIB,
            disk_reserve_bytes: 8 * GIB,
            pressure_some_bp: 1000,
            critical_full_bp: 2000,
            recovery_samples: 5,
            grow_cooldown_secs: 30,
            shrink_cooldown_secs: 600,
        }
    }
}

impl Config {
    /// Reserve used for a particular physical-memory size.
    pub const fn effective_reserve_bytes(self, total_bytes: u64) -> u64 {
        let automatic = if total_bytes / 10 > GIB {
            total_bytes / 10
        } else {
            GIB
        };
        let requested = if self.reserve_bytes == 0 {
            automatic
        } else {
            self.reserve_bytes
        };
        if requested > total_bytes {
            total_bytes
        } else {
            requested
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Pressure {
    #[default]
    Healthy,
    Elevated,
    Critical,
    Recovering,
}

impl Pressure {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Elevated => "elevated",
            Self::Critical => "critical",
            Self::Recovering => "recovering",
        }
    }
}

/// The most relevant explanation for the current decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reason {
    Healthy,
    LowAvailable,
    MemoryPressure,
    MemoryStall,
    SwapIoPressure,
    Recovering,
    InvalidSnapshot,
}

impl Reason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::LowAvailable => "low_available_memory",
            Self::MemoryPressure => "memory_reclaim_pressure",
            Self::MemoryStall => "memory_full_stall",
            Self::SwapIoPressure => "swap_or_io_pressure",
            Self::Recovering => "waiting_for_stable_recovery",
            Self::InvalidSnapshot => "invalid_memory_sample",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Decision {
    pub pressure: Pressure,
    /// A bounded growth request, after cooldown. The adapter may refuse it.
    pub grow_swap_bytes: u64,
    /// Permission to consider shrinking; never permission to drain used swap.
    pub shrink_allowed: bool,
    /// Percentage of physical RAM for the managed background aggregate.
    /// 100 means the adapter can remove a temporary `memory.high` restriction.
    pub background_high_percent: u8,
    /// Maximum proactive reclaim request for explicitly managed background work.
    pub reclaim_bytes: u64,
    /// A cost-policy hint in Linux's 0..=200 range. Device-aware callers may
    /// override it; the policy does not assume swap is compressed or on disk.
    pub recommended_swappiness: u16,
    pub reason: Reason,
}

/// Stateful hysteresis and cooldowns. Contains no heap allocations.
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    config: Config,
    pressure: Pressure,
    recovery_count: u32,
    last_observed_secs: Option<u64>,
    last_growth_secs: Option<u64>,
    last_reclaim_secs: Option<u64>,
    healthy_since_secs: Option<u64>,
}

impl Policy {
    pub const fn new(config: Config) -> Self {
        Self {
            config,
            pressure: Pressure::Healthy,
            recovery_count: 0,
            last_observed_secs: None,
            last_growth_secs: None,
            last_reclaim_secs: None,
            healthy_since_secs: None,
        }
    }

    pub const fn config(&self) -> Config {
        self.config
    }

    pub const fn pressure(&self) -> Pressure {
        self.pressure
    }

    /// Advance the policy by one measurement.
    ///
    /// Use monotonic seconds. If the clock moves backwards, action cooldowns
    /// restart at the supplied time and shrink eligibility is discarded.
    /// Growth/reclaim cooldowns begin when a request is emitted, including when
    /// the adapter later rejects it, so a failure cannot cause a tight retry loop.
    pub fn observe(&mut self, sample: Snapshot, now_secs: u64) -> Decision {
        if self.last_observed_secs.is_some_and(|last| now_secs < last) {
            self.last_growth_secs = Some(now_secs);
            self.last_reclaim_secs = Some(now_secs);
            self.healthy_since_secs = None;
            self.recovery_count = 0;
        }
        self.last_observed_secs = Some(now_secs);

        if sample.total_bytes == 0
            || sample.available_bytes > sample.total_bytes
            || sample.swap_free_bytes > sample.swap_total_bytes
        {
            self.healthy_since_secs = None;
            self.recovery_count = 0;
            return Decision {
                pressure: self.pressure,
                grow_swap_bytes: 0,
                shrink_allowed: false,
                background_high_percent: high_percent(self.pressure),
                reclaim_bytes: 0,
                recommended_swappiness: 60,
                reason: Reason::InvalidSnapshot,
            };
        }

        let reserve = self.config.effective_reserve_bytes(sample.total_bytes);
        // Zero thresholds do not turn a zero-PSI observation into an alarm.
        let some_limit = self.config.pressure_some_bp.clamp(1, 10_000);
        let full_limit = self.config.critical_full_bp.clamp(1, 10_000);
        let memory_stall = sample.memory_full_bp >= full_limit;
        let low_available = sample.available_bytes < reserve;
        let critical_available = sample.available_bytes < reserve / 4;
        let raw = if memory_stall || critical_available {
            Pressure::Critical
        } else if low_available || sample.memory_some_bp >= some_limit {
            Pressure::Elevated
        } else {
            Pressure::Healthy
        };
        let io_busy = sample.io_full_bp >= IO_BUSY_BP
            || sample.swap_in_bytes_per_sec >= SWAP_IO_BUSY_BYTES
            || (sample.swap_in_bytes_per_sec >= SWAP_IO_BUSY_BYTES / 4
                && sample.swap_out_bytes_per_sec >= SWAP_IO_BUSY_BYTES);
        let comfortable = sample.available_bytes
            >= reserve.saturating_add(reserve / 4).min(sample.total_bytes)
            && sample.memory_some_bp <= some_limit / 2
            && sample.memory_full_bp <= full_limit / 4
            && !io_busy;

        match raw {
            Pressure::Critical => {
                self.pressure = Pressure::Critical;
                self.recovery_count = 0;
            }
            Pressure::Elevated => {
                // Keep a critical restriction until pressure has actually
                // cleared; one slightly better sample is not recovery.
                if self.pressure != Pressure::Critical {
                    self.pressure = Pressure::Elevated;
                }
                self.recovery_count = 0;
            }
            Pressure::Healthy => {
                if self.pressure != Pressure::Healthy {
                    self.pressure = Pressure::Recovering;
                    self.recovery_count = if comfortable {
                        self.recovery_count.saturating_add(1)
                    } else {
                        0
                    };
                    if self.recovery_count >= self.config.recovery_samples.max(1) {
                        self.pressure = Pressure::Healthy;
                        self.recovery_count = 0;
                    }
                }
            }
            Pressure::Recovering => unreachable!(),
        }

        if self.pressure == Pressure::Healthy && comfortable {
            self.healthy_since_secs.get_or_insert(now_secs);
        } else {
            self.healthy_since_secs = None;
        }

        let near_reserve = sample.available_bytes < reserve.saturating_mul(2);
        let should_grow = (raw != Pressure::Healthy || near_reserve)
            && sample.swap_free_bytes < self.config.grow_free_bytes
            && self.config.chunk_bytes != 0
            && self.config.max_disk_swap_bytes != 0
            && elapsed(
                now_secs,
                self.last_growth_secs,
                self.config.grow_cooldown_secs,
            );
        let grow_swap_bytes = if should_grow {
            let bytes = self
                .config
                .grow_free_bytes
                .saturating_sub(sample.swap_free_bytes)
                .min(self.config.chunk_bytes)
                .min(self.config.max_disk_swap_bytes);
            self.last_growth_secs = Some(now_secs);
            bytes
        } else {
            0
        };

        let reclaim_bytes = if matches!(self.pressure, Pressure::Elevated | Pressure::Critical)
            && raw != Pressure::Healthy
            && !io_busy
            && elapsed(now_secs, self.last_reclaim_secs, RECLAIM_COOLDOWN_SECS)
        {
            self.last_reclaim_secs = Some(now_secs);
            if self.pressure == Pressure::Critical {
                128 * MIB
            } else {
                64 * MIB
            }
        } else {
            0
        };

        let shrink_allowed = self.pressure == Pressure::Healthy
            && comfortable
            && !near_reserve
            && sample.swap_total_bytes != 0
            && sample.swap_free_bytes
                >= self
                    .config
                    .grow_free_bytes
                    .saturating_add(self.config.chunk_bytes)
            && self.healthy_since_secs.is_some_and(|start| {
                now_secs.saturating_sub(start) >= self.config.shrink_cooldown_secs
            })
            && elapsed(
                now_secs,
                self.last_growth_secs,
                self.config.shrink_cooldown_secs,
            );

        let reason = if io_busy {
            Reason::SwapIoPressure
        } else if memory_stall {
            Reason::MemoryStall
        } else if low_available {
            Reason::LowAvailable
        } else if sample.memory_some_bp >= some_limit {
            Reason::MemoryPressure
        } else if self.pressure == Pressure::Recovering {
            Reason::Recovering
        } else {
            Reason::Healthy
        };

        Decision {
            pressure: self.pressure,
            grow_swap_bytes,
            shrink_allowed,
            background_high_percent: high_percent(self.pressure),
            reclaim_bytes,
            recommended_swappiness: if io_busy {
                20
            } else if matches!(self.pressure, Pressure::Elevated | Pressure::Critical) {
                100
            } else {
                60
            },
            reason,
        }
    }
}

const fn high_percent(pressure: Pressure) -> u8 {
    match pressure {
        Pressure::Healthy => 100,
        Pressure::Elevated => 70,
        Pressure::Critical => 50,
        Pressure::Recovering => 85,
    }
}

fn elapsed(now: u64, previous: Option<u64>, interval: u64) -> bool {
    previous.is_none_or(|last| now.saturating_sub(last) >= interval)
}
