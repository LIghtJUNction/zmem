//! Sampling and confirmed application of the managed background budget.
//!
//! Every path is an explicitly registered aggregate. Partial discovery holds
//! existing controls. Redistribution tightens all groups before relaxing any.

use super::*;
use crate::budget::{BudgetPlan, GroupUsage, cap_and_redistribute, plan_background};

const MAX_GROUPS: usize = 1024;
const RELAX_INTERVAL: Duration = Duration::from_secs(2);
const RELAX_BYTES: u64 = 256 * MIB;

#[derive(Clone, Debug)]
pub struct Group {
    pub path: PathBuf,
    dev: u64,
    ino: u64,
    pub current_bytes: u64,
    pub swap_bytes: Option<u64>,
    pub populated: bool,
    pub sampled_high_bytes: Option<u64>,
    pub requested_high_bytes: Option<u64>,
    pub request_available: bool,
    pub actual_high_bytes: Option<u64>,
    pub readback_available: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub groups: Vec<Group>,
    pub errors: Vec<String>,
    pub sample_complete: bool,
    pub plan_available: bool,
    pub requested_budget_bytes: Option<u64>,
    pub actual_budget_bytes: Option<u64>,
    pub readback_complete: bool,
    pub applied_complete: bool,
}

#[derive(Default)]
pub(super) struct State {
    confirmed_budget: Option<u64>,
    last_relax: Option<Instant>,
}

impl State {
    fn observe(&mut self, actual: Option<u64>, now: Instant) {
        if let Some(actual) = actual {
            if self
                .confirmed_budget
                .is_some_and(|previous| actual > previous)
            {
                self.last_relax = Some(now);
            }
            self.confirmed_budget = Some(actual);
        }
        // An unlimited read-back can be a systemd reset or a new group. Keep
        // the last finite aggregate until the new set has been constrained.
    }

    fn growth_allowance(&self, now: Instant) -> u64 {
        if self.last_relax.is_none_or(|last| {
            now.checked_duration_since(last)
                .is_some_and(|elapsed| elapsed >= RELAX_INTERVAL)
        }) {
            RELAX_BYTES
        } else {
            0
        }
    }

    fn confirm(&mut self, actual: Option<u64>, episode: bool, now: Instant) {
        if !episode {
            self.confirmed_budget = None;
            self.last_relax = None;
        } else if let Some(actual) = actual {
            if self
                .confirmed_budget
                .is_none_or(|previous| actual > previous)
            {
                self.last_relax = Some(now);
            }
            self.confirmed_budget = Some(actual);
        }
    }
}

pub(super) fn paths() -> Result<Vec<PathBuf>> {
    let mut candidates = vec![Path::new(CGROOT).join("zmem.slice/zmem-background.slice")];
    let users = match fs::read_dir(Path::new(CGROOT).join("user.slice")) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if let Some(users) = users {
        for (index, entry) in users.enumerate() {
            if index >= MAX_GROUPS {
                return Err("background discovery exceeds 1024 user entries".into());
            }
            let entry = entry?;
            let name = entry.file_name();
            let Some(uid) = name
                .to_str()
                .and_then(|name| name.strip_prefix("user-"))
                .and_then(|name| name.strip_suffix(".slice"))
                .filter(|uid| !uid.is_empty() && uid.bytes().all(|c| c.is_ascii_digit()))
            else {
                continue;
            };
            candidates.push(entry.path().join(format!(
                "user@{uid}.service/zmem.slice/zmem-background.slice"
            )));
        }
    }
    let mut groups = Vec::new();
    for path in candidates {
        match fs::metadata(&path) {
            Ok(meta) if meta.is_dir() => groups.push(path),
            Ok(_) => {
                return Err(format!(
                    "background aggregate is not a directory: {}",
                    path.display()
                )
                .into());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    groups.sort_unstable();
    Ok(groups)
}

fn high_value(value: &str) -> Result<Option<u64>> {
    if value.trim() == "max" {
        Ok(None)
    } else {
        Ok(Some(value.trim().parse()?))
    }
}

fn populated(path: &Path) -> Result<bool> {
    match counters(&read(path.join("cgroup.events"))?).get("populated") {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err("missing or invalid cgroup populated state".into()),
    }
}

fn same_group(group: &Group) -> Result<()> {
    let meta = fs::metadata(&group.path)?;
    if !meta.is_dir() || meta.dev() != group.dev || meta.ino() != group.ino {
        return Err(format!(
            "background group identity changed: {}",
            group.path.display()
        )
        .into());
    }
    Ok(())
}

fn sample_group(path: &Path) -> Result<Group> {
    let meta = fs::metadata(path)?;
    let current_bytes = read(path.join("memory.current"))?.trim().parse()?;
    let swap_bytes = match read(path.join("memory.swap.current")) {
        Ok(value) => Some(value.trim().parse()?),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    let high = high_value(&read(path.join("memory.high"))?)?;
    let group = Group {
        path: path.into(),
        dev: meta.dev(),
        ino: meta.ino(),
        current_bytes,
        swap_bytes,
        populated: populated(path)?,
        sampled_high_bytes: high,
        requested_high_bytes: None,
        request_available: false,
        actual_high_bytes: high,
        readback_available: true,
    };
    same_group(&group)?;
    Ok(group)
}

fn actual_budget(groups: &[Group]) -> Result<Option<u64>> {
    let mut total = 0u64;
    for group in groups.iter().filter(|group| group.populated) {
        if !group.readback_available {
            return Err("background high read-back is incomplete".into());
        }
        let Some(high) = group.actual_high_bytes else {
            return Ok(None);
        };
        total = total
            .checked_add(high)
            .ok_or("background high sum overflow")?;
    }
    Ok(Some(total))
}

pub(super) fn sample() -> Report {
    let mut report = Report::default();
    let paths = match paths() {
        Ok(paths) => paths,
        Err(error) => {
            report.errors.push(format!("background discovery: {error}"));
            return report;
        }
    };
    for path in paths {
        match sample_group(&path) {
            Ok(group) => report.groups.push(group),
            Err(_)
                if fs::metadata(&path)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
            Err(error) => report
                .errors
                .push(format!("background sample {}: {error}", path.display())),
        }
    }
    report.sample_complete = report.errors.is_empty();
    if report.sample_complete {
        match actual_budget(&report.groups) {
            Ok(actual) => {
                report.actual_budget_bytes = actual;
                report.readback_complete = true;
            }
            Err(error) => {
                report.sample_complete = false;
                report.errors.push(error.to_string());
            }
        }
    }
    report
}

fn plan(
    settings: &Settings,
    t: &mut Telemetry,
    d: &Decision,
    state: &mut State,
) -> Option<BudgetPlan> {
    if !t.background.sample_complete
        || !t.background.readback_complete
        || d.reason == zmem::Reason::InvalidSnapshot
    {
        return None;
    }
    let now = Instant::now();
    state.observe(t.background.actual_budget_bytes, now);
    let groups: Vec<_> = t
        .background
        .groups
        .iter()
        .map(|group| GroupUsage {
            current_bytes: group.current_bytes,
            populated: group.populated,
        })
        .collect();
    let plan = plan_background(
        t.sample.total_bytes,
        t.sample.available_bytes,
        settings
            .policy
            .effective_reserve_bytes(t.sample.total_bytes),
        d.background_high_percent,
        &groups,
        state.confirmed_budget,
        state.growth_allowance(now),
    );
    t.background.plan_available = true;
    t.background.requested_budget_bytes = plan.total_bytes;
    Some(plan)
}

#[derive(Clone, Debug)]
struct Update {
    index: usize,
    path: PathBuf,
    before: Option<u64>,
    requested: Option<u64>,
}

impl Update {
    fn tightens(&self) -> bool {
        match (self.before, self.requested) {
            (None, Some(_)) => true,
            (Some(before), Some(after)) => after < before,
            _ => false,
        }
    }
}

fn prepare(report: &Report, plan: &BudgetPlan, c: &Controls) -> Result<(Vec<Update>, Option<u64>)> {
    let page_size = control_page_size()?;
    let mut updates = Vec::new();
    let mut caps = Vec::new();
    if report.groups.len() != plan.limits.len() {
        return Err("background plan does not match sampled groups".into());
    }
    for (index, (group, requested)) in report.groups.iter().zip(&plan.limits).enumerate() {
        same_group(group)?;
        if populated(&group.path)? != group.populated {
            return Err(format!("background population changed: {}", group.path.display()).into());
        }
        let high = group.path.join("memory.high");
        let meta = fs::metadata(&high)?;
        let observed = read(&high)?;
        let original = c
            .entries
            .get(&high)
            .filter(|entry| entry.dev == meta.dev() && entry.ino == meta.ino())
            .map_or(observed.trim(), |entry| entry.original.as_str());
        let requested = requested.map_or_else(|| "max".into(), |bytes| bytes.to_string());
        let requested =
            canonical_control_value(&high, &capped_high(original, &requested)?, page_size)?;
        caps.push(high_value(original)?.map(|bytes| bytes / page_size));
        same_group(group)?;
        updates.push(Update {
            index,
            path: high,
            before: high_value(&observed)?,
            requested: high_value(&requested)?,
        });
    }
    let mut bounded = BudgetPlan {
        total_bytes: plan.total_bytes.map(|bytes| bytes / page_size),
        limits: plan
            .limits
            .iter()
            .map(|limit| limit.map(|bytes| bytes / page_size))
            .collect(),
    };
    let groups: Vec<_> = report
        .groups
        .iter()
        .map(|group| GroupUsage {
            current_bytes: group.current_bytes,
            populated: group.populated,
        })
        .collect();
    cap_and_redistribute(&mut bounded, &groups, &caps);
    if bounded.total_bytes.is_some() {
        for (update, group) in updates.iter_mut().zip(&report.groups) {
            if group.populated {
                update.requested = bounded.limits[update.index].map(|pages| pages * page_size);
            }
        }
    }
    Ok((updates, bounded.total_bytes.map(|pages| pages * page_size)))
}

fn apply_ordered<E>(
    updates: &[Update],
    mut write: impl FnMut(&Update) -> std::result::Result<(), E>,
) -> Vec<E> {
    let mut errors = Vec::new();
    for update in updates.iter().filter(|update| update.tightens()) {
        if let Err(error) = write(update) {
            errors.push(error);
        }
    }
    if errors.is_empty() {
        for update in updates
            .iter()
            .filter(|update| !update.tightens() && update.before != update.requested)
        {
            if let Err(error) = write(update) {
                errors.push(error);
                // Further relaxation may rely on the result of this write.
                break;
            }
        }
    }
    errors
}

fn refresh_readback(report: &mut Report, errors: &mut Vec<String>) {
    report.readback_complete = true;
    for group in &mut report.groups {
        let result = (|| -> Result<Option<u64>> {
            same_group(group)?;
            if populated(&group.path)? != group.populated {
                return Err("background population changed during application".into());
            }
            let value = high_value(&read(group.path.join("memory.high"))?)?;
            same_group(group)?;
            Ok(value)
        })();
        match result {
            Ok(value) => {
                group.actual_high_bytes = value;
                group.readback_available = true;
            }
            Err(error) => {
                group.actual_high_bytes = None;
                group.readback_available = false;
                report.readback_complete = false;
                errors.push(format!(
                    "background read-back {}: {error}",
                    group.path.display()
                ));
            }
        }
    }
    report.actual_budget_bytes = None;
    if report.readback_complete {
        match actual_budget(&report.groups) {
            Ok(actual) => report.actual_budget_bytes = actual,
            Err(error) => {
                report.readback_complete = false;
                errors.push(error.to_string());
            }
        }
    }
}

pub(super) fn apply(
    settings: &Settings,
    t: &mut Telemetry,
    d: &Decision,
    c: &mut Controls,
    errors: &mut Vec<String>,
) -> Vec<PathBuf> {
    let Some(plan) = plan(settings, t, d, &mut c.background) else {
        return Vec::new();
    };
    let (updates, requested_budget) = match prepare(&t.background, &plan, c) {
        Ok(prepared) => prepared,
        Err(error) => {
            errors.push(format!("background controls held: {error}"));
            refresh_readback(&mut t.background, errors);
            return Vec::new();
        }
    };
    t.background.requested_budget_bytes = requested_budget;
    for update in &updates {
        let group = &mut t.background.groups[update.index];
        group.requested_high_bytes = update.requested;
        group.request_available = true;
    }
    let failures = apply_ordered(&updates, |update| -> Result<()> {
        let group = &t.background.groups[update.index];
        same_group(group)?;
        if populated(&group.path)? != group.populated {
            return Err(format!("background population changed: {}", group.path.display()).into());
        }
        if high_value(&read(&update.path)?)? != update.before {
            return Err(format!(
                "background high changed before application: {}",
                group.path.display()
            )
            .into());
        }
        let value = update
            .requested
            .map_or_else(|| "max".into(), |bytes| bytes.to_string());
        c.set(&update.path, &value, true)?;
        same_group(group)?;
        if high_value(&read(&update.path)?)? != update.requested {
            return Err(format!(
                "background high read-back changed: {}",
                group.path.display()
            )
            .into());
        }
        Ok(())
    });
    let writes_complete = failures.is_empty();
    errors.extend(
        failures
            .into_iter()
            .map(|error| format!("background control: {error}")),
    );
    refresh_readback(&mut t.background, errors);
    t.background.applied_complete = writes_complete
        && t.background.readback_complete
        && t.background
            .groups
            .iter()
            .all(|group| group.actual_high_bytes == group.requested_high_bytes);
    if t.background.readback_complete {
        // Commit observed limits even after partial success, never the plan.
        // A failed tightening prevents every relaxation in this batch.
        c.background.confirm(
            t.background.actual_budget_bytes,
            plan.total_bytes.is_some(),
            Instant::now(),
        );
    }
    reclaim_paths(settings, t)
}

pub(super) fn preview(
    settings: &Settings,
    t: &mut Telemetry,
    d: &Decision,
    state: &mut State,
) -> Vec<PathBuf> {
    let Some(mut plan) = plan(settings, t, d, state) else {
        return Vec::new();
    };
    let groups: Vec<_> = t
        .background
        .groups
        .iter()
        .map(|group| GroupUsage {
            current_bytes: group.current_bytes,
            populated: group.populated,
        })
        .collect();
    let caps: Vec<_> = t
        .background
        .groups
        .iter()
        .map(|group| group.sampled_high_bytes)
        .collect();
    cap_and_redistribute(&mut plan, &groups, &caps);
    t.background.requested_budget_bytes = plan.total_bytes;
    for (group, requested) in t.background.groups.iter_mut().zip(plan.limits) {
        group.requested_high_bytes = match (group.sampled_high_bytes, requested) {
            (Some(existing), Some(requested)) => Some(existing.min(requested)),
            (Some(existing), None) => Some(existing),
            (None, requested) => requested,
        };
        group.request_available = true;
    }
    reclaim_paths(settings, t)
}

fn reclaim_paths(settings: &Settings, t: &Telemetry) -> Vec<PathBuf> {
    let reserve = settings
        .policy
        .effective_reserve_bytes(t.sample.total_bytes);
    if !t.background.sample_complete
        || t.sample.available_bytes
            >= reserve
                .saturating_add(reserve / 4)
                .min(t.sample.total_bytes)
    {
        return Vec::new();
    }
    let mut groups: Vec<_> = t
        .background
        .groups
        .iter()
        .filter(|group| group.populated && group.current_bytes > 0)
        .collect();
    groups.sort_unstable_by_key(|group| std::cmp::Reverse(group.current_bytes));
    groups.into_iter().map(|group| group.path.clone()).collect()
}

pub(super) fn json(report: &Report) -> String {
    let rows = report.groups.iter().map(|group| format!(
        "{{\"path\":{},\"populated\":{},\"current_bytes\":{},\"swap_bytes\":{},\"sampled_high_bytes\":{},\"requested_high_bytes\":{},\"request_available\":{},\"actual_high_bytes\":{},\"readback_available\":{}}}",
        json::quote(&group.path.to_string_lossy()), group.populated, group.current_bytes,
        optional_number(group.swap_bytes), optional_number(group.sampled_high_bytes),
        optional_number(group.requested_high_bytes), group.request_available,
        optional_number(group.actual_high_bytes), group.readback_available,
    )).collect::<Vec<_>>().join(",");
    format!(
        "{{\"sample_complete\":{},\"plan_available\":{},\"requested_budget_bytes\":{},\"actual_budget_bytes\":{},\"readback_complete\":{},\"applied_complete\":{},\"groups\":[{}],\"errors\":{}}}",
        report.sample_complete,
        report.plan_available,
        optional_number(report.requested_budget_bytes),
        optional_number(report.actual_budget_bytes),
        report.readback_complete,
        report.applied_complete,
        rows,
        json::strings(&report.errors),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(index: usize, before: Option<u64>, requested: Option<u64>) -> Update {
        Update {
            index,
            path: PathBuf::from(format!("group-{index}/memory.high")),
            before,
            requested,
        }
    }

    #[test]
    fn redistribution_tightens_before_relaxing_regardless_of_group_order() {
        let updates = [
            update(0, Some(GIB), Some(8 * GIB)),
            update(1, Some(8 * GIB), Some(GIB)),
        ];
        let mut order = Vec::new();
        let errors: Vec<()> = apply_ordered(&updates, |update| {
            order.push(update.index);
            Ok(())
        });
        assert!(errors.is_empty());
        assert_eq!(order, [1, 0]);
    }

    #[test]
    fn failed_tightening_blocks_all_relaxations() {
        let updates = [
            update(0, Some(GIB), Some(8 * GIB)),
            update(1, Some(8 * GIB), Some(GIB)),
            update(2, None, Some(GIB)),
        ];
        let mut order = Vec::new();
        let errors = apply_ordered(&updates, |update| {
            order.push(update.index);
            if update.index == 1 {
                Err("write failed")
            } else {
                Ok(())
            }
        });
        assert_eq!(errors, ["write failed"]);
        assert_eq!(order, [1, 2]);
    }

    #[test]
    fn failed_proposals_do_not_increase_the_confirmed_budget() {
        let start = Instant::now();
        let mut state = State::default();
        state.observe(Some(256 * MIB), start);
        let groups = [GroupUsage {
            current_bytes: 4 * GIB,
            populated: true,
        }];
        for second in [0, 2, 4, 30, 60] {
            let now = start + Duration::from_secs(second);
            let plan = plan_background(
                16 * GIB,
                8 * GIB,
                2 * GIB,
                50,
                &groups,
                state.confirmed_budget,
                state.growth_allowance(now),
            );
            assert_eq!(plan.total_bytes, Some(512 * MIB));
            state.confirm(Some(256 * MIB), true, now);
            assert_eq!(state.confirmed_budget, Some(256 * MIB));
        }
    }

    #[test]
    fn rapid_sampling_cannot_accelerate_confirmed_budget_recovery() {
        let start = Instant::now();
        let mut state = State::default();
        state.confirm(Some(GIB), true, start);
        for ms in [0, 250, 500, 1000, 1750, 1999] {
            assert_eq!(state.growth_allowance(start + Duration::from_millis(ms)), 0);
        }
        assert_eq!(
            state.growth_allowance(start + Duration::from_secs(2)),
            RELAX_BYTES
        );
        state.confirm(
            Some(GIB + RELAX_BYTES),
            true,
            start + Duration::from_secs(2),
        );
        assert_eq!(
            state.growth_allowance(start + Duration::from_millis(2250)),
            0
        );
        assert_eq!(
            state.growth_allowance(start + Duration::from_secs(4)),
            RELAX_BYTES
        );
    }

    #[test]
    fn partial_actual_limits_become_the_next_baseline_and_healthy_clears_history() {
        let start = Instant::now();
        let mut state = State::default();
        state.confirm(Some(8 * GIB), true, start);
        state.confirm(Some(7 * GIB), true, start + Duration::from_secs(2));
        assert_eq!(state.confirmed_budget, Some(7 * GIB));
        state.observe(None, start + Duration::from_secs(3));
        assert_eq!(state.confirmed_budget, Some(7 * GIB));
        state.confirm(None, false, start + Duration::from_secs(4));
        assert_eq!(state.confirmed_budget, None);
    }
}
