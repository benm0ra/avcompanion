"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const path = require("node:path");
const P = require("../com.avcompanion.control.sdPlugin/bin/plugin.js");

class FakeWS {
  constructor(url) {
    this.url = url;
    this.sent = [];
    FakeWS.last = this;
  }
  send(s) {
    this.sent.push(JSON.parse(s));
  }
  close() {
    if (this.onclose) this.onclose();
  }
}

function fakeTimers() {
  const q = [];
  return {
    q,
    setTimeout: (fn, ms) => {
      q.push({ fn, ms });
      return q.length;
    },
    clearTimeout: () => {},
    runAll() {
      while (q.length) q.shift().fn();
    },
  };
}

const STATE = { t: "state", seq: 1, mute: false, effective: "live", gain_cdb: -600 };

test("helpers", () => {
  assert.equal(P.gainPct(-6000), 0);
  assert.equal(P.gainPct(2400), 100);
  assert.equal(P.gainPct(99999), 100);
  assert.equal(P.gainPct(0), 71);
  assert.equal(P.gainPct(600, -1200, 1200), 75, "uses the range the app reports");
  assert.equal(P.gainPct(0, 5, 5), 71, "falls back on a nonsense range");
  assert.equal(P.gainLabel(-600), "-6.0 dB");
  assert.equal(P.gainLabel(150), "+1.5 dB");
  assert.deepEqual(P.parseArgs(["-port", "123", "-pluginUUID", "abc", "-registerEvent", "reg", "-info", "{}"]), {
    port: "123",
    pluginUUID: "abc",
    registerEvent: "reg",
    info: "{}",
  });
  assert.equal(
    P.discoveryPath({ APPDATA: "C:\\Users\\x\\AppData\\Roaming" }, "win32", "C:\\Users\\x"),
    path.join("C:\\Users\\x\\AppData\\Roaming", "AVcompanion", "daemon.json")
  );
  assert.ok(P.discoveryPath({}, "darwin", "/Users/x").endsWith("Library/Application Support/AVcompanion/daemon.json"));
});

test("daemon link: hello with token, state marks ready, commands carry ids", () => {
  const states = [];
  const t = fakeTimers();
  const link = new P.DaemonLink({
    WebSocketImpl: FakeWS,
    readDiscovery: () => ({ port: 8990, token: "tok" }),
    setTimeout: t.setTimeout,
    onState: (s) => states.push(s),
  });
  link.connect();
  const ws = FakeWS.last;
  assert.equal(ws.url, "ws://127.0.0.1:8990");
  assert.equal(link.cmd("mute.toggle"), false, "no commands before the daemon answers");
  ws.onopen();
  assert.deepEqual(ws.sent[0], { t: "hello", proto: 1, token: "tok", client: "streamdeck" });
  ws.onmessage({ data: JSON.stringify(STATE) });
  assert.equal(states.length, 1);
  assert.equal(link.cmd("gain.nudge", { ticks: 2 }), true);
  assert.deepEqual(ws.sent[1], { t: "cmd", id: 1, name: "gain.nudge", ticks: 2 });
  link.cmd("mute.toggle");
  assert.equal(ws.sent[2].id, 2);
});

test("daemon link: reconnects with backoff and re-reads discovery", () => {
  const t = fakeTimers();
  let port = null;
  const states = [];
  const link = new P.DaemonLink({
    WebSocketImpl: FakeWS,
    readDiscovery: () => (port ? { port, token: "t" } : null),
    setTimeout: t.setTimeout,
    onState: (s) => states.push(s),
  });
  FakeWS.last = null;
  link.connect(); // app not running
  assert.equal(FakeWS.last, null);
  assert.equal(t.q[0].ms, 250);
  t.q.shift().fn(); // still not running
  assert.equal(t.q[0].ms, 500);
  port = 8989;
  t.q.shift().fn();
  assert.equal(FakeWS.last.url, "ws://127.0.0.1:8989");
  FakeWS.last.onopen();
  FakeWS.last.onmessage({ data: JSON.stringify(STATE) });
  // app quits and comes back on another port
  port = 8991;
  FakeWS.last.onclose();
  assert.equal(states.at(-1), null, "plugin is told the link dropped");
  assert.equal(link.cmd("mute.toggle"), false);
  assert.equal(t.q[0].ms, 250, "backoff resets after a good connection");
  t.q.shift().fn();
  assert.equal(FakeWS.last.url, "ws://127.0.0.1:8991");
});

function harness(connected = true) {
  const sd = [];
  const cmds = [];
  const t = fakeTimers();
  const daemon = {
    cmd: (name, extra) => {
      if (!connected) return false;
      cmds.push({ name, ...extra });
      return true;
    },
  };
  const plugin = new P.Plugin((m) => sd.push(m), daemon, t);
  return { sd, cmds, t, plugin };
}

test("plugin: key and dial draw from daemon state only", () => {
  const { sd, plugin } = harness();
  plugin.onSdEvent({ event: "willAppear", action: P.ACTION_MUTE, context: "k1" });
  plugin.onSdEvent({ event: "willAppear", action: P.ACTION_GAIN, context: "d1" });
  assert.equal(sd[0].payload.image, P.KEY_IMG.offline, "offline before any state");
  assert.equal(sd[1].payload.title, "App offline");
  sd.length = 0;

  plugin.onDaemonState(STATE);
  assert.deepEqual(sd[0], { event: "setImage", context: "k1", payload: { image: P.KEY_IMG.live, target: 0 } });
  assert.deepEqual(sd[1], {
    event: "setFeedback",
    context: "d1",
    payload: { title: "Mic gain", value: "-6.0 dB", indicator: { value: 64 } },
  });
  sd.length = 0;

  plugin.onDaemonState({ ...STATE, mute: true, effective: "muted" });
  assert.equal(sd[0].payload.image, P.KEY_IMG.muted);
  assert.equal(sd[1].payload.title, "MUTED");
  sd.length = 0;

  plugin.onDaemonState({ ...STATE, effective: "zoom_muted" });
  assert.equal(sd[0].payload.image, P.KEY_IMG.zoom_muted);
  plugin.onDaemonState({ ...STATE, effective: "something-new" });
  assert.equal(sd.at(-2).payload.image, P.KEY_IMG.unverified, "unknown values fall back to unverified");

  plugin.onSdEvent({ event: "willDisappear", action: P.ACTION_MUTE, context: "k1" });
  sd.length = 0;
  plugin.onDaemonState(null);
  assert.equal(sd.length, 1, "only the dial is still visible");
  assert.equal(sd[0].payload.title, "App offline");
});

test("plugin: key press sends a command and never changes the key itself", () => {
  const { sd, cmds, plugin } = harness();
  plugin.onSdEvent({ event: "willAppear", action: P.ACTION_MUTE, context: "k1" });
  sd.length = 0;
  plugin.onSdEvent({ event: "keyUp", action: P.ACTION_MUTE, context: "k1" });
  assert.deepEqual(cmds, [{ name: "mute.toggle" }]);
  assert.equal(sd.length, 0);
});

test("plugin: alert when the app is offline", () => {
  const { sd, plugin } = harness(false);
  plugin.onSdEvent({ event: "keyUp", action: P.ACTION_MUTE, context: "k1" });
  assert.deepEqual(sd, [{ event: "showAlert", context: "k1" }]);
});

test("plugin: dial ticks are coalesced, fine and coarse kept apart", () => {
  const { cmds, t, plugin } = harness();
  for (const ticks of [1, 2, -1, 3]) {
    plugin.onSdEvent({ event: "dialRotate", action: P.ACTION_GAIN, context: "d1", payload: { ticks, pressed: false } });
  }
  plugin.onSdEvent({ event: "dialRotate", action: P.ACTION_GAIN, context: "d1", payload: { ticks: -2, pressed: true } });
  assert.equal(cmds.length, 0, "nothing sent inside the window");
  assert.equal(t.q.length, 1, "one timer for the whole burst");
  assert.equal(t.q[0].ms, 15);
  t.runAll();
  assert.deepEqual(cmds, [
    { name: "gain.nudge", ticks: 5 },
    { name: "gain.nudge", ticks: -2, fine: true },
  ]);

  plugin.onSdEvent({ event: "dialRotate", action: P.ACTION_GAIN, context: "d1", payload: { ticks: 1 } });
  plugin.onSdEvent({ event: "dialRotate", action: P.ACTION_GAIN, context: "d1", payload: { ticks: -1 } });
  t.runAll();
  assert.equal(cmds.length, 2, "a burst that nets to zero sends nothing");
});

test("plugin: dial press mutes, touch resets gain", () => {
  const { cmds, plugin } = harness();
  plugin.onSdEvent({ event: "dialDown", action: P.ACTION_GAIN, context: "d1" });
  plugin.onSdEvent({ event: "touchTap", action: P.ACTION_GAIN, context: "d1" });
  assert.deepEqual(cmds, [{ name: "mute.toggle" }, { name: "gain.set", cdb: 0 }]);
});
