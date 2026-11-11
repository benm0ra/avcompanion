// Front end talks to Rust over Tauri IPC only. It never opens the daemon socket.
const { invoke } = window.__TAURI__.core;
const $ = (id) => document.getElementById(id);

let draggingGain = false;

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
    $("hint").textContent = d.suggested_output
      ? ""
      : "No virtual cable found. Install VB-Cable (Windows) or BlackHole (macOS), then refresh.";
    $("error").textContent = "";
  } catch (e) {
    $("error").textContent = String(e);
  }
}

async function start() {
  $("error").textContent = "";
  try {
    await invoke("start_audio", { input: $("input").value, output: $("output").value });
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
    pill.textContent = "Running";
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

  if (!draggingGain) $("gain").value = s.gain_cdb;
  $("gainval").textContent = (s.gain_cdb / 100).toFixed(1) + " dB";
  $("port").textContent = s.daemon_port ? "127.0.0.1:" + s.daemon_port : "starting";
  $("xruns").textContent = s.xruns;
}

async function poll() {
  try {
    render(await invoke("get_state"));
  } catch (_) {
    /* app shutting down */
  }
}

$("start").addEventListener("click", start);
$("stop").addEventListener("click", () => invoke("stop_audio"));
$("refresh").addEventListener("click", refreshDevices);
$("mute").addEventListener("click", async () => {
  const s = await invoke("get_state");
  await invoke("set_mute", { on: !s.mute });
});
$("gain").addEventListener("pointerdown", () => (draggingGain = true));
$("gain").addEventListener("pointerup", () => (draggingGain = false));
$("gain").addEventListener("input", (e) => invoke("set_gain", { cdb: Number(e.target.value) }));
$("unity").addEventListener("click", () => invoke("set_gain", { cdb: 0 }));

refreshDevices();
setInterval(poll, 60);
