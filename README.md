# AVcompanion

Desktop companion for your microphone in Zoom, with a Stream Deck plugin for a mute key and a gain dial. Download the installer from the latest successful run under Actions (artifact `AVcompanion-windows-msi`).

Two modes:

| Mode | What it does | Needs |
| --- | --- | --- |
| **Direct** (default) | Sets your real microphone's level and mute in Windows. Zoom keeps using the microphone as usual. No added delay | Nothing else |
| **Full processing** | mic -> gain / click-free mute / soft clip -> virtual cable -> Zoom | A virtual audio cable. The installer carries VB-CABLE's setup and offers it in the app |

The Zoom bridge (reading and mirroring Zoom's own mute) is not in this build: the key shows the app's own mute state.

## What has and has not been verified

| Part | Status |
| --- | --- |
| `src-tauri/crates/av-core` (ring, drift correction, gain ramp, soft clip, mute) | Compiled and tested: 9 tests pass, including 10-minute simulated runs at up to +/-300 ppm clock drift with zero dropouts |
| Stream Deck plugin (`plugin/`) | 8 unit tests pass; the real plugin process was also run end to end against stand-in Stream Deck and daemon servers |
| Tauri app (routing, direct mode, WebSocket daemon, hotkeys, UI) | Compiles without warnings in CI and builds into the Windows installer. Not yet run on a real machine |
| `manifest.json` | Passes `streamdeck validate` in CI |
| Real audio, real Stream Deck hardware, installing the MSI, the VB-CABLE setup button | Not tested |

## Using it

Direct mode: pick your microphone and press Start. In Zoom's audio settings turn off "Automatically adjust microphone volume", or Zoom will move the level back.

Full processing: choose that mode, install VB-CABLE from the button if it is missing, set the cable and your microphone to 48000 Hz in Windows Sound settings, pick "CABLE Input" as the destination and press Start. In Zoom choose "CABLE Output" as the microphone and set the microphone mode to "Original sound for musicians".

When the app stops or exits in direct mode it puts the microphone's mute back the way it found it.

## Building it yourself (Windows 10/11)

Not needed to use the app; CI builds the installer.

1. Rust (stable, MSVC toolchain): https://rustup.rs
2. Visual Studio Build Tools with "Desktop development with C++"
3. Node.js 22 or newer (includes npm)
4. For the plugin: Stream Deck app 7.1 or newer

## First run

```
npm install
npm test            # audio core + plugin tests
npm run dev         # builds and launches the app
```

Hotkeys: `Ctrl+Shift+M` toggles mute. Hold `Ctrl+Alt+Space` to talk (muted again on release).

## Stream Deck plugin

```
npm run plugin:validate
npm run plugin:link     # links the plugin folder into Stream Deck for development
```

Restart Stream Deck, then drag "Mic Mute" onto a key and "Mic Gain" onto a dial (Stream Deck +).
Dial: turn = 0.5 dB steps, turn while pressed = 0.1 dB steps, press = mute, tap screen = 0 dB.

The plugin finds the app through `%APPDATA%\AVcompanion\daemon.json` (port and access token), which the app rewrites on every start.

## Release build

```
npm run release
```

Produces `plugin/dist/com.avcompanion.control.streamDeckPlugin` and the MSI under `src-tauri/target/release/bundle/msi/`. Neither is code-signed yet.

## Layout

```
src-tauri/crates/av-core/   real-time audio core, no OS dependencies
src-tauri/src/audio.rs      device listing, stream supervisor thread (cpal)
src-tauri/src/daemon.rs     WebSocket daemon on 127.0.0.1:8989-8999, protocol v1
src-tauri/src/main.rs       Tauri setup, IPC commands, global hotkeys
ui/                         static front end (no bundler)
plugin/                     Stream Deck plugin (plain Node.js, no dependencies) and tests
scripts/release.mjs         one-command build of both artifacts
```

## Known gaps

- Direct mode is Windows-only so far, and its range and step size are whatever the microphone's driver offers. Some microphones move in coarse steps, so fine dial steps may do nothing.
- Direct mode has no soft clipper and no fade; mute is the operating system's own.
- Full processing needs devices already at 48 kHz; there is no resampler fallback yet.
- If a device is unplugged the app reports "Device lost" and waits for Start; it does not reconnect on its own yet.
- Latency is unmeasured. With default Windows shared-mode buffers expect roughly 30-40 ms added.
- The installer does not yet embed the Stream Deck plugin.

## License and third-party software

AVcompanion is released under the MIT License (see `LICENSE`). Its Rust and npm dependencies are under MIT and/or Apache-2.0 licenses.

VB-CABLE Virtual Audio Device is a product of VB-Audio Software (www.vb-cable.com). VB-CABLE is a donationware, all participations are welcome. This source repository does not contain it. Release builds download VB-Audio's driver pack, check it against a pinned SHA-256, and include it unmodified so the app can open VB-Audio's own setup when you ask for it. It is never installed silently. If you redistribute AVcompanion inside an organisation whose users cannot see or support VB-CABLE themselves, VB-Audio's licensing terms require you to buy licences from them.

BlackHole (Existential Audio) is a separate product that macOS users install themselves under its own terms.

AVcompanion is an independent project. It is not affiliated with, endorsed by or sponsored by Zoom Video Communications, Elgato / Corsair, VB-Audio or Existential Audio. "Zoom", "Stream Deck", "Elgato", "VB-Cable" and "BlackHole" are trademarks of their respective owners and are used here only to describe compatibility.

Security policy and threat model: see `SECURITY.md`.
