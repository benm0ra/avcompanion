//! Audio routing: physical mic -> av-core pipeline -> virtual cable.
//!
//! `cpal::Stream` is not `Send` on every platform, so one supervisor thread
//! creates, owns and drops the streams. Everything else talks to it over a
//! channel. The audio callbacks themselves only call into `av-core`.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{mpsc, Arc, Mutex};

use av_core::{meter_block, pipeline, ControlState, SAMPLE_RATE};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Substrings that identify virtual-cable endpoints.
const VIRTUAL_CAPTURE_SIDE: [&str; 2] = ["CABLE Output", "BlackHole"];
const VIRTUAL_RENDER_SIDE: [&str; 2] = ["CABLE Input", "BlackHole"];

#[derive(Clone, Default, serde::Serialize)]
pub struct DeviceList {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub default_input: Option<String>,
    /// First output that looks like a virtual cable, if any.
    pub suggested_output: Option<String>,
}

#[derive(Clone, Default, serde::Serialize)]
pub struct RouteStatus {
    pub running: bool,
    pub input: Option<String>,
    pub output: Option<String>,
    pub error: Option<String>,
}

enum Cmd {
    Start {
        input: String,
        /// `None` is direct mode: meter the microphone only, no routing.
        output: Option<String>,
        reply: mpsc::Sender<Result<(), String>>,
    },
    Stop,
}

pub struct AudioHandle {
    tx: mpsc::Sender<Cmd>,
    pub status: Arc<Mutex<RouteStatus>>,
    /// Set from a stream error callback when a device disappears or is reconfigured.
    pub lost: Arc<AtomicBool>,
}

impl AudioHandle {
    pub fn start(&self, input: String, output: Option<String>) -> Result<(), String> {
        let (reply, rx) = mpsc::channel();
        self.tx
            .send(Cmd::Start {
                input,
                output,
                reply,
            })
            .map_err(|_| "audio supervisor is not running".to_string())?;
        rx.recv()
            .map_err(|_| "audio supervisor did not answer".to_string())?
    }
    pub fn stop(&self) {
        let _ = self.tx.send(Cmd::Stop);
    }
}

pub fn list_devices() -> Result<DeviceList, String> {
    let host = cpal::default_host();
    let mut list = DeviceList::default();
    for d in host.input_devices().map_err(|e| e.to_string())? {
        if let Ok(name) = d.name() {
            // Never offer the cable's own capture side as a source: that is a feedback loop.
            if !VIRTUAL_CAPTURE_SIDE.iter().any(|v| name.contains(v)) {
                list.inputs.push(name);
            }
        }
    }
    for d in host.output_devices().map_err(|e| e.to_string())? {
        if let Ok(name) = d.name() {
            if list.suggested_output.is_none()
                && VIRTUAL_RENDER_SIDE.iter().any(|v| name.contains(v))
            {
                list.suggested_output = Some(name.clone());
            }
            list.outputs.push(name);
        }
    }
    list.default_input = host
        .default_input_device()
        .and_then(|d| d.name().ok())
        .filter(|n| list.inputs.contains(n));
    Ok(list)
}

/// True when a virtual cable's render side is present.
pub fn cable_present() -> bool {
    list_devices()
        .map(|d| d.suggested_output.is_some())
        .unwrap_or(false)
}

pub fn spawn_supervisor(ctl: Arc<ControlState>) -> AudioHandle {
    let (tx, rx) = mpsc::channel::<Cmd>();
    let status = Arc::new(Mutex::new(RouteStatus::default()));
    let lost = Arc::new(AtomicBool::new(false));
    let (st, lo) = (status.clone(), lost.clone());

    std::thread::Builder::new()
        .name("audio-supervisor".into())
        .spawn(move || {
            let mut streams: Option<(cpal::Stream, Option<cpal::Stream>)> = None;
            while let Ok(cmd) = rx.recv() {
                match cmd {
                    Cmd::Start {
                        input,
                        output,
                        reply,
                    } => {
                        // Drop the old streams before opening new ones.
                        streams = None;
                        lo.store(false, Relaxed);
                        let result = open(&input, output.as_deref(), ctl.clone(), lo.clone());
                        let mut s = st.lock().unwrap();
                        match result {
                            Ok(pair) => {
                                streams = Some(pair);
                                *s = RouteStatus {
                                    running: true,
                                    input: Some(input),
                                    output,
                                    error: None,
                                };
                                let _ = reply.send(Ok(()));
                            }
                            Err(e) => {
                                *s = RouteStatus {
                                    running: false,
                                    input: None,
                                    output: None,
                                    error: Some(e.clone()),
                                };
                                let _ = reply.send(Err(e));
                            }
                        }
                    }
                    Cmd::Stop => {
                        streams = None;
                        let mut s = st.lock().unwrap();
                        s.running = false;
                        s.error = None;
                    }
                }
            }
            drop(streams);
        })
        .expect("failed to start audio supervisor thread");

    AudioHandle { tx, status, lost }
}

fn find_device(
    mut devices: impl Iterator<Item = cpal::Device>,
    name: &str,
) -> Option<cpal::Device> {
    devices.find(|d| d.name().map(|n| n == name).unwrap_or(false))
}

fn check_format(
    name: &str,
    cfg: &cpal::SupportedStreamConfig,
    need_48k: bool,
) -> Result<(), String> {
    if need_48k && cfg.sample_rate().0 != SAMPLE_RATE {
        return Err(format!(
            "\"{name}\" is set to {} Hz. Set its default format to 48000 Hz in the system sound settings, then press Start again.",
            cfg.sample_rate().0
        ));
    }
    if cfg.sample_format() != cpal::SampleFormat::F32 {
        return Err(format!(
            "\"{name}\" uses sample format {:?}; this build only handles 32-bit float streams.",
            cfg.sample_format()
        ));
    }
    Ok(())
}

fn open(
    input: &str,
    output: Option<&str>,
    ctl: Arc<ControlState>,
    lost: Arc<AtomicBool>,
) -> Result<(cpal::Stream, Option<cpal::Stream>), String> {
    let host = cpal::default_host();
    let in_dev = find_device(host.input_devices().map_err(|e| e.to_string())?, input)
        .ok_or_else(|| format!("input device \"{input}\" not found"))?;
    let in_cfg = in_dev.default_input_config().map_err(|e| e.to_string())?;
    let in_ch = in_cfg.channels() as usize;
    let in_stream_cfg: cpal::StreamConfig = in_cfg.config();
    let lost_in = lost.clone();

    // Direct mode: Zoom keeps using the real microphone. We only listen for the meter.
    let Some(output) = output else {
        check_format(input, &in_cfg, false)?;
        let in_stream = in_dev
            .build_input_stream(
                &in_stream_cfg,
                move |data: &[f32], _: &cpal::InputCallbackInfo| meter_block(data, &ctl),
                move |_err| lost_in.store(true, Relaxed),
                None,
            )
            .map_err(|e| format!("could not open \"{input}\": {e}"))?;
        in_stream.play().map_err(|e| e.to_string())?;
        return Ok((in_stream, None));
    };

    let out_dev = find_device(host.output_devices().map_err(|e| e.to_string())?, output)
        .ok_or_else(|| format!("output device \"{output}\" not found"))?;
    let out_cfg = out_dev.default_output_config().map_err(|e| e.to_string())?;
    check_format(input, &in_cfg, true)?;
    check_format(output, &out_cfg, true)?;
    let out_ch = out_cfg.channels() as usize;
    let out_stream_cfg: cpal::StreamConfig = out_cfg.config();

    // Fresh ring and processors for every start: never resume a stale ring.
    ctl.capture_block.store(0, Relaxed);
    let (mut cap, mut ren) = pipeline(ctl);

    let in_stream = in_dev
        .build_input_stream(
            &in_stream_cfg,
            move |data: &[f32], _: &cpal::InputCallbackInfo| cap.process(data, in_ch),
            move |_err| lost_in.store(true, Relaxed),
            None,
        )
        .map_err(|e| format!("could not open \"{input}\": {e}"))?;

    let lost_out = lost;
    let out_stream = out_dev
        .build_output_stream(
            &out_stream_cfg,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| ren.process(data, out_ch),
            move |_err| lost_out.store(true, Relaxed),
            None,
        )
        .map_err(|e| format!("could not open \"{output}\": {e}"))?;

    in_stream.play().map_err(|e| e.to_string())?;
    out_stream.play().map_err(|e| e.to_string())?;
    Ok((in_stream, Some(out_stream)))
}
