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

async function launch(game) {
  status(game ? `Starting Battle.net and launching ${game}…` : "Starting Battle.net…");
  try {
    await invoke("launch", { game: game || null });
    status(game ? `Battle.net is starting ${game}.` : "Battle.net is starting.");
  } catch (e) { status(String(e), true); }
}

async function showPanel(name) {
  document.querySelectorAll(".panel-page").forEach((p) => p.classList.toggle("active", p.id === `panel-${name}`));
  $("#panel-title").textContent = { settings: "Settings", games: "Play a game", disclaimer: "Disclaimer" }[name] || "";
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
  loadConfig();
  checkUpdate(version);
}

init();
