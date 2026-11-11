# AVcompanion

Desktop companion that owns your microphone path into Zoom: mic -> gain / mute / soft clip -> virtual cable -> Zoom, with a Stream Deck plugin for a mute key and a gain dial.

This is the Phase 1 + 2 build from the architecture plan. The Zoom bridge (reading and mirroring Zoom's own mute) is Phase 3 and is not in this build: the key shows the app's own mute state.

## What has and has not been verified

| Part | Status |
| --- | --- |
| `src-tauri/crates/av-core` (ring, drift correction, gain ramp, soft clip, mute) | Compiled and tested: 9 tests pass, including 10-minute simulated runs at up to +/-300 ppm clock drift with zero dropouts |
| Stream Deck plugin (`plugin/`) | 8 unit tests pass; the real plugin process was also run end to end against stand-in Stream Deck and daemon servers |
| Tauri app (`src-tauri/src/*.rs`: cpal routing, WebSocket daemon, hotkeys) and UI | Written and syntax-checked only. It was not compiled, because its dependencies could not be downloaded where it was written. Expect to fix a few compile errors on first build |
| `manifest.json` | Valid JSON; not yet run through `streamdeck validate` |
| Real audio, real Stream Deck hardware, MSI build | Not tested |

## Prerequisites (Windows 10/11)

1. Rust (stable, MSVC toolchain): https://rustup.rs
2. Visual Studio Build Tools with "Desktop development with C++"
3. Node.js 22 or newer (includes npm)
4. VB-Cable virtual audio driver, with the cable and your microphone both set to 48000 Hz in Windows Sound settings
5. For the plugin: Stream Deck app 7.1 or newer

## First run

```
npm install
npm test            # audio core + plugin tests
npm run dev         # builds and launches the app
```

In the app: pick your microphone, pick "CABLE Input (VB-Audio Virtual Cable)" as the destination, press Start.
In Zoom: choose "CABLE Output (VB-Audio Virtual Cable)" as the microphone, set the microphone mode to "Original sound for musicians", and turn off "Automatically adjust microphone volume".

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

- Devices must already be at 48 kHz; there is no resampler fallback yet.
- If a device is unplugged the app reports "Device lost" and waits for Start; it does not reconnect on its own yet.
- Latency is unmeasured. With default Windows shared-mode buffers expect roughly 30-40 ms added.
- The installer does not yet embed the plugin or check for VB-Cable.

## License and third-party software

AVcompanion is released under the MIT License (see `LICENSE`). Its Rust and npm dependencies are under MIT and/or Apache-2.0 licenses.

This repository contains no third-party audio driver. VB-Cable (VB-Audio) and BlackHole (Existential Audio) are separate products that you install yourself under their own terms.

AVcompanion is an independent project. It is not affiliated with, endorsed by or sponsored by Zoom Video Communications, Elgato / Corsair, VB-Audio or Existential Audio. "Zoom", "Stream Deck", "Elgato", "VB-Cable" and "BlackHole" are trademarks of their respective owners and are used here only to describe compatibility.

Security policy and threat model: see `SECURITY.md`.
