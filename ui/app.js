// BlizzNux launcher UI. Talks to the Rust side through Tauri commands.
const { invoke } = window.__TAURI__.core;
const $ = (s) => document.querySelector(s);

const GAMES = [
  ["WoW", "World of Warcraft"], ["WTCG", "Hearthstone"], ["Hero", "Heroes of the Storm"],
  ["Pro", "Overwatch 2"], ["Fen", "Diablo IV"], ["D3", "Diablo III"],
  ["OSI", "Diablo II: Resurrected"], ["S2", "StarCraft II"], ["W3", "Warcraft III: Reforged"],
];
const FORUM = "https://blizznux.com";
const FEED = `${FORUM}/api/discussions?filter[tag]=updates&sort=-createdAt&page[limit]=8&include=firstPost`;
const RELEASES = "https://api.github.com/repos/BlizzNux/launcher/releases/latest";

function status(msg, err = false) {
  const el = $("#status"); el.textContent = msg; el.classList.toggle("err", err);
}

async function launch(game) {
  status(game ? `Starting Battle.net and launching ${game}…` : "Starting Battle.net…");
  try {
    await invoke("launch", { game: game || null });
    status(game ? `Battle.net is starting ${game}.` : "Battle.net is starting.");
  } catch (e) { status(String(e), true); }
}

function showPage(name) {
  document.querySelectorAll(".nav").forEach((b) => b.classList.toggle("active", b.dataset.page === name));
  document.querySelectorAll(".page").forEach((p) => p.classList.toggle("active", p.id === `page-${name}`));
  invoke("community_visible", { visible: name === "community" }).catch(() => {});
}

function renderGames() {
  $("#games").innerHTML = GAMES.map(([code, name]) =>
    `<div class="tile"><div class="name">${name}</div><div class="code">${code}</div><button data-game="${code}">Play</button></div>`).join("");
  $("#games").addEventListener("click", (e) => { const g = e.target.dataset.game; if (g) launch(g); });
}

function stripHtml(s) { const d = document.createElement("div"); d.innerHTML = s || ""; return (d.textContent || "").replace(/\s+/g, " ").trim(); }

async function loadNews() {
  const list = $("#news");
  try {
    const data = await invoke("fetch_json", { url: FEED });
    const posts = Object.fromEntries((data.included || []).filter((p) => p.type === "posts").map((p) => [p.id, p]));
    const items = (data.data || []).map((d) => {
      const a = d.attributes, fp = d.relationships?.firstPost?.data?.id;
      const excerpt = stripHtml(posts[fp]?.attributes?.contentHtml).slice(0, 160);
      return { id: d.id, slug: a.slug, title: a.title, date: (a.createdAt || "").slice(0, 10), comments: a.commentCount, excerpt };
    });
    if (!items.length) { list.innerHTML = `<li class="muted">No updates yet.</li>`; return; }
    list.innerHTML = items.map((i) =>
      `<li data-url="${FORUM}/d/${i.slug}"><div class="title">${i.title}</div><div class="meta">${i.date} · ${i.comments} comment${i.comments === 1 ? "" : "s"}</div>${i.excerpt ? `<div class="excerpt">${i.excerpt}</div>` : ""}</li>`).join("");
  } catch (e) {
    list.innerHTML = `<li class="muted">Could not load updates: ${e}</li>`;
  }
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
      $("#update-text").textContent = `BlizzNux ${rel.tag_name} is available (you have v${current}).`;
      $("#update-btn").onclick = () => invoke("open_external", { url: rel.html_url });
      $("#update-banner").classList.remove("hidden");
    }
  } catch { /* offline or rate-limited: stay quiet */ }
}

async function loadConfig() {
  const c = await invoke("read_config");
  $("#cfg-prefix").value = c.PREFIX || "";
  $("#cfg-proton").value = c.PROTON || "";
  $("#cfg-offload").value = c.OFFLOAD || "auto";
}

async function saveConfig() {
  try {
    await invoke("write_config", { values: { PREFIX: $("#cfg-prefix").value, PROTON: $("#cfg-proton").value, OFFLOAD: $("#cfg-offload").value } });
    $("#cfg-status").textContent = "Saved.";
  } catch (e) { $("#cfg-status").textContent = String(e); }
  setTimeout(() => { $("#cfg-status").textContent = ""; }, 3000);
}

async function init() {
  const version = await invoke("app_version");
  $("#version").textContent = version;
  document.querySelectorAll(".nav").forEach((b) => b.addEventListener("click", () => showPage(b.dataset.page)));
  $("#launch-main").onclick = () => launch();
  $("#launch-side").onclick = () => launch();
  $("#news").addEventListener("click", (e) => {
    const li = e.target.closest("li[data-url]");
    if (li) { invoke("community_navigate", { url: li.dataset.url }).then(() => showPage("community")).catch((err) => status(String(err), true)); }
  });
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
  renderGames();
  showPage("home");
  loadConfig();
  loadNews();
  checkUpdate(version);
}

init();
