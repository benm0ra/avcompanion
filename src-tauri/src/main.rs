// Hide the console window in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod daemon;

use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

use av_core::ControlState;
use serde_json::{json, Value};
use tauri::Manager;
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

struct AppState {
    shared: Arc<daemon::Shared>,
    audio: audio::AudioHandle,
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

#[tauri::command(async)]
fn start_audio(
    input: String,
    output: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let r = state.audio.start(input, output);
    state.shared.bump();
    r
}

#[tauri::command]
fn stop_audio(state: tauri::State<'_, AppState>) {
    state.audio.stop();
    state.shared.bump();
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
    let route = state.audio.status.lock().unwrap().clone();
    json!({
        "mute": s.ctl.mute.load(Relaxed),
        "gain_cdb": s.ctl.gain_cdb.load(Relaxed),
        "peak_dbfs": s.meter_cdb.load(Relaxed) as f64 / 100.0,
        "clip": s.clip.load(Relaxed),
        "xruns": s.ctl.xruns.load(Relaxed),
        "route": route,
        "lost": state.audio.lost.load(Relaxed),
        "daemon_port": s.port.load(Relaxed),
    })
}

fn main() {
    let ctl = Arc::new(ControlState::default());
    let shared = daemon::Shared::new(ctl.clone());
    let audio = audio::spawn_supervisor(ctl);

    let lost = audio.lost.clone();
    let status = audio.status.clone();
    let setup_shared = shared.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.set_focus();
            }
        }))
        .manage(AppState { shared, audio })
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
            get_state
        ])
        .run(tauri::generate_context!())
        .expect("error while running AVcompanion");
}
