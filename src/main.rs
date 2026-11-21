#[cfg(target_os = "linux")]
mod config;
#[cfg(target_os = "linux")]
mod install;
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
  zmem run --class background|interactive|disposable [--max-memory MiB] -- COMMAND...
  sudo zmem install --replace [--config PATH]
  sudo zmem rollback

Global: --config PATH, --help, --version
JSON byte counts are bytes; PSI basis points use 100 = 1%.
The no_std library is portable. Live system management requires Linux + systemd + cgroup v2.
Background tasks are throttled; disposable tasks explicitly opt into a hard memory limit.
";

#[cfg(target_os = "linux")]
fn execute() -> Result<()> {
    use std::{
        fs,
        path::{Path, PathBuf},
    };
    use zmem::{GIB, Policy};
    let mut args: Vec<String> = std::env::args().skip(1).collect();
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
    let settings = if configuration.exists() {
        config::Settings::read(&configuration)?
    } else if custom_config {
        return Err(format!("config does not exist: {}", configuration.display()).into());
    } else {
        config::Settings::default()
    };
    match action.as_str() {
        "status" | "plan" => {
            if args
                .iter()
                .any(|a| !["--json", "--processes", "--live"].contains(&a.as_str()))
            {
                return Err("unknown status/plan argument".into());
            }
            if action == "status" && flag("--json") && !flag("--processes") && !flag("--live") {
                if let Ok(meta) = fs::metadata("/run/zmem/status.json") {
                    if meta
                        .modified()
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|d| d.as_secs() <= 10)
                    {
                        print!("{}", linux::read("/run/zmem/status.json")?);
                        return Ok(());
                    }
                }
            }
            let t = linux::Sampler::new().sample()?;
            let d = Policy::new(settings.policy).observe(t.sample, 0);
            if flag("--json") {
                println!(
                    "{}",
                    linux::telemetry_json(
                        &t,
                        &d,
                        &settings.profile,
                        None,
                        &[],
                        flag("--processes")
                    )
                );
            } else {
                println!(
                    "zmem {}  ·  {}  ·  {}",
                    env!("CARGO_PKG_VERSION"),
                    d.pressure.as_str(),
                    settings.profile
                );
                println!(
                    "Memory   {:>6.2} / {:>6.2} GiB available",
                    t.sample.available_bytes as f64 / GIB as f64,
                    t.sample.total_bytes as f64 / GIB as f64
                );
                println!(
                    "Swap     {:>6.2} / {:>6.2} GiB used",
                    t.sample
                        .swap_total_bytes
                        .saturating_sub(t.sample.swap_free_bytes) as f64
                        / GIB as f64,
                    t.sample.swap_total_bytes as f64 / GIB as f64
                );
                println!(
                    "Stalls   memory {:.2}% some · {:.2}% full · IO {:.2}% full",
                    t.sample.memory_some_bp as f64 / 100.0,
                    t.sample.memory_full_bp as f64 / 100.0,
                    t.sample.io_full_bp as f64 / 100.0
                );
                println!(
                    "Policy   {} · background high {}% · swap growth {} MiB",
                    d.reason.as_str(),
                    d.background_high_percent,
                    d.grow_swap_bytes / zmem::MIB
                );
                for z in &t.zram {
                    println!(
                        "{}    {:.2} GiB data → {:.2} GiB resident",
                        z.name,
                        z.original as f64 / GIB as f64,
                        z.resident as f64 / GIB as f64
                    );
                }
                if flag("--processes") {
                    println!(
                        "Processes (RSS + swap, shared pages may be counted more than once):\n{}",
                        linux::top_processes(20)
                    );
                }
            }
            Ok(())
        }
        "doctor" => {
            if args.iter().any(|a| a != "--json") {
                return Err("doctor accepts --json".into());
            }
            let (result, ok) = linux::doctor_json();
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
                "{{\"schema\":\"zmem.commands.v1\",\"version\":{},\"transport\":\"cli\",\"read_only\":[{{\"name\":\"status\",\"argv\":[\"zmem\",\"status\",\"--json\"],\"description\":\"Current memory, PSI, swap, zram and policy; counts in bytes, PSI in basis points\"}},{{\"name\":\"processes\",\"argv\":[\"zmem\",\"status\",\"--json\",\"--processes\"]}},{{\"name\":\"plan\",\"argv\":[\"zmem\",\"plan\",\"--json\"]}},{{\"name\":\"doctor\",\"argv\":[\"zmem\",\"doctor\",\"--json\"]}}],\"mutations\":[{{\"name\":\"run\",\"root\":false,\"classes\":[\"background\",\"interactive\",\"disposable\"],\"hard_limit_opt_in_class\":\"disposable\"}},{{\"name\":\"once\",\"root\":true,\"required_flag\":\"--apply\"}},{{\"name\":\"install\",\"root\":true,\"replacement_flag\":\"--replace\"}},{{\"name\":\"rollback\",\"root\":true}}],\"automatic_process_kill\":false,\"automatic_global_cache_drop\":false}}",
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
        "run" => {
            let split = args
                .iter()
                .position(|a| a == "--")
                .ok_or("run requires -- before the command")?;
            let mut class = "background";
            let mut max = None;
            let mut i = 0;
            while i < split {
                match args[i].as_str() {
                    "--class" => {
                        i += 1;
                        class = args
                            .get(i)
                            .filter(|_| i < split)
                            .ok_or("--class requires a value")?;
                    }
                    "--max-memory" => {
                        i += 1;
                        max = Some(
                            args.get(i)
                                .filter(|_| i < split)
                                .ok_or("--max-memory requires MiB")?
                                .parse::<u64>()?,
                        );
                    }
                    other => return Err(format!("unknown run option: {other}").into()),
                }
                i += 1;
            }
            linux::run_workload(class, max, &args[split + 1..])
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
        "__reclaim" => {
            if args.len() != 2 {
                return Err("invalid reclaim request".into());
            }
            linux::reclaim(Path::new(&args[0]), args[1].parse()?)
        }
        _ => Err(format!("unknown command: {action}; use zmem --help").into()),
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    if let Err(error) = execute() {
        eprintln!("zmem: {error}");
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
