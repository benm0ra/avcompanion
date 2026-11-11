// Front end talks to Rust over Tauri IPC only. It never opens the daemon socket.
const { invoke } = window.__TAURI__.core;
const $ = (id) => document.getElementById(id);

let draggingGain = false;
const mode = () => document.querySelector('input[name="mode"]:checked').value;

function fill(select, names, preferred) {
  const keep = select.value;
  select.innerHTML = "";
  for (const n of names) {
    const o = document.createElement("option");
    o.value = o.textContent = n;
    select.appendChild(o);
  }
  const pick = names.includes(keep) ? keep : preferred;
  if (pick && names.includes(pick)) select.value = pick;
}

async function refreshDevices() {
  try {
    const d = await invoke("list_devices");
    fill($("input"), d.inputs, d.default_input);
    fill($("output"), d.outputs, d.suggested_output);
    $("error").textContent = "";
  } catch (e) {
    $("error").textContent = String(e);
  }
  await refreshMode();
}

async function refreshMode() {
  const cable = mode() === "cable";
  $("cableBox").hidden = !cable;
  $("hint").textContent = cable
    ? "In Zoom, choose the cable's output (for VB-CABLE: \"CABLE Output\") as your microphone."
    : "Zoom keeps using this microphone. In Zoom's audio settings, turn off \"Automatically adjust microphone volume\".";
  if (!cable) return;
  try {
    const vb = await invoke("vbcable_status");
    $("vbBox").hidden = vb.installed;
    $("vbInstall").hidden = !vb.bundled;
    $("vbNote").textContent = vb.bundled
      ? "Windows will ask for administrator approval, then VB-Audio's setup opens. Restart the PC if it asks, then press Refresh devices."
      : "Download it from www.vb-cable.com, install it, then press Refresh devices.";
  } catch (_) {
    $("vbBox").hidden = true;
  }
}

async function start() {
  $("error").textContent = "";
  try {
    const args = { mode: mode(), input: $("input").value, output: null };
    if (args.mode === "cable") args.output = $("output").value || null;
    await invoke("start_audio", args);
  } catch (e) {
    $("error").textContent = String(e);
  }
}

function render(s) {
  const pill = $("route");
  if (s.lost) {
    pill.textContent = "Device lost";
    pill.className = "pill bad";
    $("error").textContent = "An audio device was disconnected or reconfigured. Press Start to reconnect.";
  } else if (s.route.running) {
    pill.textContent = s.mode === "direct" ? "Running · direct" : "Running · full processing";
    pill.className = "pill ok";
  } else {
    pill.textContent = "Stopped";
    pill.className = "pill";
  }

  const m = $("mute");
  m.textContent = s.mute ? "MUTED" : "LIVE";
  m.className = "mute " + (s.mute ? "muted" : "live");

  // Meter spans -60..0 dBFS.
  const pct = Math.max(0, Math.min(100, ((s.peak_dbfs + 60) / 60) * 100));
  $("bar").style.width = pct + "%";
  $("peak").textContent = s.peak_dbfs <= -119 ? "-inf dBFS" : s.peak_dbfs.toFixed(1) + " dBFS";
  $("clip").className = "clip" + (s.clip ? " on" : "");

  const g = $("gain");
  g.min = s.gain_min_cdb;
  g.max = s.gain_max_cdb;
  if (!draggingGain) g.value = s.gain_cdb;
  $("gainval").textContent = (s.gain_cdb / 100).toFixed(1) + " dB";
  // 0 dB is outside some microphones' range in direct mode.
  $("unity").hidden = s.gain_min_cdb > 0 || s.gain_max_cdb < 0;
  $("port").textContent = s.daemon_port ? "127.0.0.1:" + s.daemon_port : "starting";
  $("xruns").textContent = s.xruns;
}

async function poll() {
  // A hidden or minimised window has nothing to draw.
  if (document.hidden) return;
  try {
    render(await invoke("get_state"));
  } catch (_) {
    /* app shutting down */
  }
}

$("start").addEventListener("click", start);
$("stop").addEventListener("click", () => invoke("stop_audio"));
$("refresh").addEventListener("click", refreshDevices);
for (const r of document.querySelectorAll('input[name="mode"]')) r.addEventListener("change", refreshMode);
$("vbInstall").addEventListener("click", async () => {
  try {
    await invoke("install_vbcable");
  } catch (e) {
    $("error").textContent = String(e);
  }
});
$("mute").addEventListener("click", () => invoke("toggle_mute"));
$("gain").addEventListener("pointerdown", () => (draggingGain = true));
$("gain").addEventListener("pointerup", () => (draggingGain = false));
$("gain").addEventListener("input", (e) => invoke("set_gain", { cdb: Number(e.target.value) }));
$("unity").addEventListener("click", () => invoke("set_gain", { cdb: 0 }));

// ---- Zoom remote
async function zoom(name, args) {
  const msg = $("zMsg");
  msg.textContent = "Working...";
  msg.className = "hint";
  try {
    msg.textContent = await invoke("zoom_command", { name, args: args || null });
  } catch (e) {
    msg.textContent = String(e);
    msg.className = "error";
  }
}
function remember(id, key, prop) {
  const el = $(id);
  try {
    const saved = localStorage.getItem(key);
    if (saved !== null) el[prop] = prop === "checked" ? saved === "1" : saved;
    el.addEventListener("change", () => localStorage.setItem(key, prop === "checked" ? (el.checked ? "1" : "0") : el.value));
  } catch (_) {
    /* storage unavailable: the fields just start empty */
  }
}
remember("zMatch", "zoom.match", "value");
remember("zSound", "zoom.sound", "checked");
$("zShare").addEventListener("click", () => zoom("zoom.share.open"));
$("zPause").addEventListener("click", () => zoom("zoom.share.pause"));
$("zWindow").addEventListener("click", () => zoom("zoom.share.window", { match: $("zMatch").value, sound: $("zSound").checked }));
$("zDiag").addEventListener("click", () => zoom("zoom.diagnostics"));

refreshDevices();
setInterval(poll, 60);
