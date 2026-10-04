// BlizzNux launcher control bar. The BlizzNux.com view sits above this bar; the Rust side
// resizes this webview between "bar" (bottom strip) and "full" (settings, disclaimer).
const { invoke } = window.__TAURI__.core;
const $ = (s) => document.querySelector(s);

const FORUM = "https://blizznux.com/";
const SITE_ORIGIN = "https://blizznux.com";
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

// ---- BlizzNux account ----
let account = { linked: false, username: "", sharing: false };

async function refreshAccount() {
  try { account = await invoke("account_status"); } catch (e) { console.error(e); return account; }
  $("#acct-unlinked").classList.toggle("hidden", account.linked);
  $("#acct-linked").classList.toggle("hidden", !account.linked);
  $("#acct-name").textContent = account.username;
  $("#cfg-share").checked = account.sharing;
  $("#cfg-bug-auto").checked = account.bug_auto;
  return account;
}

// Pacing of the login check. Every check starts the whole forum software on the server, so a
// launcher nobody logs in to must not keep asking: quick while a login is likely, slower after
// that, and not at all once the pairing has expired. A page load in the embedded view checks at
// once, because finishing the login reloads the page.
const POLL_FAST_MS = 2000, POLL_SLOW_MS = 15000, POLL_FAST_FOR_MS = 120000;
const POLL_MAX_FAILURES = 5, REPAIRS_PER_RUN = 3;
let pollTimer = null, pollStarted = 0, pollFailures = 0;
let pairing = false;        // a pairing is open on the site and worth asking about
let ownNavigation = false;  // the next page load in the embedded view is the launcher's doing
let repairs = 0;

function stopPolling() { clearTimeout(pollTimer); pollTimer = null; }
function schedulePoll(ms) { stopPolling(); pollTimer = setTimeout(pollLogin, ms); }
function pollDelay() { return Date.now() - pollStarted < POLL_FAST_FOR_MS ? POLL_FAST_MS : POLL_SLOW_MS; }
// Every page the launcher itself puts in the embedded view goes through here, so that its load is
// not mistaken for the user having logged in.
function showSite(url) { ownNavigation = true; $("#site").src = url; }

// Not logged in: the embedded view shows the site's front page, where the site opens its login
// box once and lets the visitor close it. The launcher pairs itself in the background and
// collects the token once a login lands; an account stays optional.
async function startLogin({ navigate = true, delay = 0 } = {}) {
  stopPolling();
  try {
    const s = await invoke("link_start");
    pairing = true; pollStarted = Date.now(); pollFailures = 0;
    if (navigate) {
      // A page that has only just loaded gets a moment to finish its own requests first: loading
      // the next one on top of it cuts them short, and the site then flashes an error.
      if (delay) await new Promise((done) => setTimeout(done, delay));
      showSite(s.url);
    }
    schedulePoll(POLL_FAST_MS);
  } catch (e) {
    pairing = false;
    console.error("pairing not started:", e);   // site unreachable: plain forum, try again next launch
    if (navigate) showSite(FORUM);
  }
}

let pollBusy = false;

async function pollLogin() {
  if (pollBusy || account.linked || !pairing) return;   // one poll in flight; nothing to ask once linked
  pollBusy = true;
  stopPolling();
  let next = null;
  try {
    const r = await invoke("link_poll");
    if (r.status === "linked") {
      pairing = false;
      await refreshAccount();
      // The view stays where it is: the login happened on the site's own page, which has just
      // reloaded as logged in. Loading it once more would cut that page's requests short.
      if (r.returning) status(`Logged in as ${r.username}.`); else askSharing(r.username);
    } else if (r.status === "expired" || r.status === "none") {
      pairing = false;   // no new pairing by itself: a login or the Log in button starts the next one
    } else if (r.status === "busy") {
      // The site asked for a pause (too many requests, or it is down): wait, then carry on.
      if (++pollFailures < POLL_MAX_FAILURES) next = Math.max(r.retry_after || 30, 5) * 1000; else pairing = false;
    } else {
      pollFailures = 0;
      next = pollDelay();
    }
  } catch (e) {
    pairing = false;
    console.error(e);
  } finally {
    pollBusy = false;
  }
  if (next !== null && pairing) schedulePoll(next);
}

// The embedded view finished loading a page. While a pairing is open that may be the login having
// gone through, so ask right away. After the pairing has expired it is the only sign of a late
// login: pair again, a few times per run at most.
function onSiteLoaded() {
  const own = ownNavigation;
  ownNavigation = false;
  if (account.linked) { if (!own) checkLink("load"); return; }   // logging out on the site reloads the page
  if (own) return;
  if (pairing) pollLogin();
  else if (repairs < REPAIRS_PER_RUN) { repairs++; startLogin({ delay: SETTLE_MS }); }
}

// There is one login, not two that can disagree. The launcher asks the site whether its link
// still stands at start and whenever the embedded view loads a page by itself. A link the site
// has ended (revoked on the profile, logged out in the view) is dropped here as well, and the
// login page comes back; logging in there links the launcher again without further questions.
const LINK_CHECK_SPACING_MS = 10000, LINK_CHECK_EVERY_MS = 600000, SETTLE_MS = 1500;
let lastLinkCheck = 0, linkCheckTimer = null;

// `why` is what prompted the question: "start", a page "load" in the embedded view, or the "timer".
async function checkLink(why = "timer") {
  if (!account.linked) return;
  const wait = LINK_CHECK_SPACING_MS - (Date.now() - lastLinkCheck);
  if (wait > 0) {
    // Asked a moment ago. Ask again when the spacing allows instead of dropping the question:
    // a logout right after the launcher started would otherwise go unnoticed.
    if (!linkCheckTimer) linkCheckTimer = setTimeout(() => { linkCheckTimer = null; checkLink(why); }, wait + 50);
    return;
  }
  lastLinkCheck = Date.now();
  try {
    const r = await invoke("link_check");
    if (r.status !== "revoked") return;
    await refreshAccount();
    status("You are logged out of BlizzNux.com.");
    // Found on a page load: the user has just logged out on the site and is looking at its front
    // page. Pair quietly and leave that page alone; the login box is for a launcher that starts
    // logged out, not for someone who has just chosen to log out. Otherwise show the front page
    // afresh, after the page that is loading at start has settled.
    if (why === "load") startLogin({ navigate: false });
    else startLogin({ delay: why === "start" ? SETTLE_MS : 0 });
  } catch (e) { console.error(e); }
}

// One inline question in the bar after linking; no dialog. Settings holds the switches after that.
function askSharing(username) {
  $("#bar-notice-text").textContent = `Logged in as ${username}. Share compatibility reports with the community?`;
  $("#bar-notice").classList.remove("hidden");
  const done = async (yes) => {
    $("#bar-notice").classList.add("hidden");
    try { await invoke("write_config", { values: { REPORTS_SHARE: yes ? "1" : "0" } }); await refreshAccount(); } catch (e) { status(String(e), true); }
    status(yes ? "Sharing verification reports; blizznux.com fills your profile's setup fields from them. Bug reports can be automated in Settings." : "Not sharing. You can change this in Settings.");
  };
  $("#notice-yes").onclick = () => done(true);
  $("#notice-no").onclick = () => done(false);
}

// ---- reports to BlizzNux ----
let report = { kind: "run", game: null, outcome: null, body: null };

async function openReport(kind, game, intro) {
  report = { kind, game, outcome: null, body: null };
  $("#report-title").textContent = kind === "run" ? `How did ${game?.name || "the game"} run?` : "Report a problem";
  $("#report-intro").textContent = intro || "";
  $("#outcomes").classList.toggle("hidden", kind !== "run");
  document.querySelectorAll(".outcome").forEach((b) => b.classList.remove("active"));
  $("#report-comment").value = "";
  $("#report-preview").textContent = "";
  $("#report-status").textContent = "";
  $("#report-send").disabled = kind === "run";
  $("#report-comment").placeholder = kind === "run" ? "Anything worth noting? Optional." : "What went wrong? The launcher log is attached automatically.";
  showPanel("report");
  if (kind === "bug") await previewReport();
}

async function previewReport() {
  try {
    report.body = await invoke("build_report", { kind: report.kind, game: report.game?.code || null, outcome: report.outcome, comment: $("#report-comment").value });
    $("#report-preview").textContent = JSON.stringify(report.body, null, 2);
    $("#report-send").disabled = false;
  } catch (e) { report.body = null; $("#report-preview").textContent = ""; $("#report-status").textContent = String(e); }
}

async function sendReport() {
  $("#report-send").disabled = true; $("#report-status").textContent = "Sending…";
  try {
    const r = await invoke("submit_report", { kind: report.kind, game: report.game?.code || null, outcome: report.outcome, comment: $("#report-comment").value });
    if (r.sent) {
      $("#report-status").textContent = r.url ? "Sent. Thank you." : "Sent.";
      setTimeout(hidePanel, 1500);
    } else {
      $("#report-status").textContent = `${r.reason} Saved: the launcher tries again each time it starts, for up to a week.`;
    }
  } catch (e) { $("#report-status").textContent = String(e); $("#report-send").disabled = false; }
}

// A session of under a minute is never reported by itself: a lost connection, a login queue and
// quitting look the same as a crash. Ask, and send only what the user picks.
async function onGameClosedEarly(ev) {
  const g = ev.payload || {};
  await openReport("run", { code: g.code, name: g.name }, `${g.name} closed after ${g.seconds} seconds. If it crashed or would not start, choose Broken. If you closed it yourself or lost the connection to Blizzard, close this and nothing is sent.`);
}

async function onGameEnded(ev) {
  const g = ev.payload || {};
  const mins = Math.round((g.seconds || 0) / 60);
  const auto = (await invoke("read_config")).REPORTS_AUTO === "1";
  await openReport("run", { code: g.code, name: g.name }, `${g.name} ran for about ${mins} minute${mins === 1 ? "" : "s"}. Your setup and the game build are attached${auto ? " and will be sent as soon as you pick an answer" : ""}.`);
  report.auto = auto;
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

// What the launcher sees running in its prefix; kept current by the watcher on the Rust side,
// whoever started Battle.net and whenever. The button says so instead of starting a second copy.
let session = { battlenet: false, game: null };

function showSession(s) {
  if (s) session = s;
  $("#launch").textContent = !state.installed ? "Set up Battle.net" : session.battlenet ? "Battle.net is running" : "Launch Battle.net";
}

async function refreshState(openSetupIfMissing = false) {
  try { state = await invoke("install_state"); } catch (e) { console.error(e); return state; }
  refreshAddonsButton();
  showSession();
  if (openSetupIfMissing && !state.installed) { showPanel("setup"); loadChecks(); }
  return state;
}

async function launch(game) {
  if (!state.installed) { showPanel("setup"); loadChecks(); return; }
  if (session.battlenet) { status(session.game ? `${session.game} is running.` : "Battle.net is already running."); return; }
  status(game ? `Starting Battle.net and launching ${game}…` : "Starting Battle.net…");
  try {
    await invoke("launch", { game: game || null });
    status(game ? `Battle.net is starting ${game}.` : "Battle.net is starting.");
  } catch (e) { status(String(e), true); }
}

async function showPanel(name) {
  document.querySelectorAll(".panel-page").forEach((p) => p.classList.toggle("active", p.id === `panel-${name}`));
  $("#panel-title").textContent = { settings: "Settings", disclaimer: "Disclaimer", setup: "Set up Battle.net", addons: "Addons", report: "Report" }[name] || "";
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
  const b = $("#update");
  // 1. Signed in-app update (AppImage builds): downloads, verifies and restarts.
  try {
    const u = await invoke("update_check");
    if (u.available) {
      b.textContent = u.can_install ? `Update to v${u.version}` : `Update v${u.version}`;
      b.title = u.can_install ? `You have v${current}. Click to download, verify and restart.` : `You have v${current}. Opens the release page in your browser.`;
      b.onclick = u.can_install ? installUpdate : () => invoke("open_external", { url: u.url });
      b.classList.remove("hidden");
      return;
    }
    return; // reachable and up to date
  } catch { /* no update manifest yet, or offline: fall back to the release list */ }
  // 2. Fallback: compare with the latest GitHub release and open its page.
  try {
    const rel = await invoke("fetch_json", { url: RELEASES });
    if (rel.tag_name && newerVersion(rel.tag_name, current)) {
      b.textContent = `Update ${rel.tag_name}`;
      b.title = `You have v${current}. Opens the release page in your browser.`;
      b.onclick = () => invoke("open_external", { url: rel.html_url });
      b.classList.remove("hidden");
    }
  } catch { /* offline or rate-limited: stay quiet */ }
}

async function installUpdate() {
  const b = $("#update");
  b.disabled = true; b.textContent = "Downloading…";
  const un = await window.__TAURI__.event.listen("update-progress", (e) => {
    const p = e.payload || {}; if (p.total) b.textContent = `Downloading ${Math.round(p.downloaded * 100 / p.total)}%`;
  });
  try { await invoke("update_install"); }
  catch (e) { status(`Update failed: ${e}`, true); b.disabled = false; b.textContent = "Update"; }
  un();
}

async function checkBattlenet() {
  try {
    const r = await invoke("battlenet_update_check");
    if (r.available) status(`Battle.net ${r.latest} is out (you have ${r.installed}). Launch Battle.net and it updates itself.`);
  } catch { /* offline: stay quiet */ }
}

async function loadConfig() {
  const c = await invoke("read_config");
  $("#cfg-prefix").value = c.PREFIX || "";
  $("#cfg-reports-auto").checked = c.REPORTS_AUTO === "1";
  refreshAccount();
  $("#cfg-proton").value = c.PROTON || "";
  const off = document.querySelector(`input[name=offload][value="${c.OFFLOAD || "auto"}"]`) || document.querySelector('input[name=offload][value="auto"]');
  off.checked = true;
}

async function saveConfig() {
  try {
    await invoke("write_config", { values: { PREFIX: $("#cfg-prefix").value, PROTON: $("#cfg-proton").value, OFFLOAD: document.querySelector("input[name=offload]:checked").value, REPORTS_AUTO: $("#cfg-reports-auto").checked ? "1" : "0", REPORTS_SHARE: $("#cfg-share").checked ? "1" : "0", BUG_AUTO: $("#cfg-bug-auto").checked ? "1" : "0" } });
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
  $("#launch").onclick = () => launch();
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
  $("#report-bug").onclick = () => openReport("bug", null, "Describe what went wrong. The last lines of the launcher log and your setup are attached; home paths are replaced with ~.");
  document.querySelectorAll(".outcome").forEach((b) => { b.onclick = async () => {
    document.querySelectorAll(".outcome").forEach((o) => o.classList.toggle("active", o === b));
    report.outcome = b.dataset.outcome;
    $("#report-send").disabled = false;   // an answer that cannot be previewed can still be saved
    await previewReport();
    if (report.auto) sendReport();
  }; });
  $("#report-send").onclick = sendReport;
  $("#report-skip").onclick = hidePanel;
  $("#report-comment").addEventListener("change", () => { if (report.body) previewReport(); });
  window.__TAURI__.event.listen("game-ended", onGameEnded);
  window.__TAURI__.event.listen("session-state", (e) => showSession(e.payload));
  window.__TAURI__.event.listen("game-closed-early", onGameClosedEarly);
  window.__TAURI__.event.listen("launch-result", (e) => { if (!(e.payload || {}).ok) status("Battle.net did not start within two minutes. Check the log in Settings.", true); });
  window.__TAURI__.event.listen("report-sent", (e) => { const p = e.payload || {}; status(`Shared ${p.kind === "launch" ? "launch" : "game"} report (${p.outcome}).`); });
  $("#acct-link").onclick = () => { hidePanel(); startLogin(); };
  $("#site").addEventListener("load", onSiteLoaded);
  $("#cfg-bug-auto").addEventListener("change", saveConfig);
  $("#cfg-share").addEventListener("change", saveConfig);
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
  try { showSession(await invoke("session_state")); } catch (e) { console.error(e); }
  const st = await refreshState(true);
  const acct = await refreshAccount();
  // One navigation only: two loads racing before the session cookie exists leave the page with a
  // CSRF token that does not match the stored session, and the first login attempt fails.
  if (st.installed && !acct.linked) startLogin(); else { showSite(FORUM); checkLink("start"); }
  checkUpdate(version);
  checkBattlenet();
  invoke("flush_reports").then((n) => { if (n) status(n === 1 ? "Sent the report saved earlier." : `Sent the ${n} reports saved earlier.`); }).catch(() => {});
  setInterval(refreshAddonsButton, 60000);   // picks up a WoW install made after launch
  setInterval(() => checkLink("timer"), LINK_CHECK_EVERY_MS);   // a link ended from another browser is noticed without a restart
}

init();
