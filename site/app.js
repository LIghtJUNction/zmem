(() => {
  "use strict";
  const english = {
    skip: "Skip to content", "nav.how": "How it works", "nav.start": "Get started",
    "hero.label": "LINUX MEMORY CONTROL", "hero.title1": "Room for your work.", "hero.title2": "Space to respond.",
    "hero.intro": "zmem brings memory pressure, compressed swap and disk swap under one controller. Keep headroom for the foreground while background work moves at a measured pace.",
    "hero.cta": "Get started", "hero.demo": "Explore the policy", "hero.note": "Written in Rust · Runs locally · One view for people and AI",
    "terminal.label": "ILLUSTRATIVE OUTPUT", "terminal.available": "Available memory", "terminal.active": "In use", "terminal.headroom": "Headroom", "terminal.availableShort": "Available",
    "terminal.swap": "Swap available", "terminal.policy": "Current policy", "terminal.normal": "hold / observe", "terminal.last": "Read the pressure. Bound the action.", "terminal.caption": "Illustrative values. Live measurements come from your Linux kernel.",
    "facts.deps": "Third-party Rust dependencies", "facts.core": "Allocation-free policy", "facts.ai": "Structured control for AI", "facts.host": "PSI + cgroup v2",
    "principles.label": "01 / THE CONTROL LOOP", "principles.title": "Measure pressure. Preserve headroom.", "principles.intro": "Memory is there to be used. zmem looks at how long tasks wait for it, and the cost of the next action.",
    "principles.observe.title": "Observe real stalls", "principles.observe.body": "Combine available memory, PSI waiting time, swap activity and IO pressure to identify when productive memory use turns into sustained thrashing.",
    "principles.act.title": "Take small steps. Keep tasks alive.", "principles.act.body": "Add bounded swap capacity when needed, apply memory high-water marks to managed background work, and protect desktop sessions and SSH.",
    "principles.recover.title": "Recover at a steady pace", "principles.recover.body": "Cooldowns and recovery hysteresis limit oscillation. Shrinking selects only unused, owned swap files. Restarting the daemon preserves active swap.",
    "demo.label": "02 / EXPLORE THE POLICY", "demo.title": "Move the pressure. See the decision.", "demo.intro": "Use a simulated 16 GiB machine to see how available memory and waiting time shape the policy.",
    "demo.tag": "Browser simulation · No access to your device", "demo.footnote": "This illustrates a simplified single decision. The real daemon also checks disk headroom, IO pressure, cooldowns and recovery history.",
    "demo.idle": "Everyday work", "demo.build": "Parallel build", "demo.model": "Model loading", "demo.available": "Available memory", "demo.pressure": "Memory stall time · full PSI", "demo.decision": "Policy", "demo.budget": "Background memory high-water mark",
    "capabilities.label": "03 / CLEAR RESPONSIBILITIES", "capabilities.title": "One place to manage memory.",
    "capabilities.swap.title": "Own the swap lifecycle", "capabilities.swap.body": "Adopt existing zram and disk swap. Grow on demand, preserve free disk space, and initialize zram on the next boot.",
    "capabilities.work.title": "Give background work a budget", "capabilities.work.body": "Start managed jobs with zmem run. Apply throttling and bounded reclaim under pressure. Explicitly disposable work can use a hard limit.",
    "capabilities.explain.title": "Explain every decision", "capabilities.explain.body": "Inspect process usage, memory pressure, compression and policy reasons. JSON gives AI and scripts the same structured view.",
    "capabilities.small.title": "Keep the controller small", "capabilities.small.body": "An allocation-free policy core with no third-party Rust dependencies. The resident path stays bounded; process detail is queried on demand.",
    "capabilities.scope": "The kernel performs paging and final OOM handling. zmem manages early intervention and task budgets. Application checkpoints, general memory deduplication and energy optimization remain research directions.",
    "install.label": "04 / GET STARTED", "install.title": "Read the state. Then hand over control.", "install.intro": "For Linux systems with PSI, cgroup v2 and systemd. Installing the management service requires root privileges.", "install.source": "Install from crates.io", "install.binary": "Download Linux x86_64 / ARM64 binaries ↗",
    "install.handoff.title": "Adopt in place. Keep a way back.", "install.handoff.body": "The installer saves existing services and configuration, adopts active swap and switches the management service. Stopping or upgrading zmem preserves active swap.", "install.rollback": "Restore the previous management configuration", "install.guide": "Installation and recovery guide",
    copy: "Copy", "commands.command": "Command", "commands.purpose": "Purpose", "commands.status": "Inspect system and process memory", "commands.plan": "Read policy decisions and reasons", "commands.observe": "Continuously observe and emit decisions", "commands.run": "Run a build with a background budget", "commands.schema": "Read the command contract for AI",
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
