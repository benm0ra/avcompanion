// Hide the console window in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod daemon;
mod direct;
mod vbcable;

use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

use av_core::{ControlState, GAIN_MAX_CDB, GAIN_MIN_CDB};
use serde_json::{json, Value};
use tauri::Manager;
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

struct AppState {
    shared: Arc<daemon::Shared>,
    audio: audio::AudioHandle,
    direct: direct::DirectHandle,
}

impl AppState {
    /// Stop metering/routing and hand the microphone back.
    fn stop_all(&self) {
        self.audio.stop();
        self.direct.detach();
        self.shared.bump();
    }
}

/// Ctrl+Shift+M toggles mute.
fn sc_toggle() -> Shortcut {
    Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyM)
}
/// Ctrl+Alt+Space is push-to-talk: live while held, muted on release.
fn sc_ptt() -> Shortcut {
    Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Space)
}

#[tauri::command(async)]
fn list_devices() -> Result<audio::DeviceList, String> {
    audio::list_devices()
}

/// `mode` is "direct" (control the real microphone, nothing else to install)
/// or "cable" (full processing through a virtual cable).
#[tauri::command(async)]
fn start_audio(
    mode: String,
    input: String,
    output: Option<String>,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    state.audio.stop();
    state.direct.detach();
    let result = match mode.as_str() {
        "direct" => {
            state.shared.direct.store(true, Relaxed);
            state
                .direct
                .attach(input.clone())
                .and_then(|()| state.audio.start(input, None))
                .inspect_err(|_| state.direct.detach())
        }
        "cable" => match output {
            Some(output) => {
                state.shared.direct.store(false, Relaxed);
                state.shared.gain_min.store(GAIN_MIN_CDB, Relaxed);
                state.shared.gain_max.store(GAIN_MAX_CDB, Relaxed);
                // The app is now the only thing between mic and Zoom: start from unity, unmuted.
                state.shared.ctl.set_gain_cdb(0);
                state.shared.ctl.mute.store(false, Relaxed);
                state.audio.start(input, Some(output))
            }
            None => Err("choose the virtual cable to send audio to".to_string()),
        },
        other => Err(format!("unknown mode \"{other}\"")),
    };
    state.shared.bump();
    result
}

#[tauri::command(async)]
fn stop_audio(state: tauri::State<'_, AppState>) {
    state.stop_all();
}

#[tauri::command]
fn set_gain(cdb: i32, state: tauri::State<'_, AppState>) -> i32 {
    let c = state.shared.ctl.set_gain_cdb(cdb);
    state.shared.bump();
    c
}

#[tauri::command]
fn set_mute(on: bool, state: tauri::State<'_, AppState>) {
    state.shared.ctl.mute.store(on, Relaxed);
    state.shared.bump();
}

#[tauri::command]
fn get_state(state: tauri::State<'_, AppState>) -> Value {
    let s = &state.shared;
    let route = state
        .audio
        .status
        .lock()
        .map(|r| r.clone())
        .unwrap_or_default();
    json!({
        "mode": if s.direct.load(Relaxed) { "direct" } else { "cable" },
        "mute": s.ctl.mute.load(Relaxed),
        "gain_cdb": s.ctl.gain_cdb.load(Relaxed),
        "gain_min_cdb": s.gain_min.load(Relaxed),
        "gain_max_cdb": s.gain_max.load(Relaxed),
        "peak_dbfs": s.meter_cdb.load(Relaxed) as f64 / 100.0,
        "clip": s.clip.load(Relaxed),
        "xruns": s.ctl.xruns.load(Relaxed),
        "route": route,
        "lost": state.audio.lost.load(Relaxed),
        "daemon_port": s.port.load(Relaxed),
    })
}

/// Whether a virtual cable is installed, and whether this build carries VB-CABLE's setup.
#[tauri::command(async)]
fn vbcable_status(app: tauri::AppHandle) -> Value {
    let bundled = app
        .path()
        .resource_dir()
        .ok()
        .and_then(|d| vbcable::bundled_setup(&d))
        .is_some();
    json!({ "installed": audio::cable_present(), "bundled": bundled })
}

/// Open VB-Audio's own setup program. The user confirms the Windows administrator prompt.
#[tauri::command(async)]
fn install_vbcable(app: tauri::AppHandle) -> Result<(), String> {
    let dir = app.path().resource_dir().map_err(|e| e.to_string())?;
    let exe = vbcable::bundled_setup(&dir).ok_or(
        "This build does not include the VB-CABLE setup. Download it from www.vb-cable.com.",
    )?;
    vbcable::launch_setup(&exe)
}

fn main() {
    let ctl = Arc::new(ControlState::default());
    let shared = daemon::Shared::new(ctl.clone());
    let audio = audio::spawn_supervisor(ctl);
    let direct = direct::spawn(shared.clone(), audio.lost.clone());

    let lost = audio.lost.clone();
    let status = audio.status.clone();
    let setup_shared = shared.clone();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.set_focus();
            }
        }))
        .manage(AppState {
            shared,
            audio,
            direct,
        })
        .setup(move |app| {
            let sh = setup_shared.clone();
            app.handle().plugin(
                tauri_plugin_global_shortcut::Builder::new()
                    .with_handler(move |_app, shortcut, event| {
                        if shortcut == &sc_toggle() {
                            if event.state() == ShortcutState::Pressed {
                                sh.ctl.toggle_mute();
                                sh.bump();
                            }
                        } else if shortcut == &sc_ptt() {
                            let held = event.state() == ShortcutState::Pressed;
                            sh.ctl.mute.store(!held, Relaxed);
                            sh.bump();
                        }
                    })
                    .build(),
            )?;
            // A shortcut already taken by another app must not stop the app from starting.
            for sc in [sc_toggle(), sc_ptt()] {
                if let Err(e) = app.global_shortcut().register(sc) {
                    eprintln!("hotkey not registered: {e}");
                }
            }

            tauri::async_runtime::spawn(daemon::run(setup_shared.clone()));
            tauri::async_runtime::spawn(daemon::poller(setup_shared.clone(), lost, move || {
                status.lock().map(|s| s.running).unwrap_or(false)
            }));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_devices,
            start_audio,
            stop_audio,
            set_gain,
            set_mute,
            get_state,
            vbcable_status,
            install_vbcable
        ])
        .build(tauri::generate_context!())
        .expect("error while building AVcompanion");

    app.run(|handle, event| {
        // On exit, give the microphone back (direct mode restores its original mute state).
        if let tauri::RunEvent::Exit = event {
            handle.state::<AppState>().stop_all();
        }
    });
}
