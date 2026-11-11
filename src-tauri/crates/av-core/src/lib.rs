//! AVcompanion real-time audio core.
//!
//! Everything in this crate is safe to call from an audio callback:
//! no locks, no allocation after construction, no syscalls.
//! Internal format is mono `f32` at 48 kHz.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicUsize, Ordering::*};
use std::sync::Arc;

pub const SAMPLE_RATE: u32 = 48_000;
pub const GAIN_MIN_CDB: i32 = -6000;
pub const GAIN_MAX_CDB: i32 = 2400;

// ---------------------------------------------------------------- control

/// Shared between UI, daemon, hotkeys and both audio callbacks.
/// Every field is independent, so `Relaxed` ordering is sufficient.
#[derive(Debug)]
pub struct ControlState {
    pub mute: AtomicBool,
    /// Gain in centi-dB (-600 = -6.0 dB). Writers clamp via `set_gain_cdb`.
    pub gain_cdb: AtomicI32,
    /// Capture peak since last poll, as `f32::to_bits` (non-negative).
    pub peak_bits: AtomicU32,
    /// Set when the soft clipper engaged since last poll.
    pub clipped: AtomicBool,
    pub xruns: AtomicU32,
    /// Largest capture block seen, in frames. Used to size the ring cushion.
    pub capture_block: AtomicUsize,
}

impl Default for ControlState {
    fn default() -> Self {
        Self {
            mute: AtomicBool::new(false),
            gain_cdb: AtomicI32::new(0),
            peak_bits: AtomicU32::new(0),
            clipped: AtomicBool::new(false),
            xruns: AtomicU32::new(0),
            capture_block: AtomicUsize::new(0),
        }
    }
}

impl ControlState {
    pub fn set_gain_cdb(&self, cdb: i32) -> i32 {
        let c = cdb.clamp(GAIN_MIN_CDB, GAIN_MAX_CDB);
        self.gain_cdb.store(c, Relaxed);
        c
    }
    pub fn nudge_gain_cdb(&self, delta: i32) -> i32 {
        self.set_gain_cdb(self.gain_cdb.load(Relaxed).saturating_add(delta))
    }
    pub fn toggle_mute(&self) -> bool {
        !self.mute.fetch_xor(true, Relaxed)
    }
    /// Take the capture peak (linear, 0.0..) accumulated since the last call.
    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.peak_bits.swap(0, Relaxed))
    }
}

// -------------------------------------------------------------------- dsp

#[inline]
pub fn cdb_to_lin(cdb: i32) -> f32 {
    10f32.powf(cdb as f32 / 2000.0)
}

/// Linear amplitude to centi-dBFS, floored at -120 dB.
pub fn lin_to_cdb(x: f32) -> i32 {
    if x <= 1.0e-6 {
        -12000
    } else {
        (2000.0 * x.log10()).round() as i32
    }
}

pub const CLIP_KNEE: f32 = 0.8;
/// Hard ceiling of the clipper output (about -0.09 dBFS).
pub const CLIP_CEIL: f32 = 0.99;

/// Unity gain below the knee, tanh above it, slope-continuous at the knee.
/// Output magnitude never exceeds `CLIP_CEIL`.
#[inline]
pub fn soft_clip(x: f32) -> f32 {
    let a = x.abs();
    if a <= CLIP_KNEE {
        x
    } else {
        let h = CLIP_CEIL - CLIP_KNEE;
        x.signum() * (CLIP_KNEE + h * ((a - CLIP_KNEE) / h).tanh())
    }
}

// ------------------------------------------------------------------- ring

struct RingInner {
    buf: Box<[AtomicU32]>,
    mask: usize,
    head: AtomicUsize, // next write index (free-running)
    tail: AtomicUsize, // next read index (free-running)
}

/// Wait-free single-producer single-consumer ring of `f32`.
/// Samples are stored as bit patterns in atomics, so there is no `unsafe`.
pub fn ring(min_capacity: usize) -> (Producer, Consumer) {
    let cap = min_capacity.max(2).next_power_of_two();
    let buf: Vec<AtomicU32> = (0..cap).map(|_| AtomicU32::new(0)).collect();
    let inner = Arc::new(RingInner {
        buf: buf.into_boxed_slice(),
        mask: cap - 1,
        head: AtomicUsize::new(0),
        tail: AtomicUsize::new(0),
    });
    (Producer { r: inner.clone() }, Consumer { r: inner })
}

pub struct Producer {
    r: Arc<RingInner>,
}

pub struct Consumer {
    r: Arc<RingInner>,
}

impl Producer {
    /// Returns false (and stores nothing) when the ring is full.
    #[inline]
    pub fn push(&mut self, v: f32) -> bool {
        let h = self.r.head.load(Relaxed);
        let t = self.r.tail.load(Acquire);
        if h.wrapping_sub(t) > self.r.mask {
            return false;
        }
        self.r.buf[h & self.r.mask].store(v.to_bits(), Relaxed);
        self.r.head.store(h.wrapping_add(1), Release);
        true
    }
}

impl Consumer {
    #[inline]
    pub fn pop(&mut self) -> Option<f32> {
        let t = self.r.tail.load(Relaxed);
        let h = self.r.head.load(Acquire);
        if t == h {
            return None;
        }
        let v = f32::from_bits(self.r.buf[t & self.r.mask].load(Relaxed));
        self.r.tail.store(t.wrapping_add(1), Release);
        Some(v)
    }
    #[inline]
    pub fn len(&self) -> usize {
        self.r
            .head
            .load(Acquire)
            .wrapping_sub(self.r.tail.load(Relaxed))
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Drop everything currently queued.
    pub fn clear(&mut self) {
        let h = self.r.head.load(Acquire);
        self.r.tail.store(h, Release);
    }
}

// ------------------------------------------------------------------ drift

/// Reads the ring at a slightly variable rate so that its fill level stays
/// at a target. This absorbs the clock difference between the capture and
/// render devices. Linear interpolation; adequate for speech.
pub struct DriftReader {
    target: f32,
    fill_lp: f32,
    ratio: f64,
    phase: f64,
    s0: f32,
    s1: f32,
    primed: bool,
}

/// Proportional gain: ratio offset per frame of fill error.
const DRIFT_KP: f64 = 2.0e-6;
/// Maximum rate correction, +/- 500 ppm (under one cent of pitch).
const DRIFT_MAX: f64 = 500.0e-6;
/// Fill low-pass coefficient per block.
const DRIFT_ALPHA: f32 = 0.02;

impl DriftReader {
    pub fn new(target_frames: usize) -> Self {
        Self {
            target: target_frames as f32,
            fill_lp: target_frames as f32,
            ratio: 1.0,
            phase: 0.0,
            s0: 0.0,
            s1: 0.0,
            primed: false,
        }
    }
    pub fn set_target(&mut self, frames: usize) {
        self.target = frames as f32;
    }
    pub fn ratio_ppm(&self) -> f64 {
        (self.ratio - 1.0) * 1.0e6
    }
    pub fn is_primed(&self) -> bool {
        self.primed
    }

    /// Call once at the start of each render block with the current ring fill.
    pub fn begin_block(&mut self, fill: usize) {
        if !self.primed {
            if fill as f32 >= self.target {
                self.primed = true;
                self.fill_lp = fill as f32;
                self.ratio = 1.0;
            }
            return;
        }
        self.fill_lp += DRIFT_ALPHA * (fill as f32 - self.fill_lp);
        let err = (self.fill_lp - self.target) as f64;
        self.ratio = 1.0 + (DRIFT_KP * err).clamp(-DRIFT_MAX, DRIFT_MAX);
    }

    /// Next output sample. Returns silence while priming or after an underrun.
    #[inline]
    pub fn next(&mut self, c: &mut Consumer, xruns: &AtomicU32) -> f32 {
        if !self.primed {
            return 0.0;
        }
        self.phase += self.ratio;
        while self.phase >= 1.0 {
            self.phase -= 1.0;
            self.s0 = self.s1;
            match c.pop() {
                Some(v) => self.s1 = v,
                None => {
                    xruns.fetch_add(1, Relaxed);
                    self.primed = false;
                    self.s0 = 0.0;
                    self.s1 = 0.0;
                    self.phase = 0.0;
                    return 0.0;
                }
            }
        }
        self.s0 + (self.s1 - self.s0) * self.phase as f32
    }
}

// ------------------------------------------------------------- processors

/// Extra cushion on top of one capture block plus one render block.
pub const RING_MARGIN_FRAMES: usize = 96; // 2 ms

/// Runs inside the capture callback.
pub struct CaptureProcessor {
    prod: Producer,
    ctl: Arc<ControlState>,
}

impl CaptureProcessor {
    pub fn new(prod: Producer, ctl: Arc<ControlState>) -> Self {
        Self { prod, ctl }
    }

    /// `input` is interleaved with `channels` channels. Downmixes to mono.
    pub fn process(&mut self, input: &[f32], channels: usize) {
        let channels = channels.max(1);
        let frames = input.len() / channels;
        self.ctl.capture_block.fetch_max(frames, Relaxed);
        let inv = 1.0 / channels as f32;
        let mut peak = 0.0f32;
        let mut dropped = false;
        for frame in input.chunks_exact(channels) {
            let mut m = 0.0f32;
            for s in frame {
                m += *s;
            }
            m *= inv;
            let a = m.abs();
            if a > peak {
                peak = a;
            }
            if !self.prod.push(m) {
                dropped = true;
            }
        }
        if dropped {
            self.ctl.xruns.fetch_add(1, Relaxed);
        }
        // Bit patterns of non-negative floats are order-preserving.
        self.ctl.peak_bits.fetch_max(peak.to_bits(), Relaxed);
    }
}

/// Runs inside the render callback.
pub struct RenderProcessor {
    cons: Consumer,
    ctl: Arc<ControlState>,
    drift: DriftReader,
    gain: f32,
    max_block: usize,
}

impl RenderProcessor {
    pub fn new(cons: Consumer, ctl: Arc<ControlState>) -> Self {
        Self {
            cons,
            ctl,
            drift: DriftReader::new(1056),
            gain: 0.0, // fade in from silence
            max_block: 0,
        }
    }

    pub fn drift_ppm(&self) -> f64 {
        self.drift.ratio_ppm()
    }

    /// Fills interleaved `out` with the processed mono signal on every channel.
    pub fn process(&mut self, out: &mut [f32], channels: usize) {
        let channels = channels.max(1);
        let frames = out.len() / channels;
        if frames == 0 {
            return;
        }
        if frames > self.max_block {
            self.max_block = frames;
        }
        let target_fill =
            self.ctl.capture_block.load(Relaxed) + self.max_block + RING_MARGIN_FRAMES;
        self.drift.set_target(target_fill);
        self.drift.begin_block(self.cons.len());

        let target = if self.ctl.mute.load(Relaxed) {
            0.0
        } else {
            cdb_to_lin(self.ctl.gain_cdb.load(Relaxed))
        };
        let step = (target - self.gain) / frames as f32;
        let mut clipped = false;
        for frame in out.chunks_exact_mut(channels) {
            self.gain += step;
            let x = self.drift.next(&mut self.cons, &self.ctl.xruns) * self.gain;
            if x.abs() > CLIP_KNEE {
                clipped = true;
            }
            let y = soft_clip(x);
            for s in frame.iter_mut() {
                *s = y;
            }
        }
        self.gain = target; // remove accumulated rounding
        if clipped {
            self.ctl.clipped.store(true, Relaxed);
        }
    }
}

/// Direct mode: the app does not sit in the audio path, it only meters the
/// microphone. Runs inside a capture callback.
pub fn meter_block(input: &[f32], ctl: &ControlState) {
    let mut peak = 0.0f32;
    for s in input {
        let a = s.abs();
        if a > peak {
            peak = a;
        }
    }
    ctl.peak_bits.fetch_max(peak.to_bits(), Relaxed);
    if peak >= CLIP_CEIL {
        ctl.clipped.store(true, Relaxed);
    }
}

/// Build a matched capture/render pair around one ring.
pub fn pipeline(ctl: Arc<ControlState>) -> (CaptureProcessor, RenderProcessor) {
    let (p, c) = ring(16_384);
    (
        CaptureProcessor::new(p, ctl.clone()),
        RenderProcessor::new(c, ctl),
    )
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soft_clip_is_identity_below_knee_and_bounded_above() {
        for i in -80..=80 {
            let x = i as f32 / 100.0;
            assert_eq!(soft_clip(x), x);
        }
        for x in [0.81f32, 1.0, 2.0, 10.0, 1000.0] {
            let y = soft_clip(x);
            assert!(y > CLIP_KNEE && y <= CLIP_CEIL, "{x} -> {y}");
            assert_eq!(soft_clip(-x), -y);
        }
        // continuous at the knee
        assert!((soft_clip(0.8001) - 0.8001).abs() < 1e-4);
    }

    #[test]
    fn gain_conversion() {
        assert!((cdb_to_lin(0) - 1.0).abs() < 1e-6);
        assert!((cdb_to_lin(-600) - 0.5012).abs() < 1e-3);
        assert!((cdb_to_lin(2000) - 10.0).abs() < 1e-3);
        assert_eq!(lin_to_cdb(1.0), 0);
        assert_eq!(lin_to_cdb(0.0), -12000);
        assert!((lin_to_cdb(0.5) - -602).abs() <= 1);
    }

    #[test]
    fn control_clamps_and_toggles() {
        let c = ControlState::default();
        assert_eq!(c.set_gain_cdb(99_999), GAIN_MAX_CDB);
        assert_eq!(c.set_gain_cdb(-99_999), GAIN_MIN_CDB);
        c.set_gain_cdb(0);
        assert_eq!(c.nudge_gain_cdb(-50), -50);
        assert!(c.toggle_mute());
        assert!(!c.toggle_mute());
    }

    #[test]
    fn ring_fifo_and_full() {
        let (mut p, mut c) = ring(4);
        for i in 0..4 {
            assert!(p.push(i as f32));
        }
        assert!(!p.push(9.0));
        assert_eq!(c.len(), 4);
        for i in 0..4 {
            assert_eq!(c.pop(), Some(i as f32));
        }
        assert_eq!(c.pop(), None);
        // wrap-around
        for round in 0..100 {
            assert!(p.push(round as f32));
            assert_eq!(c.pop(), Some(round as f32));
        }
    }

    #[test]
    fn ring_across_threads() {
        let (mut p, mut c) = ring(1024);
        const N: u32 = 200_000;
        let t = std::thread::spawn(move || {
            let mut i = 0u32;
            while i < N {
                if p.push(i as f32) {
                    i += 1;
                } else {
                    std::thread::yield_now();
                }
            }
        });
        let mut expect = 0u32;
        while expect < N {
            if let Some(v) = c.pop() {
                assert_eq!(v, expect as f32);
                expect += 1;
            } else {
                std::thread::yield_now();
            }
        }
        t.join().unwrap();
    }

    /// Simulate two devices with different clocks and block sizes.
    /// Returns (xruns after warm-up, min fill, max fill, final drift ppm).
    fn simulate(
        ppm: f64,
        cap_block: usize,
        ren_block: usize,
        seconds: f64,
    ) -> (u32, usize, usize, f64) {
        let ctl = Arc::new(ControlState::default());
        let (mut cap, mut ren) = pipeline(ctl.clone());
        let fs = SAMPLE_RATE as f64;
        let cap_dt = cap_block as f64 / (fs * (1.0 + ppm * 1e-6));
        let ren_dt = ren_block as f64 / fs;
        let input = vec![0.25f32; cap_block];
        let mut out = vec![0.0f32; ren_block * 2];
        let (mut tc, mut tr) = (0.0f64, 0.0037f64);
        let (mut min_fill, mut max_fill) = (usize::MAX, 0usize);
        let mut warm_xruns = 0;
        let warm = 2.0;
        while tc < seconds || tr < seconds {
            if tc <= tr {
                cap.process(&input, 1);
                tc += cap_dt;
            } else {
                if tr > warm {
                    let f = ren.cons.len();
                    min_fill = min_fill.min(f);
                    max_fill = max_fill.max(f);
                } else {
                    warm_xruns = ctl.xruns.load(Relaxed);
                }
                ren.process(&mut out, 2);
                tr += ren_dt;
            }
        }
        (
            ctl.xruns.load(Relaxed) - warm_xruns,
            min_fill,
            max_fill,
            ren.drift_ppm(),
        )
    }

    #[test]
    fn drift_is_absorbed_without_xruns() {
        for &(ppm, cb, rb) in &[
            (0.0, 480, 480),
            (50.0, 480, 480),
            (-50.0, 480, 480),
            (200.0, 480, 480),
            (-200.0, 480, 480),
            (120.0, 480, 128),
            (-120.0, 144, 480),
            (300.0, 441, 512),
        ] {
            let (xr, lo, hi, d) = simulate(ppm, cb, rb, 600.0);
            println!("ppm {ppm:>6} cap {cb} ren {rb}: xruns {xr} fill {lo}..{hi} drift {d:.1}");
            assert_eq!(xr, 0, "xruns at {ppm} ppm ({cb}/{rb})");
            assert!(
                lo >= rb,
                "fill {lo} fell below one render block at {ppm} ppm"
            );
            assert!(
                hi < 8 * (cb + rb),
                "fill {hi} grew without bound at {ppm} ppm"
            );
            assert!(d.abs() <= 500.0, "correction {d} ppm exceeds the clamp");
        }
    }

    #[test]
    fn mute_fades_to_exact_silence_and_gain_applies() {
        let ctl = Arc::new(ControlState::default());
        let (mut cap, mut ren) = pipeline(ctl.clone());
        let input = vec![0.5f32; 480];
        let mut out = vec![0.0f32; 480 * 2];
        for _ in 0..20 {
            cap.process(&input, 1);
            ren.process(&mut out, 2);
        }
        assert!(
            (out[900] - 0.5).abs() < 1e-3,
            "unity gain passthrough, got {}",
            out[900]
        );
        assert_eq!(out[900], out[901], "both channels carry the same signal");

        ctl.set_gain_cdb(-600);
        for _ in 0..3 {
            cap.process(&input, 1);
            ren.process(&mut out, 2);
        }
        assert!((out[900] - 0.5 * cdb_to_lin(-600)).abs() < 1e-3);

        ctl.mute.store(true, Relaxed);
        cap.process(&input, 1);
        ren.process(&mut out, 2); // fade block
        assert!(out[0].abs() > 0.0 && out[958].abs() < out[0].abs());
        cap.process(&input, 1);
        ren.process(&mut out, 2);
        assert!(
            out.iter().all(|s| *s == 0.0),
            "muted output must be exact zeros"
        );
        assert!((ctl.take_peak() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn hot_signal_is_clipped_and_flagged() {
        let ctl = Arc::new(ControlState::default());
        ctl.set_gain_cdb(2400);
        let (mut cap, mut ren) = pipeline(ctl.clone());
        let input = vec![0.9f32; 480];
        let mut out = vec![0.0f32; 480];
        for _ in 0..20 {
            cap.process(&input, 1);
            ren.process(&mut out, 1);
        }
        assert!(out.iter().all(|s| s.abs() <= CLIP_CEIL));
        assert!(ctl.clipped.load(Relaxed));
    }

    #[test]
    fn meter_block_tracks_peak_and_clip() {
        let ctl = ControlState::default();
        meter_block(&[0.1, -0.4, 0.2], &ctl);
        assert!(!ctl.clipped.load(Relaxed));
        assert!((ctl.take_peak() - 0.4).abs() < 1e-6);
        meter_block(&[0.0, 1.0], &ctl);
        assert!(ctl.clipped.load(Relaxed));
    }

    #[test]
    fn stereo_capture_is_downmixed() {
        let ctl = Arc::new(ControlState::default());
        let (mut cap, mut ren) = pipeline(ctl.clone());
        let mut input = vec![0.0f32; 960];
        for f in input.chunks_exact_mut(2) {
            f[0] = 0.6;
            f[1] = 0.2;
        }
        let mut out = vec![0.0f32; 480];
        for _ in 0..20 {
            cap.process(&input, 2);
            ren.process(&mut out, 1);
        }
        assert!((out[400] - 0.4).abs() < 1e-3);
    }
}
