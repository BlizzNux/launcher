// BlizzNux launcher control bar. The BlizzNux.com view sits above this bar; the Rust side
// resizes this webview between "bar" (bottom strip) and "full" (settings, disclaimer).
const { invoke } = window.__TAURI__.core;
const $ = (s) => document.querySelector(s);

const GAMES = [
  ["WoW", "World of Warcraft"], ["WTCG", "Hearthstone"], ["Hero", "Heroes of the Storm"],
  ["Pro", "Overwatch 2"], ["Fen", "Diablo IV"], ["D3", "Diablo III"],
  ["OSI", "Diablo II: Resurrected"], ["S2", "StarCraft II"], ["W3", "Warcraft III: Reforged"],
];
const FORUM = "https://blizznux.com/";
const RELEASES = "https://api.github.com/repos/BlizzNux/launcher/releases/latest";

let statusTimer;
function status(msg, err = false) {
  const el = $("#status"); el.textContent = msg; el.classList.toggle("err", err);
  clearTimeout(statusTimer);
  if (msg) statusTimer = setTimeout(() => { el.textContent = ""; }, 8000);
}

let state = { installed: true, prefix: "", umu: true };
let logTimer;

function esc(s) { return String(s).replace(/[&<>"]/g, (ch) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[ch])); }

async function loadChecks() {
  let checks = [];
  try { checks = await invoke("readiness"); } catch (e) { console.error(e); return true; }
  const plans = {};
  for (const c of checks) { if (!c.ok) { try { plans[c.name] = await invoke("fix_plan", { check: c.name }); } catch { plans[c.name] = null; } } }
  $("#checks").innerHTML = checks.map((c) => {
    const cls = c.ok ? "ok" : (c.blocking ? "bad" : "warn");
    const mark = c.ok ? "✓" : (c.blocking ? "✗" : "!");
    const fix = !c.ok && plans[c.name] ? ` <button class="fix" data-check="${esc(c.name)}">Fix this for me</button>` : "";
    return `<li class="${cls}"><span class="mark">${mark}</span><span><strong>${esc(c.name)}</strong>: ${esc(c.detail)}${c.ok ? "" : ` <span class="hint">— <code>${esc(c.hint)}</code></span>`}${fix}</span></li>`;
  }).join("");
  $("#checks").querySelectorAll(".fix").forEach((b) => { b.onclick = () => offerFix(b.dataset.check, plans[b.dataset.check]); });
  const blocked = checks.some((c) => !c.ok && c.blocking);
  $("#do-install").disabled = blocked;
  $("#do-install").title = blocked ? "Fix the items marked ✗ first" : "";
  return !blocked;
}

function offerFix(name, steps) {
  const el = $("#setup-log");
  el.classList.remove("hidden");
  el.innerHTML = `<div>This will run as administrator (you'll be asked for your password):</div>` +
    steps.map((s) => `<div><code>${esc(s)}</code></div>`).join("") +
    `<div style="margin-top:10px"><button id="fix-run" class="launch">Run</button> <button id="fix-cancel" class="flat">Cancel</button></div>`;
  $("#fix-cancel").onclick = () => { el.classList.add("hidden"); };
  $("#fix-run").onclick = async () => {
    el.textContent = "Running… answer the password prompt if it appears.";
    try {
      const out = await invoke("fix_apply", { check: name });
      el.textContent = (out.trim() || "Done.") + "\n\nRe-checking…";
    } catch (e) {
      el.textContent = "Failed:\n" + String(e).trim();
    }
    await loadChecks();
    await refreshState();
  };
}

// ---- WoW addons ----
let wow = { installs: [], current: null };

function addonsStatus(msg, err = false) { const el = $("#addons-status"); el.textContent = msg; el.style.color = err ? "var(--accent-2)" : ""; }

async function loadAddons() {
  try { wow.installs = await invoke("wow_installs"); } catch (e) { addonsStatus(String(e), true); return; }
  const none = wow.installs.length === 0;
  $("#addons-none").classList.toggle("hidden", !none);
  $("#addons-ui").classList.toggle("hidden", none);
  if (none) return;
  refreshWowUp();
  if (!wow.current || !wow.installs.some((w) => w.flavor === wow.current.flavor)) wow.current = wow.installs[0];
  $("#flavors").innerHTML = wow.installs.map((w) => `<button class="flavor${w.flavor === wow.current.flavor ? " active" : ""}" data-flavor="${w.flavor}">${esc(w.label)}</button>`).join("");
  $("#flavors").querySelectorAll(".flavor").forEach((b) => { b.onclick = () => { wow.current = wow.installs.find((w) => w.flavor === b.dataset.flavor); loadAddons(); }; });
  await refreshAddonList();
}

let wowupTimer;
async function refreshWowUp() {
  let s;
  try { s = await invoke("wowup_status"); } catch (e) { $("#wowup-status").textContent = String(e); return; }
  const btn = $("#wowup-action"), rm = $("#wowup-remove"), st = $("#wowup-status");
  clearInterval(wowupTimer);
  if (s.downloading) {
    const pct = s.total ? Math.round(s.downloaded * 100 / s.total) : 0;
    st.textContent = `Downloading WowUp… ${pct}% (${Math.round(s.downloaded / 1048576)} MB)`;
    btn.disabled = true; btn.textContent = "Downloading…";
    wowupTimer = setInterval(refreshWowUp, 1500);
    return;
  }
  btn.disabled = false;
  if (s.installed) {
    st.textContent = `WowUp ${s.version} is installed.`;
    btn.textContent = "Open WowUp";
    btn.onclick = async () => { try { addonsStatus(await invoke("wowup_launch")); } catch (e) { addonsStatus(String(e), true); } };
    rm.classList.remove("hidden");
    rm.onclick = async () => { if (!confirm("Remove WowUp? Your addons stay in place.")) return; try { await invoke("wowup_remove"); } catch (e) { addonsStatus(String(e), true); } refreshWowUp(); };
  } else {
    st.textContent = "Not installed. About 120 MB, downloaded from WowUp's GitHub releases.";
    btn.textContent = "Install WowUp";
    rm.classList.add("hidden");
    btn.onclick = async () => {
      btn.disabled = true; btn.textContent = "Downloading…"; st.textContent = "Starting download…";
      wowupTimer = setInterval(refreshWowUp, 1500);
      try { const v = await invoke("wowup_install"); addonsStatus(`WowUp ${v} installed.`); }
      catch (e) { addonsStatus(String(e), true); }
      clearInterval(wowupTimer);
      refreshWowUp();
    };
  }
}

async function refreshAddonList() {
  try {
    const list = await invoke("list_addons", { addonsDir: wow.current.addons_dir });
    $("#addon-list").innerHTML = list.length
      ? list.map((ad) => `<li><span class="t">${esc(ad.title)}<div class="v">${esc(ad.folder)}${ad.version ? " · " + esc(ad.version) : ""}</div></span><button class="rm" data-folder="${esc(ad.folder)}">Remove</button></li>`).join("")
      : `<li class="muted" style="background:none;border:0">No addons installed for ${esc(wow.current.label)}.</li>`;
    $("#addon-list").querySelectorAll(".rm").forEach((b) => { b.onclick = async () => {
      if (!confirm(`Remove ${b.dataset.folder}?`)) return;
      try { await invoke("remove_addon", { addonsDir: wow.current.addons_dir, folder: b.dataset.folder }); addonsStatus(`Removed ${b.dataset.folder}.`); }
      catch (e) { addonsStatus(String(e), true); }
      refreshAddonList();
    }; });
  } catch (e) { addonsStatus(String(e), true); }
}

async function installZipFile(file) {
  addonsStatus(`Installing ${file.name}…`);
  try {
    const bytes = new Uint8Array(await file.arrayBuffer());
    const folders = await invoke("install_addon_bytes", bytes, { headers: { "x-addons-dir": wow.current.addons_dir } });
    addonsStatus(`Installed: ${folders.join(", ")}`);
  } catch (e) { addonsStatus(String(e), true); }
  refreshAddonList();
}

async function installFromUrl() {
  const url = $("#addon-url").value.trim();
  if (!url) return;
  addonsStatus("Downloading…");
  try {
    const folders = await invoke("install_addon_url", { addonsDir: wow.current.addons_dir, flavor: wow.current.flavor, family: wow.current.family, url });
    addonsStatus(`Installed: ${folders.join(", ")}`);
    $("#addon-url").value = "";
  } catch (e) { addonsStatus(String(e), true); }
  refreshAddonList();
}

function showLog(on) {
  const el = $("#setup-log");
  clearInterval(logTimer);
  if (!on) { el.classList.add("hidden"); return; }
  el.classList.remove("hidden");
  const tick = async () => {
    try {
      const text = await invoke("read_log");
      const lines = text.split("\n").filter((l) => l.trim()).slice(-10);
      el.textContent = lines.join("\n") || "Waiting for output…";
      el.scrollTop = el.scrollHeight;
    } catch { /* ignore */ }
  };
  tick();
  logTimer = setInterval(tick, 2000);
}

// The Addons button only exists when World of Warcraft is actually installed in the prefix.
async function refreshAddonsButton() {
  try {
    const installs = await invoke("wow_installs");
    $("#open-addons").classList.toggle("hidden", installs.length === 0);
  } catch { $("#open-addons").classList.add("hidden"); }
}

async function refreshState(openSetupIfMissing = false) {
  try { state = await invoke("install_state"); } catch (e) { console.error(e); return state; }
  refreshAddonsButton();
  $("#launch").textContent = state.installed ? "Launch Battle.net" : "Set up Battle.net";
  $("#open-games").disabled = !state.installed;
  $("#open-games").title = state.installed ? "" : "Install Battle.net first";
  if (openSetupIfMissing && !state.installed) { showPanel("setup"); loadChecks(); }
  return state;
}

async function launch(game) {
  if (!state.installed) { showPanel("setup"); loadChecks(); return; }
  status(game ? `Starting Battle.net and launching ${game}…` : "Starting Battle.net…");
  try {
    await invoke("launch", { game: game || null });
    status(game ? `Battle.net is starting ${game}.` : "Battle.net is starting.");
  } catch (e) { status(String(e), true); }
}

async function showPanel(name) {
  document.querySelectorAll(".panel-page").forEach((p) => p.classList.toggle("active", p.id === `panel-${name}`));
  $("#panel-title").textContent = { settings: "Settings", games: "Play a game", disclaimer: "Disclaimer", setup: "Set up Battle.net", addons: "Addons" }[name] || "";
  if (name === "addons") loadAddons();
  $("#panel").classList.remove("hidden");
}

async function hidePanel() {
  $("#panel").classList.add("hidden");
}

function newerVersion(latest, current) {
  const a = latest.replace(/^v/, "").split(".").map(Number), b = current.split(".").map(Number);
  for (let i = 0; i < 3; i++) { if ((a[i] || 0) !== (b[i] || 0)) return (a[i] || 0) > (b[i] || 0); }
  return false;
}

async function checkUpdate(current) {
  try {
    const rel = await invoke("fetch_json", { url: RELEASES });
    if (rel.tag_name && newerVersion(rel.tag_name, current)) {
      const b = $("#update");
      b.textContent = `Update ${rel.tag_name}`;
      b.title = `You have v${current}. Opens the release page in your browser.`;
      b.onclick = () => invoke("open_external", { url: rel.html_url });
      b.classList.remove("hidden");
    }
  } catch { /* offline or rate-limited: stay quiet */ }
}

async function loadConfig() {
  const c = await invoke("read_config");
  $("#cfg-prefix").value = c.PREFIX || "";
  $("#cfg-proton").value = c.PROTON || "";
  const off = document.querySelector(`input[name=offload][value="${c.OFFLOAD || "auto"}"]`) || document.querySelector('input[name=offload][value="auto"]');
  off.checked = true;
}

async function saveConfig() {
  try {
    await invoke("write_config", { values: { PREFIX: $("#cfg-prefix").value, PROTON: $("#cfg-proton").value, OFFLOAD: document.querySelector("input[name=offload]:checked").value } });
    $("#cfg-status").textContent = "Saved.";
  } catch (e) { $("#cfg-status").textContent = String(e); }
  setTimeout(() => { $("#cfg-status").textContent = ""; }, 3000);
}

function initTitlebar() {
  const win = window.__TAURI__.window.getCurrentWindow();
  $("#win-min").onclick = () => win.minimize();
  $("#win-max").onclick = () => win.toggleMaximize();
  $("#win-close").onclick = () => win.close();
  $("#titlebar").addEventListener("dblclick", (e) => { if (!e.target.closest("button")) win.toggleMaximize(); });
}

async function init() {
  const version = await invoke("app_version");
  $("#version").textContent = version;
  $("#title-version").textContent = `v${version}`;
  initTitlebar();
  $("#games").innerHTML = GAMES.map(([code, name]) =>
    `<button class="tile" data-game="${code}"><span class="name">${name}</span><span class="code">${code}</span></button>`).join("");
  $("#games").addEventListener("click", (e) => {
    const t = e.target.closest("[data-game]");
    if (t) { hidePanel().then(() => launch(t.dataset.game)); }
  });
  $("#launch").onclick = () => launch();
  $("#open-games").onclick = () => showPanel("games");
  $("#home").onclick = () => { $("#site").src = FORUM; };
  $("#panel").addEventListener("click", (e) => { if (e.target === $("#panel")) hidePanel(); });
  $("#open-settings").onclick = () => showPanel("settings");
  $("#open-addons").onclick = () => showPanel("addons");
  $("#addons-open").onclick = () => invoke("open_addons_folder", { addonsDir: wow.current.addons_dir }).catch((e) => addonsStatus(String(e), true));
  $("#addon-zip").addEventListener("change", (e) => { const f = e.target.files[0]; if (f) installZipFile(f); e.target.value = ""; });
  $("#addon-url-go").onclick = installFromUrl;
  $("#addon-url").addEventListener("keydown", (e) => { if (e.key === "Enter") installFromUrl(); });
  $("#open-disclaimer").onclick = () => showPanel("disclaimer");
  $("#close").onclick = hidePanel;
  document.addEventListener("keydown", (e) => { if (e.key === "Escape" && !$("#panel").classList.contains("hidden")) hidePanel(); });
  document.querySelectorAll("a[data-url]").forEach((a) => { a.onclick = (e) => { e.preventDefault(); invoke("open_external", { url: a.dataset.url }); }; });
  $("#cfg-save").onclick = saveConfig;
  $("#dpi-apply").onclick = async () => {
    $("#diag").textContent = "Applying DPI…";
    try { $("#diag").textContent = await invoke("set_dpi", { dpi: Number($("#dpi").value) }); }
    catch (e) { $("#diag").textContent = String(e); }
  };
  $("#doctor").onclick = async () => {
    $("#diag").textContent = "Running doctor…";
    try { $("#diag").textContent = await invoke("doctor"); } catch (e) { $("#diag").textContent = String(e); }
  };
  $("#log").onclick = async () => { $("#diag").textContent = (await invoke("read_log")) || "(log is empty)"; };
  $("#do-install").onclick = async () => {
    $("#setup-status").textContent = "Downloading the installer from Blizzard. First runs also fetch Proton and its runtime (about 1 GB); the installer window opens when that's done.";
    showLog(true);
    try { await invoke("install"); } catch (e) { $("#setup-status").textContent = String(e); showLog(false); return; }
    const started = Date.now();
    const poll = setInterval(async () => {
      const s = await refreshState();
      if (s.installed) { clearInterval(poll); showLog(false); $("#setup-status").textContent = "Battle.net is installed."; setTimeout(hidePanel, 1200); }
      else if (Date.now() - started > 20 * 60 * 1000) { clearInterval(poll); $("#setup-status").textContent = "Still not installed. The log above shows what happened."; }
    }, 4000);
  };
  $("#do-import").onclick = async () => {
    $("#setup-status").textContent = "Checking…";
    try {
      const msg = await invoke("import_prefix", { path: $("#import-path").value });
      $("#setup-status").textContent = msg.trim();
      await loadConfig();
      if ((await refreshState()).installed) setTimeout(hidePanel, 1000);
    } catch (e) { $("#setup-status").textContent = String(e).trim(); }
  };
  loadConfig();
  refreshState(true);
  checkUpdate(version);
  setInterval(refreshAddonsButton, 60000);   // picks up a WoW install made after launch
}

init();
