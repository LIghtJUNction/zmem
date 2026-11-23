use super::*;
use crate::budget::{GroupUsage, plan_background};
use std::{io::BufRead, sync::mpsc};

const WORKER: &str = r#"
import os, sys
with open(sys.argv[1] + '/cgroup.procs', 'w') as f:
    f.write(str(os.getpid()))
blocks = [(bytearray(b'\xa5') * (int(sys.argv[2]) * 1048576), 0xa5)]
print('ready', flush=True)
for command in sys.stdin:
    command = command.strip()
    if command == 'grow' and len(blocks) == 1:
        blocks.append((bytearray(b'\x5a') * (8 * 1048576), 0x5a))
        print('grown', flush=True)
    elif command == 'verify':
        assert all(block.count(value) == len(block) for block, value in blocks)
        print('verified', flush=True)
    elif command == 'quit':
        print('bye', flush=True)
        sys.exit(0)
    else:
        raise RuntimeError('unexpected fixture command')
"#;

struct FixtureWorker {
    child: Child,
    output: mpsc::Receiver<std::io::Result<String>>,
}

impl FixtureWorker {
    fn start(path: &Path, mib: u64) -> Result<Self> {
        let mut child = Command::new("/usr/bin/python3")
            .args(["-I", "-u", "-c", WORKER])
            .arg(path)
            .arg(mib.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdout = child.stdout.take().ok_or("missing worker stdout")?;
        let (send, output) = mpsc::channel();
        thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let failed = line.is_err();
                if send.send(line).is_err() || failed {
                    break;
                }
            }
        });
        Ok(Self { child, output })
    }

    fn send(&mut self, message: &str) -> Result<()> {
        self.child
            .stdin
            .as_mut()
            .ok_or("missing worker stdin")?
            .write_all(message.as_bytes())?;
        Ok(())
    }

    fn expect(&self, expected: &str, deadline: Instant) -> Result<()> {
        let line = self
            .output
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))??;
        if line != expected {
            return Err(format!("worker reported {line:?}, expected {expected:?}").into());
        }
        Ok(())
    }
}

struct SchedulingFixture {
    root: PathBuf,
    monitor: PathBuf,
    leaves: Vec<PathBuf>,
    workers: Vec<FixtureWorker>,
    moved: bool,
    enabled: bool,
    cleaned: bool,
    deadline: Instant,
}

impl SchedulingFixture {
    fn new(deadline: Instant) -> Result<Self> {
        if std::env::var("ZMEM_SCHEDULER_FIXTURE").as_deref() != Ok("1") {
            return Err("set ZMEM_SCHEDULER_FIXTURE=1 in a fresh delegated test service".into());
        }
        let membership = read("/proc/self/cgroup")?;
        let relative = membership
            .lines()
            .find_map(|line| line.strip_prefix("0::/"))
            .ok_or("missing unified cgroup membership")?;
        let root = fs::canonicalize(Path::new(CGROOT).join(relative))?;
        let unit = root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if !root.starts_with(CGROOT)
            || !unit.starts_with("zmem-scheduler-fixture-")
            || !unit.ends_with(".service")
        {
            return Err("refusing cgroup outside zmem-scheduler-fixture-*.service".into());
        }
        for (file, expected) in [
            ("memory.max", (256 * MIB).to_string()),
            ("memory.swap.max", "0".into()),
            ("pids.max", "16".into()),
            ("cgroup.procs", std::process::id().to_string()),
        ] {
            if read(root.join(file))?.trim() != expected {
                return Err(
                    format!("unexpected {file}; require an isolated bounded service").into(),
                );
            }
        }
        if fs::read_dir(&root)?
            .any(|entry| entry.is_ok_and(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir())))
        {
            return Err("test service already contains child cgroups".into());
        }
        if !read(root.join("cgroup.controllers"))?
            .split_whitespace()
            .any(|name| name == "memory")
        {
            return Err("test service requires Delegate=memory".into());
        }
        let mut fixture = Self {
            monitor: root.join("monitor"),
            root,
            leaves: Vec::new(),
            workers: Vec::new(),
            moved: false,
            enabled: false,
            cleaned: false,
            deadline,
        };
        fs::create_dir(&fixture.monitor)?;
        fs::write(
            fixture.monitor.join("cgroup.procs"),
            std::process::id().to_string(),
        )?;
        fixture.moved = true;
        fs::write(fixture.root.join("cgroup.subtree_control"), "+memory")?;
        fixture.enabled = true;
        for name in ["large", "small", "empty"] {
            let leaf = fixture.root.join(name);
            fs::create_dir(&leaf)?;
            fixture.leaves.push(leaf);
        }
        Ok(fixture)
    }

    fn cleanup(&mut self) -> Result<()> {
        let mut errors = Vec::new();
        for leaf in &self.leaves {
            if let Err(error) = write_control(&leaf.join("memory.high"), "max", true) {
                errors.push(error.to_string());
            }
        }
        for worker in &mut self.workers {
            if !matches!(worker.child.try_wait(), Ok(Some(_))) {
                let _ = worker.child.kill();
            }
        }
        let reap_deadline = self.deadline.min(Instant::now() + Duration::from_secs(2));
        for worker in &mut self.workers {
            loop {
                match worker.child.try_wait() {
                    Ok(Some(_)) => break,
                    Err(error) => {
                        errors.push(error.to_string());
                        break;
                    }
                    Ok(None) if Instant::now() < reap_deadline => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Ok(None) => {
                        errors.push("owned worker did not reap before the cleanup deadline".into());
                        break;
                    }
                }
            }
        }
        for leaf in &self.leaves {
            if let Err(error) = fs::remove_dir(leaf) {
                errors.push(error.to_string());
            }
        }
        self.leaves.retain(|leaf| leaf.exists());
        if self.enabled {
            match fs::write(self.root.join("cgroup.subtree_control"), "-memory") {
                Ok(()) => self.enabled = false,
                Err(error) => errors.push(error.to_string()),
            }
        }
        if self.moved && !self.enabled {
            match fs::write(
                self.root.join("cgroup.procs"),
                std::process::id().to_string(),
            ) {
                Ok(()) => self.moved = false,
                Err(error) => errors.push(error.to_string()),
            }
        }
        if self.monitor.exists()
            && !self.moved
            && let Err(error) = fs::remove_dir(&self.monitor)
        {
            errors.push(error.to_string());
        }
        self.cleaned = errors.is_empty();
        if self.cleaned {
            Ok(())
        } else {
            Err(errors.join("; ").into())
        }
    }
}

impl Drop for SchedulingFixture {
    fn drop(&mut self) {
        if !self.cleaned
            && let Err(error) = self.cleanup()
        {
            eprintln!("isolated scheduling fixture cleanup: {error}");
        }
    }
}

fn event(path: &Path, file: &str, key: &str) -> Result<u64> {
    read(path.join(file))?
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(' ')?;
            (name == key).then_some(value)
        })
        .ok_or_else(|| format!("missing {key} in {file}"))?
        .parse()
        .map_err(Into::into)
}

fn usages(leaves: &[PathBuf]) -> Result<Vec<GroupUsage>> {
    leaves
        .iter()
        .map(|leaf| {
            Ok(GroupUsage {
                current_bytes: read(leaf.join("memory.current"))?.trim().parse()?,
                populated: event(leaf, "cgroup.events", "populated")? == 1,
            })
        })
        .collect()
}

fn apply_limits(leaves: &[PathBuf], limits: &[Option<u64>]) -> Result<Vec<u64>> {
    let page = control_page_size()?;
    let mut actual = Vec::new();
    for (leaf, limit) in leaves.iter().zip(limits) {
        let path = leaf.join("memory.high");
        let value = limit.map_or_else(|| "max".into(), |bytes| bytes.to_string());
        write_control(&path, &value, true)?;
        let observed = read(&path)?;
        assert_eq!(
            observed.trim(),
            canonical_control_value(&path, &value, page)?
        );
        if limit.is_some() {
            actual.push(observed.trim().parse()?);
        }
    }
    Ok(actual)
}

#[test]
#[ignore = "requires a fresh delegated zmem-scheduler-fixture-*.service with explicit resource bounds"]
fn kernel_background_budget_throttles_and_recovers() -> Result<()> {
    let start = Instant::now();
    let deadline = start + Duration::from_secs(25);
    let mut fixture = SchedulingFixture::new(start + Duration::from_secs(30))?;
    let before_oom = event(&fixture.root, "memory.events", "oom")?;
    let before_kill = event(&fixture.root, "memory.events", "oom_kill")?;
    for (index, mib) in [64, 16].into_iter().enumerate() {
        fixture
            .workers
            .push(FixtureWorker::start(&fixture.leaves[index], mib)?);
    }
    for worker in &fixture.workers {
        worker.expect("ready", deadline)?;
    }
    let groups = usages(&fixture.leaves)?;
    assert!(groups[0].populated && groups[1].populated && !groups[2].populated);
    assert!(groups[0].current_bytes >= 64 * MIB);
    assert!(groups[1].current_bytes >= 16 * MIB);
    assert!(groups[0].current_bytes > groups[1].current_bytes);

    // Only the host-pressure inputs are controlled. Group use, population,
    // throttling, allocation progress and data contents come from the kernel.
    // Outside use falls back to 32 MiB after the pressure episode; subtracting
    // fresh managed use keeps each synthetic headroom sample self-consistent.
    let total = 256 * MIB;
    let reserve = 32 * MIB;
    let outside = 32 * MIB;
    let available = |groups: &[GroupUsage]| {
        let used: u64 = groups
            .iter()
            .filter(|group| group.populated)
            .map(|group| group.current_bytes)
            .sum();
        assert!(
            used + outside <= total,
            "fixture usage exceeds simulated RAM"
        );
        total - outside - used
    };
    let weighted = plan_background(
        total,
        available(&groups),
        reserve,
        70,
        &groups,
        None,
        256 * MIB,
    );
    assert_eq!(weighted.total_bytes, Some(184 * MIB));
    assert!(weighted.limits[0] > weighted.limits[1]);
    assert_eq!(weighted.limits[2], None);
    let weighted_actual = apply_limits(&fixture.leaves, &weighted.limits)?;
    assert!(weighted_actual[0] > groups[0].current_bytes);
    assert!(weighted_actual[1] > groups[1].current_bytes);

    let tight = plan_background(
        total,
        0,
        reserve,
        50,
        &groups,
        weighted.total_bytes,
        256 * MIB,
    );
    assert_eq!(tight.total_bytes, Some(64 * MIB));
    let tight_actual = apply_limits(&fixture.leaves, &tight.limits)?;
    assert!(tight_actual[0] < groups[0].current_bytes);
    let before_high = [
        event(&fixture.leaves[0], "memory.events", "high")?,
        event(&fixture.leaves[1], "memory.events", "high")?,
    ];
    for worker in &mut fixture.workers {
        worker.send("grow\n")?;
    }
    let throttle_deadline = deadline.min(Instant::now() + Duration::from_secs(5));
    loop {
        if event(&fixture.leaves[0], "memory.events", "high")? > before_high[0] {
            break;
        }
        if Instant::now() >= throttle_deadline {
            return Err("bounded allocation did not produce a memory.high event".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    let mut previous = tight.total_bytes;
    let mut relief_samples = Vec::new();
    let mut relief_actual = Vec::new();
    for _ in 0..4 {
        let current = usages(&fixture.leaves)?;
        let spare_available = available(&current);
        let max_increase =
            (256 * MIB).min(spare_available.saturating_sub(reserve + reserve / 4) / 4);
        let relief = plan_background(
            total,
            spare_available,
            reserve,
            50,
            &current,
            previous,
            256 * MIB,
        );
        let before = previous.unwrap();
        let after = relief.total_bytes.unwrap();
        assert!(
            after > before,
            "real spare headroom must release a tight budget"
        );
        assert!(
            after - before <= max_increase,
            "each sample must bound relaxation"
        );
        assert!(after <= total - reserve);
        relief_actual = apply_limits(&fixture.leaves, &relief.limits)?;
        relief_samples.push([
            total - outside - spare_available,
            spare_available,
            before,
            after,
            max_increase,
        ]);
        previous = relief.total_bytes;
    }
    assert!(
        previous.unwrap() > total / 2,
        "pressure must not impose a fixed 50% ceiling"
    );
    for worker in &mut fixture.workers {
        worker.expect("grown", deadline)?;
        worker.send("verify\n")?;
        worker.expect("verified", deadline)?;
    }
    for leaf in &fixture.leaves[..2] {
        assert_ne!(read(leaf.join("memory.high"))?.trim(), "max");
    }
    let current = usages(&fixture.leaves)?;
    let healthy = plan_background(
        total,
        available(&current),
        reserve,
        100,
        &current,
        previous,
        256 * MIB,
    );
    assert_eq!(healthy.total_bytes, None);
    assert!(healthy.limits.iter().all(Option::is_none));
    apply_limits(&fixture.leaves, &healthy.limits)?;
    for worker in &mut fixture.workers {
        worker.send("quit\n")?;
        worker.expect("bye", deadline)?;
    }
    for worker in &mut fixture.workers {
        loop {
            if let Some(status) = worker.child.try_wait()? {
                assert!(status.success(), "bounded worker must finish successfully");
                break;
            }
            if Instant::now() >= deadline {
                return Err("worker did not finish before the fixture deadline".into());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    let high_delta = [
        event(&fixture.leaves[0], "memory.events", "high")? - before_high[0],
        event(&fixture.leaves[1], "memory.events", "high")? - before_high[1],
    ];
    assert_eq!(event(&fixture.root, "memory.events", "oom")?, before_oom);
    assert_eq!(
        event(&fixture.root, "memory.events", "oom_kill")?,
        before_kill
    );
    fixture.cleanup()?;
    println!(
        "{{\"schema\":\"zmem.scheduler-kernel-test.v1\",\"synthetic_total_bytes\":{total},\"synthetic_reserve_bytes\":{reserve},\"synthetic_outside_recovered_bytes\":{outside},\"current_bytes\":[{},{},{}],\"populated\":[true,true,false],\"weighted_budget_bytes\":{},\"weighted_high_bytes\":{:?},\"throttled_budget_bytes\":{},\"throttled_high_bytes\":{:?},\"relief_sample_columns\":[\"managed_current_bytes\",\"available_bytes\",\"previous_budget_bytes\",\"budget_bytes\",\"max_increase_bytes\"],\"relief_samples\":{:?},\"relief_high_bytes\":{:?},\"pressure_cap_during_progress\":50,\"high_events_delta\":{:?},\"workers_verified_before_healthy\":2,\"oom_delta\":0,\"oom_kill_delta\":0,\"elapsed_ms\":{}}}",
        groups[0].current_bytes,
        groups[1].current_bytes,
        groups[2].current_bytes,
        weighted.total_bytes.unwrap(),
        weighted_actual,
        tight.total_bytes.unwrap(),
        tight_actual,
        relief_samples,
        relief_actual,
        high_delta,
        start.elapsed().as_millis()
    );
    Ok(())
}
