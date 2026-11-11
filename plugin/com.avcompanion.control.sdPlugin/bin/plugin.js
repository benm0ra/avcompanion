"use strict";
/*
 * AVcompanion Stream Deck plugin. No dependencies: it speaks the Stream Deck
 * plugin WebSocket protocol directly and uses Node's built-in WebSocket
 * (Node.js 22+; the manifest asks Stream Deck for Node.js 24).
 *
 * Two sockets:
 *   1. Stream Deck app  <-> this plugin   (port given on the command line)
 *   2. this plugin      <-> AVcompanion   (port and token from daemon.json)
 *
 * The plugin keeps no mute or gain state of its own. Keys and dials are
 * drawn only from the daemon's `state` frames.
 */

const fs = require("node:fs");
const path = require("node:path");
const os = require("node:os");

const ACTION_MUTE = "com.avcompanion.control.mute";
const ACTION_GAIN = "com.avcompanion.control.gain";
const ACTION_ZOOM = "com.avcompanion.control.zoomshare";
const PROTO = 1;
const GAIN_MIN_CDB = -6000;
const GAIN_MAX_CDB = 2400;
const DIAL_COALESCE_MS = 15;
const BACKOFF_MIN_MS = 250;
const BACKOFF_MAX_MS = 5000;

// ---------------------------------------------------------------- helpers

function discoveryPath(env = process.env, platform = process.platform, home = os.homedir()) {
  if (platform === "win32") {
    const base = env.APPDATA || path.join(home, "AppData", "Roaming");
    return path.join(base, "AVcompanion", "daemon.json");
  }
  if (platform === "darwin") {
    return path.join(home, "Library", "Application Support", "AVcompanion", "daemon.json");
  }
  return path.join(home, ".config", "AVcompanion", "daemon.json");
}

function readDiscovery(file = discoveryPath()) {
  try {
    const d = JSON.parse(fs.readFileSync(file, "utf8"));
    if (Number.isInteger(d.port) && typeof d.token === "string") return d;
  } catch (_) {
    /* app not running yet */
  }
  return null;
}

/** Stream Deck launches the plugin with: -port N -pluginUUID X -registerEvent E -info JSON */
function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length - 1; i++) {
    if (argv[i].startsWith("-")) out[argv[i].replace(/^-+/, "")] = argv[i + 1];
  }
  return out;
}

/** Position of `cdb` within the reachable range, 0-100. The app reports the range; direct mode narrows it to the microphone's own. */
function gainPct(cdb, min = GAIN_MIN_CDB, max = GAIN_MAX_CDB) {
  if (!Number.isFinite(min) || !Number.isFinite(max) || max <= min) {
    min = GAIN_MIN_CDB;
    max = GAIN_MAX_CDB;
  }
  const c = Math.max(min, Math.min(max, cdb));
  return Math.round(((c - min) / (max - min)) * 100);
}

function gainLabel(cdb) {
  const db = cdb / 100;
  return (db > 0 ? "+" : "") + db.toFixed(1) + " dB";
}

function svg(bg, glyph, label) {
  const mic =
    '<rect x="58" y="26" width="28" height="52" rx="14" fill="#fff"/>' +
    '<path d="M46 62a26 26 0 0 0 52 0" fill="none" stroke="#fff" stroke-width="7" stroke-linecap="round"/>' +
    '<path d="M72 88v14M56 102h32" stroke="#fff" stroke-width="7" stroke-linecap="round"/>';
  const slash = '<path d="M34 24L110 104" stroke="#fff" stroke-width="9" stroke-linecap="round"/>';
  const body = glyph === "slash" ? mic + slash : glyph === "none" ? "" : mic;
  const text = label
    ? `<text x="72" y="134" font-family="Arial, sans-serif" font-size="20" font-weight="bold" fill="#fff" text-anchor="middle">${label}</text>`
    : "";
  const s = `<svg xmlns="http://www.w3.org/2000/svg" width="144" height="144" viewBox="0 0 144 144"><rect width="144" height="144" fill="${bg}"/>${body}${text}</svg>`;
  return "data:image/svg+xml;charset=utf8," + encodeURIComponent(s);
}

/** One image per value of the daemon's `effective` field, plus offline. */
const KEY_IMG = {
  live: svg("#059669", "mic", "LIVE"),
  muted: svg("#dc2626", "slash", "MUTED"),
  zoom_muted: svg("#d97706", "slash", "ZOOM"),
  unverified: svg("#4b5563", "mic", "?"),
  offline: svg("#1f2937", "none", "OFFLINE"),
};

// ------------------------------------------------------------ daemon link

class DaemonLink {
  /**
   * opts.WebSocketImpl, opts.readDiscovery, opts.setTimeout are injectable for tests.
   * opts.onState(state|null) is called with every state frame, and with null on disconnect.
   */
  constructor(opts) {
    this.WS = opts.WebSocketImpl || globalThis.WebSocket;
    this.readDiscovery = opts.readDiscovery || readDiscovery;
    this.setTimeout = opts.setTimeout || setTimeout;
    this.onState = opts.onState || (() => {});
    this.onAck = opts.onAck || (() => {});
    this.ws = null;
    this.ready = false;
    this.backoff = BACKOFF_MIN_MS;
    this.nextId = 1;
    this.stopped = false;
  }

  connect() {
    if (this.stopped) return;
    // Re-read every attempt: port and token change when AVcompanion restarts.
    const d = this.readDiscovery();
    if (!d) return this.retry();
    let ws;
    try {
      ws = new this.WS(`ws://127.0.0.1:${d.port}`);
    } catch (_) {
      return this.retry();
    }
    this.ws = ws;
    ws.onopen = () => {
      ws.send(JSON.stringify({ t: "hello", proto: PROTO, token: d.token, client: "streamdeck" }));
    };
    ws.onmessage = (ev) => {
      let m;
      try {
        m = JSON.parse(typeof ev.data === "string" ? ev.data : String(ev.data));
      } catch (_) {
        return;
      }
      if (m.t === "state") {
        this.ready = true;
        this.backoff = BACKOFF_MIN_MS;
        this.onState(m);
      } else if (m.t === "ack") {
        this.onAck(m);
      }
    };
    ws.onerror = () => {};
    ws.onclose = () => {
      if (this.ws !== ws) return;
      this.ws = null;
      const wasReady = this.ready;
      this.ready = false;
      if (wasReady) this.onState(null);
      this.retry();
    };
  }

  retry() {
    if (this.stopped) return;
    const wait = this.backoff;
    this.backoff = Math.min(this.backoff * 2, BACKOFF_MAX_MS);
    this.setTimeout(() => this.connect(), wait);
  }

  /** Returns the command id, or 0 when the app is not connected (the caller shows an alert). */
  cmd(name, extra) {
    if (!this.ready || !this.ws) return 0;
    const id = this.nextId++;
    this.ws.send(JSON.stringify({ t: "cmd", id, name, ...extra }));
    return id;
  }

  stop() {
    this.stopped = true;
    if (this.ws) this.ws.close();
  }
}

// ----------------------------------------------------------------- plugin

class Plugin {
  /**
   * sdSend(obj) sends one JSON message to the Stream Deck app.
   * daemon is a DaemonLink (or a test double with cmd()).
   */
  constructor(sdSend, daemon, timers = { setTimeout, clearTimeout }) {
    this.sd = sdSend;
    this.daemon = daemon;
    this.timers = timers;
    this.contexts = new Map(); // context -> action UUID
    this.drawn = new Map(); // context -> last payload sent, as JSON
    this.state = null; // last daemon state frame, or null when offline
    this.pending = { coarse: 0, fine: 0, timer: null };
    this.waiting = new Map(); // command id -> context that should show the result
  }

  /** Handle one event from the Stream Deck app. */
  onSdEvent(m) {
    switch (m.event) {
      case "willAppear":
        this.contexts.set(m.context, m.action);
        this.drawn.delete(m.context); // a key that just appeared shows its manifest default
        this.draw(m.context, m.action);
        break;
      case "willDisappear":
        this.contexts.delete(m.context);
        this.drawn.delete(m.context);
        break;
      case "keyUp":
        if (m.action === ACTION_MUTE) this.send(m.context, "mute.toggle");
        else if (m.action === ACTION_ZOOM) this.zoomShare(m.context, (m.payload && m.payload.settings) || {});
        break;
      case "dialDown":
        if (m.action === ACTION_GAIN) this.send(m.context, "mute.toggle");
        break;
      case "touchTap":
        if (m.action === ACTION_GAIN) this.send(m.context, "gain.set", { cdb: 0 });
        break;
      case "dialRotate":
        if (m.action === ACTION_GAIN) this.rotate(m.context, m.payload || {});
        break;
      default:
        break;
    }
  }

  send(context, name, extra) {
    const id = this.daemon.cmd(name, extra);
    if (!id) this.sd({ event: "showAlert", context });
    return id;
  }

  /** The Zoom Share key. What it does is chosen in the key's settings. */
  zoomShare(context, settings) {
    const mode = settings.mode || "open";
    let id;
    if (mode === "window") {
      id = this.send(context, "zoom.share.window", {
        match: String(settings.match || ""),
        sound: settings.sound !== false, // sound is shared unless the key's settings turn it off
        optimize: settings.optimize === true,
      });
    } else if (mode === "pause") {
      id = this.send(context, "zoom.share.pause");
    } else if (mode === "toggle") {
      id = this.send(context, "zoom.share.toggle");
    } else {
      id = this.send(context, "zoom.share.open");
    }
    // Zoom commands can fail (not in a meeting, no matching window): show the outcome on the key.
    if (id) this.waiting.set(id, context);
  }

  /** Called with every command result from the app. */
  onDaemonAck(ack) {
    const context = this.waiting.get(ack.id);
    if (context === undefined) return;
    this.waiting.delete(ack.id);
    this.sd({ event: ack.ok ? "showOk" : "showAlert", context });
  }

  /** Sum ticks over a short window so a fast spin becomes one command. */
  rotate(context, payload) {
    const ticks = Number(payload.ticks) || 0;
    if (payload.pressed) this.pending.fine += ticks;
    else this.pending.coarse += ticks;
    if (this.pending.timer) return;
    this.pending.timer = this.timers.setTimeout(() => {
      const { coarse, fine } = this.pending;
      this.pending = { coarse: 0, fine: 0, timer: null };
      if (coarse) this.send(context, "gain.nudge", { ticks: coarse });
      if (fine) this.send(context, "gain.nudge", { ticks: fine, fine: true });
    }, DIAL_COALESCE_MS);
  }

  /** Called with every daemon state frame, and with null when the link drops. */
  onDaemonState(state) {
    this.state = state;
    for (const [context, action] of this.contexts) this.draw(context, action);
  }

  /** Send `msg` unless this context already shows exactly that. A dial spin
   *  changes gain many times a second; the mute key should not be redrawn for each. */
  paint(context, signature, msg) {
    if (this.drawn.get(context) === signature) return;
    this.drawn.set(context, signature);
    this.sd(msg);
  }

  draw(context, action) {
    const s = this.state;
    if (action === ACTION_MUTE) {
      const key = s ? (KEY_IMG[s.effective] ? s.effective : "unverified") : "offline";
      this.paint(context, key, { event: "setImage", context, payload: { image: KEY_IMG[key], target: 0 } });
    } else if (action === ACTION_GAIN) {
      const payload = s
        ? {
            title: s.effective === "live" ? "Mic gain" : "MUTED",
            value: gainLabel(s.gain_cdb),
            indicator: { value: gainPct(s.gain_cdb, s.gain_min_cdb, s.gain_max_cdb) },
          }
        : { title: "App offline", value: "--", indicator: { value: 0 } };
      this.paint(context, JSON.stringify(payload), { event: "setFeedback", context, payload });
    }
  }
}

// ------------------------------------------------------------------- main

function main() {
  const args = parseArgs(process.argv.slice(2));
  if (!args.port || !args.pluginUUID || !args.registerEvent) {
    console.error("AVcompanion plugin: must be launched by the Stream Deck app.");
    process.exit(1);
  }
  const sd = new WebSocket(`ws://127.0.0.1:${args.port}`);
  const sdSend = (obj) => {
    if (sd.readyState === 1) sd.send(JSON.stringify(obj));
  };
  const daemon = new DaemonLink({
    onState: (s) => plugin.onDaemonState(s),
    onAck: (a) => plugin.onDaemonAck(a),
  });
  const plugin = new Plugin(sdSend, daemon);

  sd.onopen = () => {
    sd.send(JSON.stringify({ event: args.registerEvent, uuid: args.pluginUUID }));
    daemon.connect();
  };
  sd.onmessage = (ev) => {
    try {
      plugin.onSdEvent(JSON.parse(String(ev.data)));
    } catch (_) {
      /* ignore malformed frames */
    }
  };
  // Stream Deck closing its socket means the plugin should exit.
  sd.onclose = () => process.exit(0);
  sd.onerror = () => {};
}

if (require.main === module) main();

module.exports = {
  ACTION_MUTE,
  ACTION_GAIN,
  ACTION_ZOOM,
  KEY_IMG,
  DaemonLink,
  Plugin,
  discoveryPath,
  gainLabel,
  gainPct,
  parseArgs,
  readDiscovery,
};
