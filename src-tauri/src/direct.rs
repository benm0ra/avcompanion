//! Direct mode: no virtual audio device. The app is not in the audio path;
//! it sets the real microphone endpoint's level and mute in the operating
//! system, and follows any change made elsewhere (Windows mixer, another app).
//!
//! COM objects are bound to the thread that created them, so one thread owns
//! the endpoint and everything else talks to it over a channel.

use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};

use crate::daemon::Shared;

enum Cmd {
    Attach {
        name: String,
        reply: mpsc::Sender<Result<(), String>>,
    },
    Detach {
        reply: mpsc::Sender<()>,
    },
}

pub struct DirectHandle {
    tx: mpsc::Sender<Cmd>,
}

impl DirectHandle {
    /// Take control of the named microphone. Adopts its current level and mute.
    pub fn attach(&self, name: String) -> Result<(), String> {
        let (reply, rx) = mpsc::channel();
        self.tx
            .send(Cmd::Attach { name, reply })
            .map_err(|_| "direct-mode thread is not running".to_string())?;
        rx.recv()
            .map_err(|_| "direct-mode thread did not answer".to_string())?
    }

    /// Release the microphone, restoring the mute state it had when attached.
    pub fn detach(&self) {
        let (reply, rx) = mpsc::channel();
        if self.tx.send(Cmd::Detach { reply }).is_ok() {
            let _ = rx.recv();
        }
    }
}

pub fn spawn(shared: Arc<Shared>, lost: Arc<AtomicBool>) -> DirectHandle {
    let (tx, rx) = mpsc::channel::<Cmd>();
    std::thread::Builder::new()
        .name("direct-control".into())
        .spawn(move || imp::run(rx, shared, lost))
        .expect("failed to start direct-control thread");
    DirectHandle { tx }
}

#[cfg(windows)]
mod imp {
    use super::Cmd;
    use crate::daemon::Shared;
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    use windows::core::Result as WinResult;
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::Foundation::BOOL;
    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
    use windows::Win32::Media::Audio::{
        eCapture, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
    };

    /// How often app-side changes are written to the endpoint.
    const TICK: Duration = Duration::from_millis(25);
    /// The endpoint is read back (to notice changes made elsewhere) every
    /// this many ticks while nothing is being written: 10 times a second.
    const READ_EVERY: u32 = 4;

    struct Endpoint {
        vol: IAudioEndpointVolume,
        min_cdb: i32,
        max_cdb: i32,
        applied_gain: i32,
        applied_mute: bool,
        original_mute: bool,
        idle_ticks: u32,
    }

    impl Endpoint {
        fn open(name: &str) -> Result<Self, String> {
            let found = unsafe { find(name) }.map_err(|e| format!("Windows audio error: {e}"))?;
            let vol = found.ok_or_else(|| format!("microphone \"{name}\" not found"))?;
            let (mut min, mut max, mut step) = (0f32, 0f32, 0f32);
            unsafe { vol.GetVolumeRange(&mut min, &mut max, &mut step) }
                .map_err(|e| format!("could not read the level range of \"{name}\": {e}"))?;
            let mut ep = Self {
                vol,
                min_cdb: (min * 100.0).ceil() as i32,
                max_cdb: (max * 100.0).floor() as i32,
                applied_gain: 0,
                applied_mute: false,
                original_mute: false,
                idle_ticks: 0,
            };
            ep.applied_gain = ep.read_gain().map_err(|e| e.to_string())?;
            ep.applied_mute = ep.read_mute().map_err(|e| e.to_string())?;
            ep.original_mute = ep.applied_mute;
            Ok(ep)
        }

        fn read_gain(&self) -> WinResult<i32> {
            Ok((unsafe { self.vol.GetMasterVolumeLevel() }? * 100.0).round() as i32)
        }
        fn read_mute(&self) -> WinResult<bool> {
            Ok(unsafe { self.vol.GetMute() }?.as_bool())
        }

        /// Push app-side changes to the device, or pull device-side changes
        /// into the app. Returns true when shared state changed.
        fn sync(&mut self, shared: &Shared) -> WinResult<bool> {
            let ctl = &shared.ctl;
            let mut changed = false;

            let asked = ctl.gain_cdb.load(Relaxed);
            let want = asked.clamp(self.min_cdb, self.max_cdb);
            let want_mute = ctl.mute.load(Relaxed);

            // Nothing to write: only poll the device every few ticks.
            if want == self.applied_gain && want_mute == self.applied_mute {
                self.idle_ticks += 1;
                if self.idle_ticks % READ_EVERY != 0 {
                    return Ok(false);
                }
            } else {
                self.idle_ticks = 0;
            }

            if want != self.applied_gain {
                unsafe {
                    self.vol
                        .SetMasterVolumeLevel(want as f32 / 100.0, std::ptr::null())
                }?;
                // Devices round to their own step size; report what was actually set.
                self.applied_gain = self.read_gain()?;
            } else {
                let dev = self.read_gain()?;
                if dev != self.applied_gain {
                    self.applied_gain = dev; // changed outside the app: follow it
                }
            }
            if asked != self.applied_gain {
                ctl.gain_cdb.store(self.applied_gain, Relaxed);
                changed = true;
            }

            if want_mute != self.applied_mute {
                unsafe { self.vol.SetMute(BOOL::from(want_mute), std::ptr::null()) }?;
                self.applied_mute = want_mute;
            } else {
                let dev = self.read_mute()?;
                if dev != self.applied_mute {
                    self.applied_mute = dev;
                    ctl.mute.store(dev, Relaxed);
                    changed = true;
                }
            }
            Ok(changed)
        }

        /// Do not leave the user's microphone muted system-wide after the app lets go.
        fn restore(&self) {
            unsafe {
                let _ = self
                    .vol
                    .SetMute(BOOL::from(self.original_mute), std::ptr::null());
            }
        }
    }

    /// Find the active capture endpoint whose friendly name matches the name
    /// shown in the device list (cpal reports the same property).
    unsafe fn find(name: &str) -> WinResult<Option<IAudioEndpointVolume>> {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let devices = enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)?;
        for i in 0..devices.GetCount()? {
            let device = devices.Item(i)?;
            let store = device.OpenPropertyStore(STGM_READ)?;
            let friendly = store.GetValue(&PKEY_Device_FriendlyName)?.to_string();
            if friendly == name {
                return Ok(Some(
                    device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)?,
                ));
            }
        }
        Ok(None)
    }

    pub fn run(rx: mpsc::Receiver<Cmd>, shared: Arc<Shared>, lost: Arc<AtomicBool>) {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let mut endpoint: Option<Endpoint> = None;
        loop {
            match rx.recv_timeout(TICK) {
                Ok(Cmd::Attach { name, reply }) => {
                    if let Some(old) = endpoint.take() {
                        old.restore();
                    }
                    match Endpoint::open(&name) {
                        Ok(ep) => {
                            // Adopt the microphone as it is; never change it just by attaching.
                            shared.ctl.set_gain_range(ep.min_cdb, ep.max_cdb);
                            shared.ctl.gain_cdb.store(ep.applied_gain, Relaxed);
                            shared.ctl.mute.store(ep.applied_mute, Relaxed);
                            endpoint = Some(ep);
                            shared.bump();
                            let _ = reply.send(Ok(()));
                        }
                        Err(e) => {
                            let _ = reply.send(Err(e));
                        }
                    }
                }
                Ok(Cmd::Detach { reply }) => {
                    if let Some(old) = endpoint.take() {
                        old.restore();
                    }
                    let _ = reply.send(());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if let Some(ep) = endpoint.as_mut() {
                match ep.sync(&shared) {
                    Ok(true) => shared.bump(),
                    Ok(false) => {}
                    Err(_) => {
                        // Device unplugged or disabled.
                        endpoint = None;
                        lost.store(true, Relaxed);
                        shared.bump();
                    }
                }
            }
        }
        if let Some(old) = endpoint.take() {
            old.restore();
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Cmd;
    use crate::daemon::Shared;
    use std::sync::atomic::AtomicBool;
    use std::sync::{mpsc, Arc};

    /// Direct mode needs a per-OS implementation; macOS (CoreAudio) is not written yet.
    pub fn run(rx: mpsc::Receiver<Cmd>, _shared: Arc<Shared>, _lost: Arc<AtomicBool>) {
        while let Ok(cmd) = rx.recv() {
            match cmd {
                Cmd::Attach { reply, .. } => {
                    let _ = reply.send(Err(
                        "Direct mode is only available on Windows in this build. Use full processing mode."
                            .to_string(),
                    ));
                }
                Cmd::Detach { reply } => {
                    let _ = reply.send(());
                }
            }
        }
    }
}
