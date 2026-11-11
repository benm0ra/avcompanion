# Security

## Reporting a problem

Please report vulnerabilities privately through GitHub's "Report a vulnerability" button on the Security tab of this repository, not in a public issue.

## What the app does with your audio and your machine

- Audio goes from your microphone to the virtual cable on the same computer. It is never recorded, stored or sent anywhere.
- The app makes no outbound network connections and has no telemetry.
- The only listening socket is the control daemon on `127.0.0.1` (ports 8989-8999). It is not reachable from other machines.

## Control daemon threat model

The daemon lets a local client change mute and gain. It carries no audio.

| Threat | Mitigation |
| --- | --- |
| A web page in your browser connecting to `ws://127.0.0.1:8989` | Any WebSocket upgrade that carries an `Origin` header is rejected. Browsers always send one |
| Another local program connecting | The first frame must contain a random 128-bit token, regenerated on every app start and stored in `daemon.json` in your user profile (file mode 0600 on macOS and Linux) |
| Token guessing | Constant-time comparison; the connection is closed on a wrong or late `hello` |
| Resource exhaustion by a local client | 16 connections maximum, 64 KiB message limit, 5 s handshake timeout, 2 s to authenticate |
| The app's own web view being abused | Content Security Policy restricts it to bundled files; it has no shell, file-system or network permissions and never opens the daemon socket |

Out of scope: a program already running as your user account can read `daemon.json` and control mute and gain. Such a program can already do far more than that, so the token is not designed to stop it.

## Known limitations

- Release builds are not yet code-signed, so Windows SmartScreen will warn on install.
- Dependency lockfiles (`Cargo.lock`, `package-lock.json`) should be committed after the first successful build so that builds are reproducible.
- `daemon.json` is left in place when the app exits. Its token is useless once the app has stopped.
