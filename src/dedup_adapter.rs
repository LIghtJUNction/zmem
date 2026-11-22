//! KSM adapter: bounded observations, journaled writes, and actual readback.
use super::{Control, Controls, Telemetry, control_page_size, owned_control_value, read};
use crate::{Result, config::Settings, dedup, json};
use std::{fs, os::unix::fs::MetadataExt, path::Path};
use zmem::Decision;

const TASK_REFRESH_MS: u64 = 10_000;

#[derive(Clone, Debug)]
pub struct Report {
    pub enabled: bool,
    pub state: &'static str,
    pub target_state: &'static str,
    pub reason: &'static str,
    pub control_owned: bool,
    pub write_succeeded: Option<bool>,
    pub task_age_ms: Option<u64>,
    pub tasks: dedup::TaskScan,
}
impl Default for Report {
    fn default() -> Self {
        Self {
            enabled: false,
            state: "unobserved",
            target_state: "unobserved",
            reason: "independent_preview_has_no_task_scan",
            control_owned: false,
            write_succeeded: None,
            task_age_ms: None,
            tasks: dedup::TaskScan::default(),
        }
    }
}

pub struct State {
    governor: dedup::Governor,
    tasks: dedup::TaskScan,
    sampled_ms: Option<u64>,
    last_update_ms: u64,
}
impl State {
    pub fn new(interval_ms: u64) -> Self {
        Self {
            governor: dedup::Governor::new(interval_ms),
            tasks: dedup::TaskScan::default(),
            sampled_ms: None,
            last_update_ms: 0,
        }
    }

    pub fn pause_on_sample_error(&mut self, controls: &mut Controls) -> Result<()> {
        let snapshot = dedup::read_ksm(Path::new(dedup::KSM_ROOT));
        let plan = self.sample_error_plan(snapshot.clone(), ownership(controls))?;
        if plan.writes.is_empty() {
            return Ok(());
        }
        // Use the same advisor checks, ownership validation, journal and
        // readback as the ordinary path. A sampling error does not authorize
        // overriding an external controller or a previously latched conflict.
        match apply_plan(controls, &snapshot, &plan) {
            Ok(()) => {
                self.governor.feedback(dedup::ApplyOutcome::Applied);
                Ok(())
            }
            Err((outcome, error)) => {
                self.governor.feedback(outcome);
                Err(error)
            }
        }
    }

    fn sample_error_plan(
        &mut self,
        snapshot: dedup::KsmSnapshot,
        ownership: Result<dedup::Ownership>,
    ) -> Result<dedup::Plan> {
        self.governor.feedback(dedup::ApplyOutcome::Failed);
        let ownership = match ownership {
            Ok(ownership) => ownership,
            Err(error) => {
                self.governor.feedback(dedup::ApplyOutcome::ExternalChange);
                return Err(error);
            }
        };
        let observation = dedup::Observation {
            ksm: snapshot,
            tasks: dedup::TaskScan::default(),
            task_age_ms: None,
        };
        // No healthy interval is credited while sampling fails. Reusing the
        // last observed timestamp also preserves the caller's monotonic clock
        // without adding a second, unrelated Instant epoch to this adapter.
        Ok(self.governor.plan(
            self.last_update_ms,
            true,
            &observation,
            dedup::PressureInput::default(),
            ownership,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        settings: &Settings,
        t: &mut Telemetry,
        decision: &Decision,
        controls: &mut Controls,
        now_ms: u64,
        observe: bool,
        errors: &mut Vec<String>,
    ) {
        self.last_update_ms = now_ms;
        if self.sampled_ms.is_none_or(|previous| {
            now_ms < previous || now_ms.saturating_sub(previous) >= TASK_REFRESH_MS
        }) {
            self.tasks = dedup::scan_tasks(Path::new("/proc"), dedup::ScanLimits::default());
            self.sampled_ms = Some(now_ms);
        }
        let mut observation = dedup::Observation {
            ksm: t.ksm.clone(),
            tasks: self.tasks.clone(),
            task_age_ms: self.sampled_ms.map(|time| now_ms.saturating_sub(time)),
        };
        let reserve = settings
            .policy
            .effective_reserve_bytes(t.sample.total_bytes);
        let memory_healthy =
            decision.pressure == zmem::Pressure::Healthy && t.sample.available_bytes >= reserve;
        let mut pressure = dedup::read_pressure(Path::new("/proc"), Some(memory_healthy));
        if let Some(io_busy) = pressure.io_busy.as_mut() {
            *io_busy |= decision.io_congested;
        }
        let ownership = match ownership(controls) {
            Ok(owned) => owned,
            Err(error) => {
                self.governor.feedback(dedup::ApplyOutcome::ExternalChange);
                errors.push(format!("dedup ownership: {error}"));
                dedup::Ownership::default()
            }
        };
        let mut plan = self.governor.plan(
            now_ms,
            settings.dedup_enabled,
            &observation,
            pressure,
            ownership,
        );
        let mut applied = None;
        if !observe && !plan.writes.is_empty() {
            match apply_plan(controls, &observation.ksm, &plan) {
                Ok(()) => {
                    self.governor.feedback(dedup::ApplyOutcome::Applied);
                    applied = Some(true);
                }
                Err((outcome, error)) => {
                    self.governor.feedback(outcome);
                    applied = Some(false);
                    errors.push(format!("dedup control: {error}"));
                }
            }
            // A requested pause/start is never reported as an achieved state.
            // Observe again and recompute without applying a second plan.
            t.ksm = dedup::read_ksm(Path::new(dedup::KSM_ROOT));
            observation.ksm = t.ksm.clone();
            let actual_ownership = ownership_or_feedback(controls, &mut self.governor);
            plan = self.governor.plan(
                now_ms,
                settings.dedup_enabled,
                &observation,
                pressure,
                actual_ownership,
            );
        }
        t.dedup = Report {
            enabled: settings.dedup_enabled,
            state: plan.state.as_str(),
            target_state: plan.target_state.as_str(),
            reason: plan.reason,
            control_owned: owned_value(controls, dedup::Knob::Run)
                .ok()
                .flatten()
                .is_some(),
            write_succeeded: applied,
            task_age_ms: observation.task_age_ms,
            tasks: observation.tasks,
        };
    }
}

fn owned_value(controls: &Controls, knob: dedup::Knob) -> Result<Option<u64>> {
    let path = Path::new(knob.path());
    let Some(entry) = controls.entries.get(path) else {
        return Ok(None);
    };
    let metadata = fs::metadata(path)?;
    let actual = read(path)?;
    if entry.dev != metadata.dev()
        || entry.ino != metadata.ino()
        || !owned_control_value(
            path,
            &entry.last,
            entry.pending_previous.as_deref(),
            actual.trim(),
            control_page_size()?,
        )?
    {
        return Err(format!("external control change preserved: {}", path.display()).into());
    }
    Ok(Some(actual.trim().parse()?))
}

fn ownership(controls: &Controls) -> Result<dedup::Ownership> {
    Ok(dedup::Ownership {
        run: owned_value(controls, dedup::Knob::Run)?
            .map(u8::try_from)
            .transpose()?,
        pages_to_scan: owned_value(controls, dedup::Knob::PagesToScan)?,
        sleep_millisecs: owned_value(controls, dedup::Knob::SleepMillisecs)?,
    })
}

fn ownership_or_feedback(controls: &Controls, governor: &mut dedup::Governor) -> dedup::Ownership {
    ownership(controls).unwrap_or_else(|_| {
        governor.feedback(dedup::ApplyOutcome::ExternalChange);
        dedup::Ownership::default()
    })
}

fn expected(snapshot: &dedup::KsmSnapshot, knob: dedup::Knob) -> Option<u64> {
    match knob {
        dedup::Knob::Run => snapshot.run.map(u64::from),
        dedup::Knob::PagesToScan => snapshot.pages_to_scan,
        dedup::Knob::SleepMillisecs => snapshot.sleep_millisecs,
    }
}

type ApplyResult = std::result::Result<(), (dedup::ApplyOutcome, Box<dyn std::error::Error>)>;

fn apply_plan(
    controls: &mut Controls,
    snapshot: &dedup::KsmSnapshot,
    plan: &dedup::Plan,
) -> ApplyResult {
    let failed = |error| (dedup::ApplyOutcome::Failed, error);
    let changed = |error| (dedup::ApplyOutcome::ExternalChange, error);
    let before = dedup::read_ksm(Path::new(dedup::KSM_ROOT));
    if !before.complete
        || !matches!(
            before.advisor,
            dedup::Advisor::None | dedup::Advisor::Missing
        )
    {
        return Err(changed(
            "KSM observation/advisor changed before control transaction".into(),
        ));
    }
    // Claim all three stopped controls, including already-matching values.
    // This records their baseline and detects later external knob changes.
    if snapshot.run == Some(0)
        && !controls
            .entries
            .contains_key(Path::new(dedup::Knob::Run.path()))
    {
        let mut claim = Vec::new();
        for knob in [
            dedup::Knob::Run,
            dedup::Knob::PagesToScan,
            dedup::Knob::SleepMillisecs,
        ] {
            let path = Path::new(knob.path());
            let old = read(path).map_err(failed)?.trim().to_owned();
            if old.parse::<u64>().ok() != expected(snapshot, knob) {
                return Err(changed(
                    format!("control changed before claim: {}", path.display()).into(),
                ));
            }
            if controls.entries.contains_key(path) {
                owned_value(controls, knob).map_err(changed)?;
                continue;
            }
            let meta = fs::metadata(path).map_err(|error| failed(error.into()))?;
            claim.push(Control {
                path: path.into(),
                original: old.clone(),
                last: old,
                dev: meta.dev(),
                ino: meta.ino(),
                pending_previous: None,
            });
        }
        let paths: Vec<_> = claim.iter().map(|entry| entry.path.clone()).collect();
        for entry in claim {
            controls.entries.insert(entry.path.clone(), entry);
        }
        if let Err(error) = controls.save() {
            for path in paths {
                controls.entries.remove(&path);
            }
            return Err(failed(error));
        }
    }
    for write in &plan.writes {
        let path = Path::new(write.knob.path());
        let actual = read(path).map_err(failed)?;
        if actual.trim().parse::<u64>().ok() != Some(write.expected) {
            return Err(changed(
                format!("control changed before write: {}", path.display()).into(),
            ));
        }
        owned_value(controls, write.knob).map_err(changed)?;
        if write.knob == dedup::Knob::Run && write.value == 1 {
            let latest = dedup::read_ksm(Path::new(dedup::KSM_ROOT));
            if !latest.complete
                || !matches!(
                    latest.advisor,
                    dedup::Advisor::None | dedup::Advisor::Missing
                )
                || latest.run != Some(0)
                || latest.pages_to_scan != Some(dedup::PAGES_TO_SCAN)
                || latest.sleep_millisecs != Some(dedup::SLEEP_MILLISECS)
            {
                return Err(changed(
                    "KSM controls changed before enabling scanning".into(),
                ));
            }
            ownership(controls).map_err(changed)?;
        }
        controls
            .set(path, &write.value.to_string(), false)
            .map_err(failed)?;
        if read(path).map_err(failed)?.trim().parse::<u64>().ok() != Some(write.value) {
            return Err(changed(
                format!("control readback changed: {}", path.display()).into(),
            ));
        }
    }
    Ok(())
}

fn optional<T: std::fmt::Display>(value: Option<T>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".into())
}

pub fn ksm_json(ksm: &dedup::KsmSnapshot) -> String {
    let advisor = match &ksm.advisor {
        dedup::Advisor::Missing => "not_supported",
        dedup::Advisor::None => "none",
        dedup::Advisor::External(mode) => mode,
        dedup::Advisor::Unreadable => "unknown",
    };
    format!(
        "{{\"available\":{},\"observation_complete\":{},\"run\":{},\"pages_to_scan\":{},\"sleep_millisecs\":{},\"advisor\":{},\"general_profit_bytes\":{},\"pages_sharing\":{},\"pages_shared\":{},\"pages_unshared\":{},\"pages_volatile\":{},\"pages_skipped\":{},\"zero_pages\":{},\"full_scans\":{},\"pages_scanned\":{},\"attribution\":\"system_wide_ksm\",\"errors\":{}}}",
        optional(ksm.available),
        ksm.complete,
        optional(ksm.run),
        optional(ksm.pages_to_scan),
        optional(ksm.sleep_millisecs),
        json::quote(advisor),
        optional(ksm.general_profit_bytes),
        optional(ksm.pages_sharing),
        optional(ksm.pages_shared),
        optional(ksm.pages_unshared),
        optional(ksm.pages_volatile),
        optional(ksm.pages_skipped),
        optional(ksm.zero_pages),
        optional(ksm.full_scans),
        optional(ksm.pages_scanned),
        json::strings(&ksm.errors),
    )
}

pub fn report_json(report: &Report) -> String {
    let tasks = report.tasks.tasks.iter().map(|task| format!(
        "{{\"pid\":{},\"start_ticks\":{},\"cgroup\":{},\"merge_any\":{},\"merging_pages\":{},\"profit_bytes\":{},\"rmap_items\":{},\"zero_pages\":{}}}",
        task.pid, task.start_ticks, json::quote(&task.cgroup), optional(task.ksm.merge_any),
        optional(task.ksm.merging_pages), optional(task.ksm.profit_bytes),
        optional(task.ksm.rmap_items), optional(task.ksm.zero_pages),
    )).collect::<Vec<_>>().join(",");
    format!(
        "{{\"enabled\":{},\"state\":{},\"target_state\":{},\"reason\":{},\"control_owned\":{},\"write_succeeded\":{},\"registration\":\"explicit_run_dedup\",\"matching_domain\":\"system_wide\",\"hard_cpu_quota\":false,\"task_observation_age_ms\":{},\"task_refresh_interval_ms\":{},\"coverage_complete\":{},\"truncated\":{},\"processes_examined\":{},\"marked_processes\":{},\"registered_tasks\":{},\"tasks\":[{}],\"errors\":{}}}",
        report.enabled,
        json::quote(report.state),
        json::quote(report.target_state),
        json::quote(report.reason),
        report.control_owned,
        optional(report.write_succeeded),
        optional(report.task_age_ms),
        TASK_REFRESH_MS,
        report.tasks.complete,
        report.tasks.truncated,
        report.tasks.processes_examined,
        report.tasks.marked_processes,
        report.tasks.tasks.len(),
        tasks,
        json::strings(&report.tasks.errors),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(run: u8) -> dedup::KsmSnapshot {
        dedup::KsmSnapshot {
            available: Some(true),
            complete: true,
            run: Some(run),
            pages_to_scan: Some(dedup::PAGES_TO_SCAN),
            sleep_millisecs: Some(dedup::SLEEP_MILLISECS),
            advisor: dedup::Advisor::None,
            ..dedup::KsmSnapshot::default()
        }
    }

    fn owned(run: u8) -> dedup::Ownership {
        dedup::Ownership {
            run: Some(run),
            pages_to_scan: Some(dedup::PAGES_TO_SCAN),
            sleep_millisecs: Some(dedup::SLEEP_MILLISECS),
        }
    }

    fn healthy() -> dedup::PressureInput {
        dedup::PressureInput {
            cpu_busy: Some(false),
            memory_busy: Some(false),
            io_busy: Some(false),
        }
    }

    fn complete_observation(run: u8) -> dedup::Observation {
        dedup::Observation {
            ksm: snapshot(run),
            task_age_ms: Some(0),
            tasks: dedup::TaskScan {
                complete: true,
                tasks: vec![dedup::RegisteredTask {
                    pid: 123,
                    start_ticks: 456,
                    cgroup: "/zmem.slice/zmem-background.slice/zmem-background-dedup-123.scope"
                        .into(),
                    ksm: dedup::ProcessKsm {
                        merge_any: Some(true),
                        ..dedup::ProcessKsm::default()
                    },
                }],
                ..dedup::TaskScan::default()
            },
        }
    }

    #[test]
    fn failed_sample_requests_only_an_owned_pause_and_reports_it_as_pending() {
        let mut state = State::new(2000);
        let plan = state.sample_error_plan(snapshot(1), Ok(owned(1))).unwrap();
        assert_eq!(plan.state, dedup::State::Scanning);
        assert_eq!(plan.target_state, dedup::State::PausedPressure);
        assert_eq!(
            plan.writes,
            [dedup::Write {
                knob: dedup::Knob::Run,
                expected: 1,
                value: 0,
            }]
        );
        let unowned = state
            .sample_error_plan(snapshot(1), Ok(dedup::Ownership::default()))
            .unwrap();
        assert_eq!(unowned.state, dedup::State::ExternallyManaged);
        assert!(unowned.writes.is_empty());
    }

    #[test]
    fn failed_sample_preserves_external_advisor_and_previous_external_latch() {
        let mut state = State::new(2000);
        let mut external = snapshot(1);
        external.advisor = dedup::Advisor::External("scan-time".into());
        let plan = state.sample_error_plan(external, Ok(owned(1))).unwrap();
        assert_eq!(plan.state, dedup::State::ExternallyManaged);
        assert!(plan.writes.is_empty());

        state.governor.feedback(dedup::ApplyOutcome::ExternalChange);
        let plan = state.sample_error_plan(snapshot(1), Ok(owned(1))).unwrap();
        assert_eq!(plan.state, dedup::State::ExternallyManaged);
        assert!(plan.writes.is_empty());
    }

    #[test]
    fn failed_sample_preserves_changed_owned_knobs_and_unknown_ownership() {
        let mut state = State::new(2000);
        let mut changed = snapshot(1);
        changed.pages_to_scan = Some(dedup::PAGES_TO_SCAN + 1);
        let plan = state.sample_error_plan(changed, Ok(owned(1))).unwrap();
        assert_eq!(plan.state, dedup::State::ExternallyManaged);
        assert!(plan.writes.is_empty());

        let mut state = State::new(2000);
        assert!(
            state
                .sample_error_plan(snapshot(1), Err("ownership could not be verified".into()))
                .is_err()
        );
        let plan = state.sample_error_plan(snapshot(1), Ok(owned(1))).unwrap();
        assert_eq!(plan.state, dedup::State::ExternallyManaged);
        assert!(plan.writes.is_empty());
    }

    #[test]
    fn failed_sample_resets_recovery_without_claiming_a_successful_pause() {
        let mut state = State::new(2000);
        let observation = complete_observation(0);
        for now in (0..=28_000).step_by(2000) {
            state.last_update_ms = now;
            state
                .governor
                .plan(now, true, &observation, healthy(), owned(0));
        }
        let plan = state.sample_error_plan(snapshot(0), Ok(owned(0))).unwrap();
        assert!(plan.writes.is_empty());
        for now in (30_000..=58_000).step_by(2000) {
            let plan = state
                .governor
                .plan(now, true, &observation, healthy(), owned(0));
            assert!(plan.writes.is_empty());
        }
        let plan = state
            .governor
            .plan(60_000, true, &observation, healthy(), owned(0));
        assert_eq!(plan.state, dedup::State::Warming);
        assert_eq!(plan.writes.last().map(|write| write.value), Some(1));

        state.governor.feedback(dedup::ApplyOutcome::Failed);
        let plan = state.sample_error_plan(snapshot(1), Ok(owned(1))).unwrap();
        assert_eq!(plan.state, dedup::State::Scanning);
        assert_eq!(plan.target_state, dedup::State::PausedPressure);
    }
}
