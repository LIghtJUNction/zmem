#[cfg(target_os = "linux")]
mod admission;
#[cfg(target_os = "linux")]
mod budget;
#[cfg(target_os = "linux")]
mod config;
#[cfg(target_os = "linux")]
mod dedup;
#[cfg(target_os = "linux")]
mod install;
#[cfg(target_os = "linux")]
mod io_delegation;
#[cfg(target_os = "linux")]
mod json;
#[cfg(target_os = "linux")]
mod linux;
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const HELP: &str = "zmem — pressure-aware memory and swap management

Usage:
  zmem status [--json] [--processes] [--live]
  zmem plan [--json]
  zmem doctor [--json]
  zmem schema
  zmem daemon [--observe] [--config PATH]
  zmem once [--apply] [--config PATH]
  zmem run --class background|interactive|disposable [--wait-timeout SECONDS] [--max-memory MiB] [--dedup] -- COMMAND...
  sudo zmem install --replace [--config PATH]
  sudo zmem rollback

Global: --config PATH, --help, --version
JSON byte counts are bytes; PSI basis points use 100 = 1%.
Status uses the active daemon's report; --processes adds a fresh process list.
--live and plan are independent previews: they do not apply their decisions,
do not retain the daemon's policy history, and cannot measure rates from one sample.
The no_std library is portable. Live system management requires Linux + systemd + cgroup v2.
Background tasks are throttled; disposable tasks explicitly opt into a hard memory limit.
Background/disposable starts wait for real memory headroom (default 60s, timeout exit 75).
--wait-timeout accepts 0..3600 seconds; 0 checks once. Interactive starts bypass admission.
After observed pressure, recovery must remain stable for 3s; per-UID reservations are spaced by 3s.
Admission covers only commands launched through run, not arbitrary terminal descendants.
--dedup opts trusted background/disposable commands into automatic KSM scanning.
KSM matching is global; only choose commands whose data may share this trust domain.
";

#[cfg(target_os = "linux")]
fn execute() -> Result<()> {
    use std::path::{Path, PathBuf};
    let raw: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if raw.first().is_some_and(|arg| arg == "run") {
        if raw[1..]
            .iter()
            .take_while(|arg| *arg != "--")
            .any(|arg| arg == "--help" || arg == "-h")
        {
            print!("{HELP}");
            return Ok(());
        }
        return run_cli::execute(run_cli::parse(&raw[1..])?);
    }
    if raw.first().is_some_and(|arg| arg == "__dedup-exec") {
        if raw.get(1).is_none_or(|arg| arg != "--") {
            return Err("dedup exec requires -- before the command".into());
        }
        return dedup::exec_registered(&raw[2..]).map_err(Into::into);
    }
    let mut args: Vec<String> = raw
        .into_iter()
        .map(|arg| {
            arg.into_string()
                .map_err(|_| "non-UTF-8 argument outside run COMMAND".into())
        })
        .collect::<Result<_>>()?;
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" {
        print!("{HELP}");
        return Ok(());
    }
    if args[0] == "--version" || args[0] == "-V" {
        println!("zmem {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let action = args.remove(0);
    let mut configuration = PathBuf::from("/etc/zmem/zmem.conf");
    let mut custom_config = false;
    let boundary = args.iter().position(|v| v == "--").unwrap_or(args.len());
    if let Some(index) = args[..boundary].iter().position(|v| v == "--config") {
        if index + 1 >= boundary {
            return Err("--config requires a path".into());
        }
        configuration = PathBuf::from(args.remove(index + 1));
        args.remove(index);
        custom_config = true;
    }
    let flag = |name: &str| args.iter().any(|v| v == name);
    if flag("--help") && action != "run" {
        print!("{HELP}");
        return Ok(());
    }
    if action == "install" {
        if args.iter().any(|a| a != "--replace") {
            return Err("install accepts --replace and --config PATH".into());
        }
        if custom_config {
            config::Settings::read(&configuration)?;
        }
        if Path::new("/etc/zmem/zmem.conf").exists() {
            config::Settings::read(Path::new("/etc/zmem/zmem.conf"))?;
        }
        return install::install(
            custom_config.then_some(configuration.as_path()),
            flag("--replace"),
        );
    }
    if action == "rollback" {
        if !args.is_empty() {
            return Err("rollback accepts no arguments".into());
        }
        return install::rollback();
    }
    if matches!(action.as_str(), "__swap-on" | "__swap-off") {
        return linux::swap_helper(&action, &args);
    }
    if action == "status" || action == "plan" {
        if args
            .iter()
            .any(|a| !["--json", "--processes", "--live"].contains(&a.as_str()))
        {
            return Err("unknown status/plan argument".into());
        }
        return status::show(
            &action,
            flag("--json"),
            flag("--processes"),
            flag("--live"),
            &configuration,
            custom_config,
        );
    }
    let settings = if configuration.exists() {
        config::Settings::read(&configuration)?
    } else if custom_config {
        return Err(format!("config does not exist: {}", configuration.display()).into());
    } else {
        config::Settings::default()
    };
    match action.as_str() {
        "doctor" => {
            if args.iter().any(|a| a != "--json") {
                return Err("doctor accepts --json".into());
            }
            let (result, ok) = linux::doctor_json();
            let mut result = json::parse(&result)?;
            result.set("user_io", io_delegation::diagnostics())?;
            println!("{result}");
            if ok {
                Ok(())
            } else {
                Err("required Linux memory-management facilities are unavailable".into())
            }
        }
        "schema" => {
            if !args.is_empty() {
                return Err("schema accepts no arguments".into());
            }
            println!(
                "{{\"schema\":\"zmem.commands.v1\",\"version\":{},\"transport\":\"cli\",\"read_only\":[{{\"name\":\"status\",\"argv\":[\"zmem\",\"status\",\"--json\"],\"description\":\"Current memory, PSI, swap, zram and policy; counts in bytes, PSI in basis points\"}},{{\"name\":\"processes\",\"argv\":[\"zmem\",\"status\",\"--json\",\"--processes\"]}},{{\"name\":\"plan\",\"argv\":[\"zmem\",\"plan\",\"--json\"]}},{{\"name\":\"doctor\",\"argv\":[\"zmem\",\"doctor\",\"--json\"]}}],\"mutations\":[{{\"name\":\"run\",\"root\":false,\"classes\":[\"background\",\"interactive\",\"disposable\"],\"hard_limit_opt_in_class\":\"disposable\",\"dedup_opt_in_flag\":\"--dedup\",\"dedup_classes\":[\"background\",\"disposable\"],\"dedup_matching_domain\":\"system_wide\",\"admission\":{{\"classes\":[\"background\",\"disposable\"],\"interactive_bypass\":true,\"wait_timeout_flag\":\"--wait-timeout\",\"default_wait_timeout_seconds\":60,\"max_wait_timeout_seconds\":3600,\"zero_timeout\":\"one immediate attempt\",\"timeout_exit_code\":75,\"signal_exit_codes\":[130,143],\"inherited_ignored_signals_preserved\":true,\"recovery_window_seconds\":3,\"per_uid_reservation_interval_seconds\":3,\"events\":\"stderr JSON; no command arguments\",\"coverage\":\"explicit run invocations only\"}}}},{{\"name\":\"once\",\"root\":true,\"required_flag\":\"--apply\"}},{{\"name\":\"install\",\"root\":true,\"replacement_flag\":\"--replace\"}},{{\"name\":\"rollback\",\"root\":true}}],\"status_semantics\":{{\"default\":\"identity-verified daemon report when available\",\"live_flag\":\"--live\",\"live_and_plan\":\"independent preview; no daemon hysteresis history\",\"rate_unavailable\":\"null, never inferred zero\",\"controls\":\"requested and actual readback are separate\",\"coverage\":\"background pacing covers registered zmem groups only\"}},\"automatic_process_kill\":false,\"automatic_global_cache_drop\":false}}",
                json::quote(env!("CARGO_PKG_VERSION"))
            );
            Ok(())
        }
        "daemon" => {
            if args.iter().any(|a| a != "--observe") {
                return Err("daemon accepts --observe and --config PATH".into());
            }
            linux::daemon(settings, configuration, flag("--observe"), false)
        }
        "once" => {
            if args.iter().any(|a| a != "--apply" && a != "--json") {
                return Err("once accepts --apply, --json and --config PATH".into());
            }
            linux::daemon(settings, configuration, !flag("--apply"), true)
        }
        "__grow" => {
            if args.len() != 1 {
                return Err("invalid growth request".into());
            }
            linux::grow_swap(&settings, args[0].parse()?)
        }
        "__shrink" => {
            if args.len() != 1 {
                return Err("invalid shrink request".into());
            }
            linux::shrink_swap(&settings, Path::new(&args[0]))
        }
        "__maintain" => {
            if args.len() != 2 {
                return Err("invalid maintenance request".into());
            }
            linux::maintain_swap(&settings, args[0].parse()?, &args[1])
        }
        "__reclaim" => {
            if args.len() != 2 {
                return Err("invalid reclaim request".into());
            }
            linux::reclaim(Path::new(&args[0]), args[1].parse()?)
        }
        _ => Err(format!("unknown command: {action}; use zmem --help").into()),
    }
}

#[cfg(target_os = "linux")]
mod run_cli {
    use crate::{Result, admission, config, linux};
    use std::{collections::BTreeSet, ffi::OsString, path::PathBuf};

    #[derive(Debug)]
    pub struct Arguments {
        pub class: String,
        pub max_mib: Option<u64>,
        pub dedup: bool,
        pub timeout_secs: u64,
        pub configuration: Option<PathBuf>,
        pub command: Vec<OsString>,
    }
    pub fn parse(args: &[OsString]) -> Result<Arguments> {
        let split = args
            .iter()
            .position(|arg| arg == "--")
            .ok_or("run requires -- before the command")?;
        if args.get(split + 1).is_none_or(|arg| arg.is_empty()) {
            return Err("run requires a nonempty command after --".into());
        }
        let mut options = Arguments {
            class: "background".into(),
            max_mib: None,
            dedup: false,
            timeout_secs: admission::DEFAULT_WAIT_TIMEOUT_SECS,
            configuration: None,
            command: args[split + 1..].to_vec(),
        };
        let mut seen = BTreeSet::new();
        let mut index = 0;
        while index < split {
            let option = args[index]
                .to_str()
                .ok_or("run option names must be UTF-8")?;
            if !seen.insert(option) {
                return Err(format!("duplicate run option: {option}").into());
            }
            if option == "--dedup" {
                options.dedup = true;
                index += 1;
                continue;
            }
            if !["--class", "--max-memory", "--wait-timeout", "--config"].contains(&option) {
                return Err(format!("unknown run option: {option}").into());
            }
            index += 1;
            let value = args
                .get(index)
                .filter(|_| index < split)
                .ok_or_else(|| format!("{option} requires a value"))?;
            if option == "--config" {
                if value.is_empty() {
                    return Err("--config requires a nonempty path".into());
                }
                options.configuration = Some(PathBuf::from(value));
            } else if option == "--class" {
                options.class = value.to_str().ok_or("class must be UTF-8")?.into();
            } else {
                let value = value
                    .to_str()
                    .ok_or("run numeric options require decimal integers")?;
                if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(format!("{option} requires a nonnegative decimal integer").into());
                }
                let value: u64 = value.parse()?;
                if option == "--wait-timeout" {
                    if value > admission::MAX_WAIT_TIMEOUT_SECS {
                        return Err("--wait-timeout must be 0..3600 seconds".into());
                    }
                    options.timeout_secs = value;
                } else {
                    options.max_mib = Some(value);
                }
            }
            index += 1;
        }
        if !["background", "interactive", "disposable"].contains(&options.class.as_str()) {
            return Err("class must be background, interactive or disposable".into());
        }
        if options.max_mib.is_some() && options.class != "disposable" {
            return Err("--max-memory applies to explicit disposable workloads".into());
        }
        if options.max_mib.is_some_and(|value| value < 16) {
            return Err("--max-memory must be at least 16 MiB".into());
        }
        if options.dedup && options.class == "interactive" {
            return Err(
                "--dedup is available only for trusted background/disposable workloads".into(),
            );
        }
        Ok(options)
    }
    pub fn execute(options: Arguments) -> Result<()> {
        let path = options
            .configuration
            .clone()
            .unwrap_or_else(|| PathBuf::from("/etc/zmem/zmem.conf"));
        let settings = if path.exists() {
            config::Settings::read(&path)?
        } else if options.configuration.is_some() {
            return Err(format!("config does not exist: {}", path.display()).into());
        } else {
            config::Settings::default()
        };
        linux::run_workload(
            &settings,
            &options.class,
            options.max_mib,
            options.dedup,
            options.timeout_secs,
            &options.command,
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::ffi::OsStringExt;

        fn arguments(values: &[&str]) -> Vec<OsString> {
            values.iter().map(OsString::from).collect()
        }

        #[test]
        fn admission_cli_defaults_and_interactive_bypass_timeout_remain_explicit() {
            let options = parse(&arguments(&["--", "command"])).unwrap();
            assert_eq!(options.class, "background");
            assert_eq!(options.timeout_secs, 60);
            let options = parse(&arguments(&[
                "--class",
                "interactive",
                "--wait-timeout",
                "0",
                "--",
                "command",
            ]))
            .unwrap();
            assert_eq!(options.class, "interactive");
            assert_eq!(options.timeout_secs, 0);
        }

        #[test]
        fn admission_cli_separator_protects_raw_command_arguments_and_paths() {
            let raw = OsString::from_vec(vec![b'x', 0xff]);
            let args = vec![
                "--config".into(),
                raw.clone(),
                "--wait-timeout".into(),
                "300".into(),
                "--".into(),
                "command".into(),
                "--class".into(),
                "--wait-timeout".into(),
                "--help".into(),
                "${VALUE}".into(),
                raw.clone(),
            ];
            let options = parse(&args).unwrap();
            assert_eq!(options.configuration, Some(PathBuf::from(&raw)));
            assert_eq!(options.timeout_secs, 300);
            assert_eq!(options.command, args[5..]);
            assert_eq!(options.class, "background");
        }

        #[test]
        fn admission_cli_rejects_invalid_options_before_any_wait_or_reservation() {
            for values in [
                vec!["--wait-timeout", "-1", "--", "command"],
                vec!["--wait-timeout", "3601", "--", "command"],
                vec!["--wait-timeout", "18446744073709551616", "--", "command"],
                vec![
                    "--wait-timeout",
                    "0",
                    "--wait-timeout",
                    "1",
                    "--",
                    "command",
                ],
                vec!["--wait-timeout", "--", "command"],
                vec!["--class", "unrecognized", "--", "command"],
                vec!["--class", "interactive", "--dedup", "--", "command"],
                vec!["--max-memory", "16", "--", "command"],
                vec![
                    "--class",
                    "disposable",
                    "--max-memory",
                    "0",
                    "--",
                    "command",
                ],
                vec!["--"],
                vec!["--", ""],
                vec!["command"],
            ] {
                assert!(parse(&arguments(&values)).is_err(), "{values:?}");
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod status {
    use crate::{
        Result, config,
        json::{self, Value},
        linux,
    };
    use std::{
        fmt::Write as _,
        fs::{self, File},
        io::Read,
        os::unix::fs::MetadataExt,
        path::Path,
        time::{SystemTime, UNIX_EPOCH},
    };
    use zmem::{GIB, MIB, Policy};

    const CACHE: &str = "/run/zmem/status.json";
    const LEGACY_MAX_AGE_MS: u64 = 10_000;
    const RATE_FIELDS: &[&str] = &[
        "file_refault_bytes_per_sec",
        "anon_refault_bytes_per_sec",
        "direct_scan_bytes_per_sec",
        "zram_read_bytes_per_sec",
        "estimated_disk_swap_in_bytes_per_sec",
    ];

    struct CacheInfo {
        written_at_ms: u64,
        verification: &'static str,
    }

    pub fn show(
        action: &str,
        json_output: bool,
        processes: bool,
        live: bool,
        configuration: &Path,
        custom_config: bool,
    ) -> Result<()> {
        let mut cache_error = None;
        if action == "status" && !live {
            match read_daemon_cache() {
                Ok((mut report, info)) => {
                    let fresh_processes = read_processes(processes)?;
                    decorate_cache(&mut report, &info, fresh_processes, now_ms()?)?;
                    // Reading actual daemon state does not depend on whether a
                    // later on-disk configuration edit is valid or was reloaded.
                    if custom_config {
                        report
                            .get_mut("observation")
                            .unwrap()
                            .set("config_override", "not_applied_to_daemon_report".into())?;
                    }
                    return emit(&report, json_output, processes);
                }
                Err(error) => cache_error = Some(error.to_string()),
            }
        }
        let settings = if configuration.exists() {
            config::Settings::read(configuration)?
        } else if custom_config {
            return Err(format!("config does not exist: {}", configuration.display()).into());
        } else {
            config::Settings::default()
        };
        let telemetry = linux::Sampler::new().sample()?;
        let decision = Policy::new(settings.policy).observe(telemetry.sample, 0);
        let mut report = json::parse(&linux::telemetry_json(
            &telemetry,
            &decision,
            &settings.profile,
            None,
            &[],
            false,
        ))?;
        let fresh_processes = read_processes(processes)?;
        decorate_preview(
            &mut report,
            action,
            cache_error.as_deref(),
            fresh_processes,
            now_ms()?,
        )?;
        emit(&report, json_output, processes)
    }

    fn emit(report: &Value, json_output: bool, processes: bool) -> Result<()> {
        if json_output {
            println!("{report}");
        } else {
            print!("{}", human_report(report, processes));
        }
        Ok(())
    }

    fn now_ms() -> Result<u64> {
        millis(SystemTime::now())
    }

    fn millis(time: SystemTime) -> Result<u64> {
        Ok(u64::try_from(time.duration_since(UNIX_EPOCH)?.as_millis())?)
    }

    fn field<'a>(report: &'a Value, path: &[&str]) -> Option<&'a Value> {
        path.iter().try_fold(report, |value, key| value.get(key))
    }

    fn number(report: &Value, path: &[&str]) -> Option<u64> {
        field(report, path).and_then(Value::as_u64)
    }

    fn text<'a>(report: &'a Value, path: &[&str]) -> Option<&'a str> {
        field(report, path).and_then(Value::as_str)
    }

    fn sample_age(report: &Value, now: u64) -> Result<u64> {
        let sampled = number(report, &["sampled_at_unix"])
            .and_then(|value| value.checked_mul(1000))
            .ok_or("daemon report has no valid sample timestamp")?;
        now.checked_sub(sampled)
            .ok_or_else(|| "daemon sample timestamp is in the future".into())
    }

    fn read_daemon_cache() -> Result<(Value, CacheInfo)> {
        let link = fs::symlink_metadata(CACHE)?;
        if !link.is_file() || link.uid() != 0 || link.mode() & 0o022 != 0 {
            return Err(
                "daemon report must be a root-owned regular file without group/other writes".into(),
            );
        }
        let file = File::open(CACHE)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err("daemon report ownership changed".into());
        }
        let mut contents = String::new();
        file.take(1024 * 1024 + 1).read_to_string(&mut contents)?;
        let report = json::parse(&contents)?;
        if text(&report, &["schema"]) != Some("zmem.status.v1") {
            return Err("unsupported daemon report schema".into());
        }
        let pid = number(&report, &["pid"])
            .filter(|pid| *pid != 0 && *pid <= u32::MAX as u64)
            .ok_or("daemon report has no valid PID")?;
        let written_at_ms = millis(metadata.modified()?)?;
        let now = now_ms()?;
        let age = sample_age(&report, now)?;
        let cache_age = now
            .checked_sub(written_at_ms)
            .ok_or("daemon cache timestamp is in the future")?;
        let verification = if report.get("instance").is_some() {
            let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
            let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
            verify_instance(&report, &stat, boot.trim())?;
            "proc_identity"
        } else {
            // Old reports lack boot/start identity. Accept only recent reports
            // whose PID is still the active system service, with a short bound.
            if age > LEGACY_MAX_AGE_MS || cache_age > LEGACY_MAX_AGE_MS {
                return Err(
                    "legacy daemon report is older than 10 seconds and has no instance identity"
                        .into(),
                );
            }
            let unit = linux::command(
                "systemctl",
                &[
                    "show",
                    "zmem.service",
                    "--property=ActiveState,MainPID",
                    "--no-pager",
                ],
                1,
            )?;
            if !legacy_service_matches(&unit, pid) {
                return Err("legacy report PID does not match the active zmem service".into());
            }
            "systemd_legacy"
        };
        Ok((
            report,
            CacheInfo {
                written_at_ms,
                verification,
            },
        ))
    }

    fn verify_instance(report: &Value, stat: &str, boot: &str) -> Result<()> {
        let pid = number(report, &["instance", "pid"]).ok_or("missing instance PID")?;
        let ticks =
            number(report, &["instance", "start_ticks"]).ok_or("missing instance start time")?;
        let saved_boot =
            text(report, &["instance", "boot_id"]).ok_or("missing instance boot ID")?;
        if pid == 0
            || number(report, &["pid"]) != Some(pid)
            || saved_boot.is_empty()
            || saved_boot != boot
        {
            return Err("daemon cache belongs to a different PID or boot".into());
        }
        let prefix = stat.split_once('(').ok_or("malformed process identity")?.0;
        let suffix = stat.rsplit_once(')').ok_or("malformed process identity")?.1;
        let fields: Vec<_> = suffix.split_whitespace().collect();
        let proc_pid = prefix.trim().parse::<u64>()?;
        let proc_ticks = fields
            .get(19)
            .ok_or("missing process start time")?
            .parse::<u64>()?;
        if proc_pid != pid
            || proc_ticks != ticks
            || fields
                .first()
                .is_none_or(|state| matches!(*state, "Z" | "X" | "x"))
        {
            return Err("daemon cache process has exited or its PID was reused".into());
        }
        Ok(())
    }

    fn legacy_service_matches(properties: &str, pid: u64) -> bool {
        let property = |name: &str| {
            properties.lines().find_map(|line| {
                let (key, value) = line.split_once('=')?;
                (key == name).then_some(value)
            })
        };
        matches!(property("ActiveState"), Some("active" | "reloading"))
            && property("MainPID").and_then(|value| value.parse::<u64>().ok()) == Some(pid)
    }

    fn read_processes(requested: bool) -> Result<Option<Value>> {
        if !requested {
            return Ok(None);
        }
        let processes = json::parse(&linux::top_processes(20))?;
        if !matches!(processes, Value::Array(_)) {
            return Err("process report is not an array".into());
        }
        Ok(Some(processes))
    }

    fn decorate_cache(
        report: &mut Value,
        info: &CacheInfo,
        processes: Option<Value>,
        now: u64,
    ) -> Result<()> {
        let age = sample_age(report, now)?;
        let cache_age = now
            .checked_sub(info.written_at_ms)
            .ok_or("daemon cache timestamp is in the future")?;
        let stale_after = number(report, &["paging", "sample_interval_ms"])
            .unwrap_or(0)
            .saturating_mul(3)
            .max(LEGACY_MAX_AGE_MS);
        let rates = match field(report, &["paging", "rates_available"]).and_then(Value::as_bool) {
            Some(true) => "measured",
            Some(false) => "warming_up",
            None => "legacy_unspecified",
        };
        let processes_requested = processes.is_some();
        report.set(
            "observation",
            json::object([
                ("source", "daemon_cache".into()),
                ("decision", "daemon_reported".into()),
                ("daemon_active", true.into()),
                ("instance_verification", info.verification.into()),
                ("sample_age_ms", age.into()),
                ("cache_age_ms", cache_age.into()),
                (
                    "stale",
                    (age > stale_after || cache_age > stale_after).into(),
                ),
                ("stale_after_ms", stale_after.into()),
                ("rates", rates.into()),
                (
                    "processes_source",
                    if processes_requested {
                        "live_procfs"
                    } else {
                        "not_requested"
                    }
                    .into(),
                ),
                (
                    "processes_sampled_at_unix",
                    if processes_requested {
                        (now / 1000).into()
                    } else {
                        Value::Null
                    },
                ),
            ]),
        )?;
        if let Some(processes) = processes {
            report.set("processes", processes)?;
        }
        Ok(())
    }

    fn decorate_preview(
        report: &mut Value,
        action: &str,
        cache_error: Option<&str>,
        processes: Option<Value>,
        now: u64,
    ) -> Result<()> {
        // A first sample has cumulative counters, not a measured rate. Null
        // prevents an unprimed sampler's zero from looking like idle swapping.
        if let Some(swap) = report.get_mut("swap") {
            swap.set("in_bytes_per_sec", Value::Null)?;
            swap.set("out_bytes_per_sec", Value::Null)?;
        }
        if report.get("paging").is_none() {
            report.set("paging", json::object([]))?;
        }
        let paging = report.get_mut("paging").ok_or("missing paging report")?;
        paging.set("rates_available", false.into())?;
        paging.set("sample_interval_ms", 0u64.into())?;
        for key in RATE_FIELDS {
            paging.set(key, Value::Null)?;
        }
        let processes_requested = processes.is_some();
        let age = sample_age(report, now)?;
        report.set(
            "observation",
            json::object([
                (
                    "source",
                    if action == "plan" {
                        "independent_plan"
                    } else {
                        "independent_snapshot"
                    }
                    .into(),
                ),
                ("decision", "preview_not_applied".into()),
                (
                    "policy_history",
                    "new_policy_without_daemon_hysteresis".into(),
                ),
                ("daemon_active", Value::Null),
                ("sample_age_ms", age.into()),
                ("cache_age_ms", Value::Null),
                ("stale", false.into()),
                ("rates", "warming_up".into()),
                (
                    "daemon_cache_unavailable",
                    cache_error.map(Value::from).unwrap_or(Value::Null),
                ),
                (
                    "processes_source",
                    if processes_requested {
                        "live_procfs"
                    } else {
                        "not_requested"
                    }
                    .into(),
                ),
                (
                    "processes_sampled_at_unix",
                    if processes_requested {
                        (now / 1000).into()
                    } else {
                        Value::Null
                    },
                ),
            ]),
        )?;
        if let Some(processes) = processes {
            report.set("processes", processes)?;
        }
        Ok(())
    }

    fn scaled(value: Option<u64>, scale: u64, unit: &str) -> String {
        value
            .map(|number| format!("{:.2} {unit}", number as f64 / scale as f64))
            .unwrap_or_else(|| "not reported".to_owned())
    }

    fn human_report(report: &Value, show_processes: bool) -> String {
        let mut out = String::new();
        let source = text(report, &["observation", "source"]).unwrap_or("unknown");
        let cached = source == "daemon_cache";
        let _ = writeln!(
            out,
            "zmem {} · {}",
            text(report, &["version"]).unwrap_or("unknown"),
            text(report, &["profile"]).unwrap_or("unknown profile")
        );
        let age = scaled(number(report, &["observation", "sample_age_ms"]), 1000, "s");
        if cached {
            let stale = field(report, &["observation", "stale"])
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let _ = writeln!(
                out,
                "State             active daemon report · sample age {age}{}",
                if stale {
                    " · STALE: this is its last report"
                } else {
                    ""
                }
            );
            if field(report, &["observation", "config_override"]).is_some() {
                let _ = writeln!(
                    out,
                    "Configuration     daemon's reported profile; --config only affects independent previews"
                );
            }
        } else {
            let _ = writeln!(out, "State             {source} · sample age {age}");
            let _ = writeln!(
                out,
                "Decision          preview only, not applied; daemon hysteresis, worker and errors are not read"
            );
            if let Some(reason) = text(report, &["observation", "daemon_cache_unavailable"]) {
                let _ = writeln!(
                    out,
                    "Daemon report     unavailable: {}",
                    json::quote(reason)
                );
            }
        }
        let memory_pressure =
            text(report, &["memory_pressure"]).unwrap_or("not reported separately");
        let _ = writeln!(out, "Memory pressure   {memory_pressure}");
        if report.get("memory_pressure").is_none() {
            let _ = writeln!(
                out,
                "Legacy policy     {} (does not distinguish memory pressure from I/O)",
                text(report, &["pressure"]).unwrap_or("not reported")
            );
        }
        let io = match field(report, &["io_congested"]).and_then(Value::as_bool) {
            Some(true) => "congested",
            Some(false) => "not congested",
            None => "not reported separately",
        };
        let _ = writeln!(
            out,
            "I/O congestion    {io} · full PSI {}",
            scaled(number(report, &["psi", "io_full_bp"]), 100, "%")
        );
        let _ = writeln!(
            out,
            "Memory            {} available / {} total",
            scaled(number(report, &["memory", "available_bytes"]), GIB, "GiB"),
            scaled(number(report, &["memory", "total_bytes"]), GIB, "GiB")
        );
        let _ = writeln!(
            out,
            "Memory stalls     some {} · full {}",
            scaled(number(report, &["psi", "memory_some_bp"]), 100, "%"),
            scaled(number(report, &["psi", "memory_full_bp"]), 100, "%")
        );
        let total_swap = number(report, &["swap", "total_bytes"]);
        let used_swap = total_swap
            .zip(number(report, &["swap", "free_bytes"]))
            .map(|(total, free)| total.saturating_sub(free));
        let _ = writeln!(
            out,
            "Swap              {} used / {} total",
            scaled(used_swap, GIB, "GiB"),
            scaled(total_swap, GIB, "GiB")
        );
        let warming = text(report, &["observation", "rates"]) == Some("warming_up");
        if warming {
            let _ = writeln!(
                out,
                "Paging rates      warming up: one sample cannot measure rates; use daemon status for measured rates"
            );
        } else {
            let _ = writeln!(
                out,
                "Swap rates        in {} · out {}{}",
                scaled(number(report, &["swap", "in_bytes_per_sec"]), MIB, "MiB/s"),
                scaled(number(report, &["swap", "out_bytes_per_sec"]), MIB, "MiB/s"),
                if text(report, &["observation", "rates"]) == Some("legacy_unspecified") {
                    " · legacy report does not declare rate readiness"
                } else {
                    ""
                }
            );
            let _ = writeln!(
                out,
                "Paging rates      file refault {} · direct scan {} · estimated disk swap-in {}",
                scaled(
                    number(report, &["paging", "file_refault_bytes_per_sec"]),
                    MIB,
                    "MiB/s"
                ),
                scaled(
                    number(report, &["paging", "direct_scan_bytes_per_sec"]),
                    MIB,
                    "MiB/s"
                ),
                scaled(
                    number(report, &["paging", "estimated_disk_swap_in_bytes_per_sec"]),
                    MIB,
                    "MiB/s"
                )
            );
        }
        let requested_high = number(report, &["plan", "background_high_percent"])
            .map(|value| format!("{value}%"))
            .unwrap_or_else(|| "not reported".to_owned());
        let _ = writeln!(
            out,
            "{}  {} · pressure severity hint {requested_high} · requested swap growth {}",
            if cached {
                "Daemon decision "
            } else {
                "Independent plan"
            },
            text(report, &["reason"]).unwrap_or("not reported"),
            scaled(number(report, &["plan", "grow_swap_bytes"]), MIB, "MiB")
        );
        if let Some(background) = field(report, &["background"]) {
            let requested =
                if background.get("plan_available").and_then(Value::as_bool) == Some(true) {
                    number(background, &["requested_budget_bytes"])
                        .map(|bytes| scaled(Some(bytes), MIB, "MiB"))
                        .unwrap_or_else(|| "no temporary ceiling".to_owned())
                } else {
                    "not calculated in this report".to_owned()
                };
            let actual =
                if background.get("readback_complete").and_then(Value::as_bool) == Some(true) {
                    number(background, &["actual_budget_bytes"])
                        .map(|bytes| scaled(Some(bytes), MIB, "MiB"))
                        .unwrap_or_else(|| "unlimited".to_owned())
                } else {
                    "unavailable".to_owned()
                };
            let _ = writeln!(
                out,
                "Background budget requested {requested} · active-group readback {actual}"
            );
        }
        match number(report, &["controls", "swappiness_actual"]) {
            Some(actual) => {
                let requested = number(report, &["controls", "swappiness_requested"])
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "not reported".to_owned());
                let page_cluster = number(report, &["controls", "page_cluster_actual"])
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "not reported".to_owned());
                let _ = writeln!(
                    out,
                    "Actual controls   swappiness {actual} · page-cluster {page_cluster}; {} swappiness {requested}",
                    if cached {
                        "daemon requested"
                    } else {
                        "preview requested"
                    }
                );
                if let Some(reason) = text(report, &["controls", "swappiness_reason"]) {
                    let _ = writeln!(out, "Control reason    {reason}");
                }
            }
            None => {
                let _ = writeln!(
                    out,
                    "Actual controls   not reported; requested policy values are not proof of applied limits"
                );
            }
        }
        if let Some(groups) = number(report, &["managed", "background_groups"]) {
            let populated = number(report, &["managed", "populated_background_groups"]);
            let tasks = number(report, &["managed", "background_tasks"]);
            let count = |value: Option<u64>| {
                value
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "unknown".to_owned())
            };
            let _ = writeln!(
                out,
                "Managed work      {} populated / {groups} groups · {} tasks · {} memory · {} reclaim candidates",
                count(populated),
                count(tasks),
                scaled(
                    number(report, &["managed", "background_memory_bytes"]),
                    GIB,
                    "GiB"
                ),
                count(number(report, &["managed", "reclaim_candidates"]))
            );
            if groups == 0 || populated == Some(0) {
                let _ = writeln!(
                    out,
                    "Coverage          no populated managed background work; background pacing does not cover ordinary applications outside zmem groups"
                );
            } else if tasks == Some(0) {
                let _ = writeln!(
                    out,
                    "Coverage          no managed background tasks reported; populated groups alone do not establish task coverage"
                );
            } else {
                let _ = writeln!(
                    out,
                    "Coverage          background pacing applies to registered zmem groups; it does not imply control of every process"
                );
            }
        } else {
            let _ = writeln!(
                out,
                "Managed coverage  not reported; no task coverage or reclaim benefit can be inferred"
            );
        }
        if let Some(scopes) = number(report, &["controls", "protected_critical_scopes"]) {
            let _ = writeln!(
                out,
                "Protection        {scopes} critical scopes · reported budget {}",
                scaled(
                    number(report, &["controls", "protection_budget_bytes"]),
                    MIB,
                    "MiB"
                )
            );
        }
        if let Some(zram) = field(report, &["zram"]).and_then(Value::as_array) {
            for device in zram {
                let _ = writeln!(
                    out,
                    "{}            logical data {} · resident {}",
                    text(device, &["device"])
                        .or_else(|| text(device, &["name"]))
                        .unwrap_or("zram"),
                    scaled(number(device, &["original_bytes"]), GIB, "GiB"),
                    scaled(number(device, &["resident_bytes"]), GIB, "GiB")
                );
            }
        }
        if let Some(dedup) = field(report, &["dedup"]) {
            let _ = writeln!(
                out,
                "Dedup             {} · {} registered tasks · {}",
                text(dedup, &["state"]).unwrap_or("not reported"),
                number(dedup, &["registered_tasks"])
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "not reported".into()),
                text(dedup, &["reason"]).unwrap_or("not reported")
            );
            let profit = match field(report, &["ksm", "general_profit_bytes"]) {
                Some(Value::Number(value)) => value
                    .parse::<i64>()
                    .ok()
                    .map(|value| format!("{:.2} MiB", value as f64 / MIB as f64)),
                _ => None,
            };
            let _ = writeln!(
                out,
                "KSM profit        {} · system-wide estimate; matching crosses process/user boundaries",
                profit.unwrap_or_else(|| "not reported".into())
            );
        }
        if cached {
            let _ = writeln!(
                out,
                "Daemon worker     {}",
                text(report, &["worker"]).unwrap_or("none in this report")
            );
            if let Some(errors) = field(report, &["errors"]).and_then(Value::as_array) {
                for error in errors {
                    let _ = writeln!(
                        out,
                        "Daemon diagnostic {}",
                        error
                            .as_str()
                            .map(json::quote)
                            .unwrap_or_else(|| error.to_string())
                    );
                }
            }
        }
        if show_processes {
            let _ = writeln!(
                out,
                "Processes         live /proc list; shared RSS may be counted more than once"
            );
            let _ = writeln!(out, "     PID     RSS MiB    Swap MiB  Name");
            if let Some(processes) = field(report, &["processes"]).and_then(Value::as_array) {
                for process in processes {
                    let _ = writeln!(
                        out,
                        "{:>8}  {:>10.2}  {:>10.2}  {}",
                        number(process, &["pid"]).unwrap_or(0),
                        number(process, &["rss_bytes"]).unwrap_or(0) as f64 / MIB as f64,
                        number(process, &["swap_bytes"]).unwrap_or(0) as f64 / MIB as f64,
                        json::quote(text(process, &["name"]).unwrap_or("unknown"))
                    );
                }
            }
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn report() -> Value {
            json::parse(r#"{
                "schema":"zmem.status.v1","version":"test","pid":77,
                "sampled_at_unix":1000,"profile":"balanced",
                "instance":{"pid":77,"start_ticks":42,"boot_id":"boot-A"},
                "pressure":"recovering","memory_pressure":"recovering","reason":"hysteresis",
                "io_congested":true,
                "memory":{"total_bytes":8589934592,"available_bytes":1073741824},
                "swap":{"total_bytes":4294967296,"free_bytes":2147483648,
                    "in_bytes_per_sec":1234567,"out_bytes_per_sec":7654321,"devices":[]},
                "paging":{"rates_available":true,"sample_interval_ms":2000,
                    "file_refault_bytes_per_sec":100,"direct_scan_bytes_per_sec":200},
                "psi":{"memory_some_bp":10,"memory_full_bp":0,"io_full_bp":3500},
                "plan":{"grow_swap_bytes":0,"background_high_percent":70},
                "worker":"reclaim","errors":["diagnostic with } and \"processes\": []"],
                "controls":{"swappiness_actual":80,"swappiness_requested":100,"page_cluster_actual":0},
                "managed":{"background_groups":0,"populated_background_groups":0,
                    "background_tasks":0,"background_memory_bytes":0,"reclaim_candidates":0},
                "zram":[],"processes":[],"future":{"worker":"nested","counter":18446744073709551615}
            }"#).unwrap()
        }

        fn process_stat(pid: u64, state: &str, ticks: u64) -> String {
            let mut fields = vec!["0".to_owned(); 20];
            fields[0] = state.to_owned();
            fields[19] = ticks.to_string();
            format!("{pid} (name with ) parentheses) {}", fields.join(" "))
        }

        #[test]
        fn adding_processes_preserves_daemon_decision_rates_diagnostics_and_future_fields() {
            let before = report();
            let mut after = before.clone();
            let processes = json::parse(r#"[{"pid":88,"name":"brace } and \"errors\": []","rss_bytes":4096,"swap_bytes":0,"cgroup":"/zmem.slice/zmem-background.slice/job.scope","managed_background":true}]"#).unwrap();
            decorate_cache(
                &mut after,
                &CacheInfo {
                    written_at_ms: 1_002_000,
                    verification: "proc_identity",
                },
                Some(processes.clone()),
                1_002_500,
            )
            .unwrap();
            let Value::Object(fields) = &before else {
                panic!("fixture must be an object");
            };
            for (key, value) in fields {
                if key != "processes" {
                    assert_eq!(after.get(key), Some(value), "daemon field {key} changed");
                }
            }
            assert_eq!(after.get("processes"), Some(&processes));
            assert_eq!(
                text(&after, &["observation", "source"]),
                Some("daemon_cache")
            );
            assert_eq!(
                number(&after, &["observation", "sample_age_ms"]),
                Some(2500)
            );
            assert_eq!(number(&after, &["observation", "cache_age_ms"]), Some(500));
            assert_eq!(json::parse(&after.to_string()).unwrap(), after);
        }

        #[test]
        fn stale_active_cache_keeps_the_last_report_and_marks_its_age() {
            let mut value = report();
            decorate_cache(
                &mut value,
                &CacheInfo {
                    written_at_ms: 1_000_000,
                    verification: "proc_identity",
                },
                None,
                1_060_000,
            )
            .unwrap();
            assert_eq!(
                field(&value, &["observation", "stale"]).and_then(Value::as_bool),
                Some(true)
            );
            assert_eq!(
                number(&value, &["observation", "sample_age_ms"]),
                Some(60_000)
            );
            assert_eq!(text(&value, &["pressure"]), Some("recovering"));
            assert_eq!(text(&value, &["worker"]), Some("reclaim"));
            assert!(human_report(&value, false).contains("STALE"));
        }

        #[test]
        fn cache_identity_rejects_reused_pids_old_boots_and_dead_processes() {
            let value = report();
            assert!(verify_instance(&value, &process_stat(77, "S", 42), "boot-A").is_ok());
            assert!(verify_instance(&value, &process_stat(77, "S", 43), "boot-A").is_err());
            assert!(verify_instance(&value, &process_stat(77, "S", 42), "boot-B").is_err());
            assert!(verify_instance(&value, &process_stat(78, "S", 42), "boot-A").is_err());
            assert!(verify_instance(&value, &process_stat(77, "Z", 42), "boot-A").is_err());
            assert!(verify_instance(&value, "77 (truncated) S", "boot-A").is_err());
        }

        #[test]
        fn legacy_cache_requires_the_same_active_system_service_pid() {
            assert!(legacy_service_matches(
                "ActiveState=active\nMainPID=77\n",
                77
            ));
            assert!(!legacy_service_matches(
                "ActiveState=active\nMainPID=78\n",
                77
            ));
            assert!(!legacy_service_matches(
                "ActiveState=inactive\nMainPID=77\n",
                77
            ));
            assert!(!legacy_service_matches("MainPID=77\n", 77));
        }

        #[test]
        fn independent_previews_never_claim_unprimed_swap_rates_are_zero() {
            let mut value = report();
            decorate_preview(&mut value, "plan", None, None, 1_002_000).unwrap();
            assert_eq!(
                text(&value, &["observation", "source"]),
                Some("independent_plan")
            );
            assert_eq!(
                text(&value, &["observation", "decision"]),
                Some("preview_not_applied")
            );
            assert_eq!(
                field(&value, &["observation", "daemon_active"]),
                Some(&Value::Null)
            );
            assert_eq!(
                field(&value, &["swap", "in_bytes_per_sec"]),
                Some(&Value::Null)
            );
            assert_eq!(
                field(&value, &["swap", "out_bytes_per_sec"]),
                Some(&Value::Null)
            );
            assert_eq!(
                field(&value, &["paging", "rates_available"]).and_then(Value::as_bool),
                Some(false)
            );
            for key in RATE_FIELDS {
                assert_eq!(field(&value, &["paging", key]), Some(&Value::Null));
            }
            let human = human_report(&value, false);
            assert!(human.contains("preview only, not applied"));
            assert!(human.contains("warming up"));
            assert!(!human.contains("Daemon worker"));
        }

        #[test]
        fn human_status_separates_io_from_memory_and_explains_empty_coverage() {
            let mut value = report();
            value.set("memory_pressure", "healthy".into()).unwrap();
            decorate_cache(
                &mut value,
                &CacheInfo {
                    written_at_ms: 1_000_000,
                    verification: "proc_identity",
                },
                None,
                1_002_000,
            )
            .unwrap();
            let human = human_report(&value, false);
            assert!(human.contains("Memory pressure   healthy"));
            assert!(human.contains("I/O congestion    congested"));
            assert!(human.contains("swappiness 80"));
            assert!(human.contains("daemon requested swappiness 100"));
            assert!(human.contains("no populated managed background work"));
            assert!(human.contains("Daemon diagnostic"));
        }
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    if let Err(error) = execute() {
        if let Some(admission) = error.downcast_ref::<admission::Failure>() {
            admission::diagnostic(&admission.json());
            std::process::exit(admission.code);
        }
        if std::env::args_os()
            .nth(1)
            .is_some_and(|arg| arg == "run" || arg == "__dedup-exec")
        {
            admission::diagnostic(&format!(
                "{{\"event\":\"run_error\",\"message\":{}}}",
                json::quote(&error.to_string())
            ));
        } else {
            eprintln!("zmem: {error}");
        }
        std::process::exit(1);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let a = std::env::args().nth(1);
        if a.as_deref() == Some("--version") {
            println!("zmem {}", env!("CARGO_PKG_VERSION"));
        } else {
            eprintln!(
                "{HELP}\nLive management requires Linux. The zmem no_std policy library supports other platforms."
            );
            std::process::exit(1);
        }
    }
}
