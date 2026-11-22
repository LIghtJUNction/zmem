(() => {
  "use strict";
  const english = {
    skip: "Skip to content", "nav.how": "How it works", "nav.start": "Get started",
    "hero.label": "LINUX MEMORY CONTROL", "hero.title1": "Room for your work.", "hero.title2": "Space to respond.",
    "hero.intro": "zmem waits before starting new background jobs under memory pressure, then budgets their running work. Maintain disk swap and inspect which controls took effect and which tasks they cover.",
    "hero.cta": "Get started", "hero.demo": "Explore the policy", "hero.note": "Written in Rust · Runs locally · One view for people and AI",
    "terminal.label": "ILLUSTRATIVE OUTPUT", "terminal.available": "Available memory", "terminal.active": "In use", "terminal.headroom": "Headroom", "terminal.availableShort": "Available",
    "terminal.swap": "Swap available", "terminal.policy": "Current policy", "terminal.normal": "hold / observe", "terminal.last": "Read the pressure. Bound the action.", "terminal.caption": "Illustrative values. Actual status identifies the daemon report's source and sample age.",
    "facts.deps": "Third-party Rust dependencies", "facts.core": "Allocation-free policy", "facts.ai": "Structured control for AI", "facts.host": "PSI + cgroup v2",
    "principles.label": "01 / THE CONTROL LOOP", "principles.title": "Measure pressure. Preserve headroom.", "principles.intro": "Memory is there to be used. zmem looks at how long tasks wait for it, and the cost of the next action.",
    "principles.observe.title": "Observe real stalls", "principles.observe.body": "Report memory pressure and IO congestion separately. Use available memory, PSI stall time, file-page refaults and paging activity to assess the next action's cost.",
    "principles.act.title": "Take small steps. Keep tasks alive.", "principles.act.body": "Adjust swap preference from measured paging cost and budget managed background work. Follow recognized desktop, terminal and remote-access processes through their actual cgroup hierarchy.",
    "principles.recover.title": "Recover at a steady pace", "principles.recover.body": "Unproductive reclaim backs off, and retained swap files are checked one at a time. Active retirement waits for sustained recovery and whole-file headroom. Dedup scanning also waits for stability before resuming.",
    "demo.label": "02 / EXPLORE THE POLICY", "demo.title": "Move the pressure. See the decision.", "demo.intro": "Use a simulated 16 GiB machine to see how available memory and waiting time shape the policy.",
    "demo.tag": "Simplified policy-core preview · Synthetic inputs · No device access", "demo.footnote": "Thresholds and high-water values are illustrative, not current quotas. This preview omits launch admission, device-cost adaptation, dynamic budgets, hysteresis, swap maintenance and KSM control. It does not predict results on your machine.",
    "demo.idle": "Everyday work", "demo.build": "Parallel build", "demo.model": "Model loading", "demo.available": "Available memory", "demo.pressure": "Memory stall time · full PSI", "demo.decision": "Policy", "demo.budget": "Illustrative high-water mark",
    "capabilities.label": "03 / CLEAR RESPONSIBILITIES", "capabilities.title": "One place to manage memory.",
    "capabilities.swap.title": "Manage the swap lifecycle", "capabilities.swap.body": "Grow in configured chunks and clean retained owned files one at a time. Active retirement needs sustained health and whole-file headroom; the kernel protects files still in use.",
    "capabilities.work.title": "Check headroom before starting work", "capabilities.work.body": "Submit jobs with zmem run. Wait under pressure, stagger starts after stable recovery, then share a background budget. The default wait is 60 seconds; a timeout leaves the command unstarted.",
    "capabilities.explain.title": "Explain every decision", "capabilities.explain.body": "Status retains daemon state and adds current process details on request. --live and plan are labelled independent, unapplied previews. JSON includes their provenance and observed coverage.",
    "capabilities.dedup.title": "Deduplicate explicitly registered work", "capabilities.dedup.body": "Use --dedup for trusted background or disposable jobs. CPU, memory or IO pressure pauses scanning while preserving merged pages. KSM matching crosses process and user boundaries.",
    "capabilities.bounds": "Maintenance attempts are at least 60 seconds apart, with at most one file each. Active retirement waits for 600 healthy seconds by default. Dedup scanning resumes after 30 stable seconds, using 128-page batches and 100 ms sleeps, not a hard CPU quota.",
    "capabilities.scope": "The kernel handles paging, page merging and final OOM. zmem does not register every application, force merged pages apart or apply filesystem deduplication to active swap. Application checkpoints, computation recovery and energy optimization remain future work.",
    "capabilities.guide": "Swap lifecycle and deduplication boundaries",
    "install.label": "04 / GET STARTED", "install.title": "Read the state. Then hand over control.", "install.intro": "For Linux systems with PSI, cgroup v2 and systemd. Installing the management service requires root privileges.", "install.source": "Install from crates.io", "install.binary": "Download Linux x86_64 / ARM64 binaries ↗",
    "install.handoff.title": "Adopt in place. Keep a way back.", "install.handoff.body": "The installer saves services and configuration while retaining active swap. Failure recovery uses the state just before that installation; explicit rollback uses the first-install baseline. Stopping or upgrading zmem does not drain swap.", "install.rollback": "Restore the first-install management baseline", "install.guide": "Installation and recovery guide",
    copy: "Copy", "commands.command": "Command", "commands.purpose": "Purpose", "commands.status": "Read the daemon report with a current process list", "commands.live": "Preview an independent sample with unprimed rates", "commands.plan": "Preview an independent policy decision without applying it", "commands.observe": "Continuously observe and emit unapplied decisions", "commands.run": "Wait for memory headroom, then run a background build", "commands.dedup": "Register a trusted job for memory deduplication", "commands.schema": "Read the command contract for AI",
    "commands.coverage": "Reports distinguish plans from actual controls and retain the last protection check's time and errors. Bounded dedup discovery refreshes every 10 seconds, reporting coverage, KSM state and a system-wide profit estimate. Missing metrics remain null; an unavailable daemon report is labelled as an independent snapshot.",
    "commands.measure": "The source includes measure_responsiveness.py to observe scheduler lateness, PSI, paging and background activity during ordinary work. The optional desktop probe measures process startup plus IPC round trips. Before/after observations compare similar work windows; causal evaluation requires controlled workload and cache conditions.",
    "commands.evidence": "Acceptance records identify the version, environment and observation window. Admission covers explicitly submitted jobs. These mechanisms do not promise memory savings, desktop latency or freedom from stalls.",
    "commands.acceptance": "Read about admission, timeouts and coverage",
    "ecosystem.title": "Part of your working environment.", "ecosystem.body": "zmem works independently and provides the default memory management plan for oh-my-ztty. Inspect the source, the policy and the release process.", "footer.note": "Memory, with room to work."
  };
  const translated = [...document.querySelectorAll("[data-i18n]")];
  const chinese = Object.fromEntries(translated.map(el => [el.dataset.i18n, el.textContent]));
  let language = "zh";
  try { if (localStorage.getItem("zmem-language") === "en") language = "en"; } catch (_) {}
  const button = document.querySelector(".language-switch");
  const status = document.getElementById("action-status");
  const available = document.getElementById("available");
  const pressure = document.getElementById("pressure");
  const scenarioButtons = [...document.querySelectorAll("[data-scenario]")];
  const messages = {
    zh: {
      healthy: ["保留余量，保持运行。", "当前压力低。观察系统，不主动回收后台内存。", "按任务设置"],
      elevated: ["给前台多留一点空间。", "内存余量开始紧张。对纳管后台任务施加高水位，按需准备 Swap。", "11.2 GiB"],
      critical: ["限制后台压力，优先保持响应。", "持续阻塞或余量过低。收紧纳管后台预算；IO 允许时进行有界回收。", "8.0 GiB"]
    },
    en: {
      healthy: ["Keep the headroom. Let work continue.", "Pressure is low. Observe the system without proactive background reclaim.", "Per-job setting"],
      elevated: ["Make more room for the foreground.", "Headroom is getting tight. Apply a high-water mark to managed background work and prepare swap as needed.", "11.2 GiB"],
      critical: ["Contain background pressure. Protect response.", "Sustained stalls or very low headroom. Tighten the managed background budget and use bounded reclaim when IO allows.", "8.0 GiB"]
    }
  };
  function updateSimulation() {
    const free = Number(available.value), stall = Number(pressure.value);
    document.getElementById("available-value").textContent = `${free.toFixed(1)} GiB`;
    document.getElementById("pressure-value").textContent = `${stall.toFixed(1)}%`;
    const state = free < 0.8 || stall >= 20 ? "critical" : free < 1.6 || stall >= 5 ? "elevated" : "healthy";
    const [title, body, budget] = messages[language][state];
    document.getElementById("decision-state").textContent = state;
    document.getElementById("decision-title").textContent = title;
    document.getElementById("decision-body").textContent = body;
    document.getElementById("decision-budget").textContent = budget;
  }
  function applyLanguage() {
    const table = language === "en" ? english : chinese;
    for (const el of translated) el.textContent = table[el.dataset.i18n] || chinese[el.dataset.i18n];
    document.documentElement.lang = language === "en" ? "en" : "zh-CN";
    button.textContent = language === "en" ? "中文" : "EN";
    button.setAttribute("aria-label", language === "en" ? "切换到中文" : "Switch language to English");
    document.title = language === "en" ? "zmem — Memory, with room to work." : "zmem — 给任务留空间，给系统留余量。";
    updateSimulation();
  }
  button.addEventListener("click", () => {
    language = language === "en" ? "zh" : "en";
    try { localStorage.setItem("zmem-language", language); } catch (_) {}
    applyLanguage();
  });
  for (const el of [available, pressure]) el.addEventListener("input", () => {
    for (const tab of scenarioButtons) { tab.classList.remove("active"); tab.setAttribute("aria-pressed", "false"); }
    updateSimulation();
  });
  const scenarios = {idle: [5.8, 0.1], build: [1.3, 6.5], model: [0.6, 25.0]};
  for (const tab of scenarioButtons) tab.addEventListener("click", () => {
    const [free, stall] = scenarios[tab.dataset.scenario];
    available.value = free; pressure.value = stall;
    for (const other of scenarioButtons) { const selected = other === tab; other.classList.toggle("active", selected); other.setAttribute("aria-pressed", String(selected)); }
    updateSimulation();
  });
  for (const copy of document.querySelectorAll("[data-copy]")) copy.addEventListener("click", async () => {
    const text = document.getElementById(copy.dataset.copy).textContent;
    try {
      if (!navigator.clipboard) throw new Error("Clipboard unavailable");
      await navigator.clipboard.writeText(text);
      copy.textContent = language === "en" ? "Copied" : "已复制";
      status.textContent = language === "en" ? "Command copied to clipboard." : "命令已复制。";
      setTimeout(() => { copy.textContent = language === "en" ? "Copy" : "复制"; }, 1800);
    } catch (_) {
      const selection = window.getSelection(), range = document.createRange();
      range.selectNodeContents(document.getElementById(copy.dataset.copy)); selection.removeAllRanges(); selection.addRange(range);
      status.textContent = language === "en" ? "Command selected. Copy it with your device's copy command." : "命令已选中，请使用设备的复制功能。";
    }
  });
  if (/^\d+\.\d+\.\d+$/.test(window.ZMEM_VERSION || "")) {
    for (const el of document.querySelectorAll("[data-version]")) el.textContent = `v${window.ZMEM_VERSION}`;
  }
  applyLanguage();
})();
