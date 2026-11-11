"use strict";
// Settings panel for the Zoom Share key. Stream Deck calls
// connectElgatoStreamDeckSocket when the panel opens; the panel saves the
// key's settings with setSettings and the plugin receives them on each press.
let socket = null;
let context = null;

const el = (id) => document.getElementById(id);

function readForm() {
  return { mode: el("mode").value, match: el("match").value, sound: el("sound").checked };
}

function showForm(settings) {
  el("mode").value = settings.mode || "open";
  el("match").value = settings.match || "";
  el("sound").checked = settings.sound !== false;
  el("windowBox").hidden = el("mode").value !== "window";
}

function save() {
  el("windowBox").hidden = el("mode").value !== "window";
  if (socket && socket.readyState === 1) {
    socket.send(JSON.stringify({ event: "setSettings", context, payload: readForm() }));
  }
}

// eslint-disable-next-line no-unused-vars
function connectElgatoStreamDeckSocket(port, uuid, registerEvent, _info, actionInfo) {
  context = uuid;
  let initial = {};
  try {
    initial = (JSON.parse(actionInfo).payload || {}).settings || {};
  } catch (_) {
    /* no saved settings yet */
  }
  showForm(initial);
  socket = new WebSocket("ws://127.0.0.1:" + port);
  socket.onopen = () => socket.send(JSON.stringify({ event: registerEvent, uuid }));
}

for (const id of ["mode", "match", "sound"]) {
  el(id).addEventListener("change", save);
  el(id).addEventListener("input", save);
}
