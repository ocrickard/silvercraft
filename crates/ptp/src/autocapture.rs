//! Auto-capture state machine: fire when a detected film frame
//! has been present and held
//! steady, then wait for the film to advance before arming again.
//!
//! Pure: fed a rectangle and a small content signature per live frame, it
//! returns a decision. The caller fires the shutter and calls
//! [`AutoCapture::notify_captured`].
//!
//! **Fix for the "stuck in advance film" hazard.** Keeping the
//! captured signature forever means that when the next frame looks like the last
//! one (blank or underexposed frames, brackets) the duplicate guard holds
//! forever. Here, physical film movement is also detected from geometry: when
//! confident detections show the frame rectangle displaced from where it was
//! captured by at least `advance_shift_frac` of its size, for
//! `advance_frames` samples (not necessarily consecutive; a moving strip loses
//! detection intermittently), the film has moved and the duplicate guard is
//! released. A hand crossing the gate does not move the detected rectangle, so
//! the obstruction case still never duplicates. When no evidence arrives the
//! decision carries `stuck = true` after `stuck_hint_after` seconds so the UI
//! can offer manual capture, and [`AutoCapture::release_hold`] lets the user
//! clear the guard explicitly.

/// A rectangle in live-frame pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    /// Left.
    pub x: i32,
    /// Top.
    pub y: i32,
    /// Width.
    pub width: i32,
    /// Height.
    pub height: i32,
}

impl Rect {
    /// Construct.
    pub fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self { x, y, width, height }
    }

    fn center(&self) -> (f64, f64) {
        (f64::from(self.x) + f64::from(self.width) / 2.0, f64::from(self.y) + f64::from(self.height) / 2.0)
    }
}

/// A small greyscale fingerprint (row-major, `size` x `size`, 0..255 scale).
#[derive(Debug, Clone, PartialEq)]
pub struct Signature {
    /// Side length.
    pub size: usize,
    /// Values.
    pub data: Vec<f32>,
}

impl Signature {
    /// A constant signature (tests, placeholders).
    pub fn filled(size: usize, value: f32) -> Self {
        Self { size, data: vec![value; size * size] }
    }

    /// Whether every value is finite.
    pub fn is_finite(&self) -> bool {
        self.data.iter().all(|v| v.is_finite())
    }

    /// Population standard deviation (the app gates confidence on `std < 0.75`).
    pub fn std(&self) -> f64 {
        if self.data.is_empty() {
            return 0.0;
        }
        let n = self.data.len() as f64;
        let mean = self.data.iter().map(|&v| f64::from(v)).sum::<f64>() / n;
        (self.data.iter().map(|&v| (f64::from(v) - mean).powi(2)).sum::<f64>() / n).sqrt()
    }

    /// Add a constant (tests).
    pub fn offset(&self, d: f32) -> Self {
        Self { size: self.size, data: self.data.iter().map(|v| v + d).collect() }
    }
}

/// Mean absolute difference; +inf when either is missing, shapes differ or
/// any value is non-finite.
pub fn sig_diff(a: Option<&Signature>, b: Option<&Signature>) -> f64 {
    let (Some(a), Some(b)) = (a, b) else { return f64::INFINITY };
    if a.size != b.size || a.data.len() != b.data.len() || a.data.is_empty() || !a.is_finite() || !b.is_finite() {
        return f64::INFINITY;
    }
    let s: f64 = a.data.iter().zip(&b.data).map(|(x, y)| f64::from((x - y).abs())).sum();
    s / a.data.len() as f64
}

/// Per-axis box-filter weights for an area resample of `src` samples to `dst`.
fn area_weights(src: usize, dst: usize) -> Vec<Vec<(usize, f64)>> {
    let scale = src as f64 / dst as f64;
    (0..dst)
        .map(|o| {
            let lo = o as f64 * scale;
            let hi = (o + 1) as f64 * scale;
            let mut w = Vec::new();
            let mut i = lo.floor() as usize;
            while (i as f64) < hi && i < src {
                let a = lo.max(i as f64);
                let b = hi.min((i + 1) as f64);
                if b > a {
                    w.push((i, (b - a) / scale));
                }
                i += 1;
            }
            w
        })
        .collect()
}

/// Content signature of a frame: crop `rect` from an interleaved RGB8 frame,
/// grey = plain mean of R, G, B, area-resample to `size` x `size`.
///
/// The resample is an exact fractional box filter, which matches OpenCV's
/// INTER_AREA when downscaling (the normal case; live crops are hundreds of
/// pixels). For crops smaller than `size` it is piecewise constant, where
/// OpenCV would interpolate bilinearly.
///
/// Returns `None` for no rect, an empty crop, a zero `size` or a buffer
/// shorter than `width * height * 3`.
pub fn frame_signature(rgb: &[u8], width: usize, height: usize, rect: Option<Rect>, size: usize) -> Option<Signature> {
    let rect = rect?;
    if size == 0 || width.checked_mul(height)?.checked_mul(3)? > rgb.len() {
        return None;
    }
    let clamp = |v: i64, hi: usize| v.clamp(0, hi as i64) as usize;
    let x0 = clamp(i64::from(rect.x), width);
    let y0 = clamp(i64::from(rect.y), height);
    let x1 = clamp(i64::from(rect.x) + i64::from(rect.width), width);
    let y1 = clamp(i64::from(rect.y) + i64::from(rect.height), height);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let (cw, ch) = (x1 - x0, y1 - y0);
    // Horizontal pass on grey rows.
    let wx = area_weights(cw, size);
    let wy = area_weights(ch, size);
    let mut rows = vec![0f64; ch * size];
    for yy in 0..ch {
        let row = (y0 + yy) * width;
        for (ox, weights) in wx.iter().enumerate() {
            let mut acc = 0.0;
            for &(ix, w) in weights {
                let p = (row + x0 + ix) * 3;
                let g = rgb.get(p..p + 3).map_or(0.0, |px| (f64::from(px[0]) + f64::from(px[1]) + f64::from(px[2])) / 3.0);
                acc += g * w;
            }
            rows[yy * size + ox] = acc;
        }
    }
    let mut data = vec![0f32; size * size];
    for (oy, weights) in wy.iter().enumerate() {
        for ox in 0..size {
            let mut acc = 0.0;
            for &(iy, w) in weights {
                acc += rows[iy * size + ox] * w;
            }
            data[oy * size + ox] = acc as f32;
        }
    }
    Some(Signature { size, data })
}

/// Phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoPhase {
    /// Waiting for a steady frame.
    Searching,
    /// Frame present, counting stable samples.
    Settling,
    /// Captured; waiting for the film to move on.
    WaitAdvance,
}

/// One decision.
#[derive(Debug, Clone, PartialEq)]
pub struct AutoDecision {
    /// Fire the shutter now.
    pub should_fire: bool,
    /// Phase after this sample.
    pub phase: AutoPhase,
    /// Status text.
    pub message: String,
    /// Held in "advance film" for a long time: offer manual capture.
    pub stuck: bool,
}

impl AutoDecision {
    fn new(should_fire: bool, phase: AutoPhase, message: impl Into<String>) -> Self {
        Self { should_fire, phase, message: message.into(), stuck: false }
    }
}

/// Tunables. `Default` is the class defaults; [`AutoCaptureParams::app`] the
/// production app's values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AutoCaptureParams {
    /// Stable samples required.
    pub settle_frames: u32,
    /// Max signature change for "stable".
    pub settle_tol: f64,
    /// Signature change that counts as a different frame.
    pub advance_thresh: f64,
    /// Samples of evidence needed for an advance (clamped >= 1).
    pub advance_frames: u32,
    /// Seconds the frame must hold.
    pub settle_seconds: f64,
    /// A gap between samples longer than this restarts settling.
    pub max_sample_gap: f64,
    /// Rectangle displacement (fraction of its size) that counts as the film
    /// physically moving. New in SilverCraft.
    pub advance_shift_frac: f64,
    /// Seconds in "advance film" before `stuck` is raised. New in SilverCraft.
    pub stuck_hint_after: f64,
}

impl Default for AutoCaptureParams {
    fn default() -> Self {
        Self {
            settle_frames: 10,
            settle_tol: 6.0,
            advance_thresh: 18.0,
            advance_frames: 3,
            settle_seconds: 2.0,
            max_sample_gap: 0.75,
            advance_shift_frac: 0.25,
            stuck_hint_after: 8.0,
        }
    }
}

impl AutoCaptureParams {
    /// The production app's values (`settle_tol` 2.5, `advance_frames` 5).
    pub fn app() -> Self {
        Self { settle_tol: 2.5, advance_frames: 5, ..Self::default() }
    }
}

/// The state machine.
#[derive(Debug, Clone)]
pub struct AutoCapture {
    p: AutoCaptureParams,
    phase: AutoPhase,
    last_sig: Option<Signature>,
    stable: u32,
    captured_sig: Option<Signature>,
    captured_rect: Option<Rect>,
    advance_count: u32,
    shift_evidence: u32,
    anchor_sig: Option<Signature>,
    anchor_rect: Option<Rect>,
    settle_since: Option<f64>,
    last_sample: Option<f64>,
    hold_since: Option<f64>,
}

impl AutoCapture {
    /// New controller.
    pub fn new(mut p: AutoCaptureParams) -> Self {
        p.advance_frames = p.advance_frames.max(1);
        Self {
            p,
            phase: AutoPhase::Searching,
            last_sig: None,
            stable: 0,
            captured_sig: None,
            captured_rect: None,
            advance_count: 0,
            shift_evidence: 0,
            anchor_sig: None,
            anchor_rect: None,
            settle_since: None,
            last_sample: None,
            hold_since: None,
        }
    }

    /// Forget everything (auto-capture toggled).
    pub fn reset(&mut self) {
        *self = Self::new(self.p);
    }

    /// Current phase.
    pub fn phase(&self) -> AutoPhase {
        self.phase
    }

    /// Parameters.
    pub fn params(&self) -> &AutoCaptureParams {
        &self.p
    }

    /// Restart the settling timer (motion, stale delivery, crop edit). Keeps
    /// the captured-frame hold and the phase.
    pub fn interrupt_settling(&mut self) {
        self.settle_since = None;
        self.stable = 0;
        self.last_sig = None;
        self.anchor_sig = None;
        self.anchor_rect = None;
    }

    /// User override: "this is a new frame". Clears the duplicate guard and
    /// re-arms.
    pub fn release_hold(&mut self) {
        self.captured_sig = None;
        self.captured_rect = None;
        self.shift_evidence = 0;
        self.advance_count = 0;
        self.hold_since = None;
        self.phase = AutoPhase::Searching;
        self.interrupt_settling();
    }

    /// Call after firing: hold until the film advances. `rect` is where the
    /// frame was captured (enables geometric advance detection).
    pub fn notify_captured(&mut self, sig: Option<&Signature>, rect: Option<Rect>) {
        self.captured_sig = sig.cloned();
        self.captured_rect = rect;
        self.shift_evidence = 0;
        self.advance_count = 0;
        self.phase = AutoPhase::WaitAdvance;
        self.stable = 0;
        self.last_sig = None;
        self.hold_since = None;
    }

    fn displaced(&self, rect: &Rect) -> bool {
        let Some(c) = self.captured_rect else { return false };
        let (cx, cy) = c.center();
        let (x, y) = rect.center();
        let fx = self.p.advance_shift_frac * f64::from(c.width.max(1));
        let fy = self.p.advance_shift_frac * f64::from(c.height.max(1));
        (x - cx).abs() >= fx || (y - cy).abs() >= fy
    }

    fn wait_decision(&mut self, now: f64, message: &str) -> AutoDecision {
        let since = *self.hold_since.get_or_insert(now);
        let mut d = AutoDecision::new(false, AutoPhase::WaitAdvance, message);
        if now - since >= self.p.stuck_hint_after {
            d.stuck = true;
            d.message = "captured — advance film (if this is already a new frame, capture manually)".into();
        }
        d
    }

    /// Feed one sample. `now` is in seconds (monotonic).
    pub fn update(&mut self, rect: Option<Rect>, sig: Option<&Signature>, now: f64, confident: bool) -> AutoDecision {
        if let Some(last) = self.last_sample
            && (now <= last || now - last > self.p.max_sample_gap)
        {
            self.interrupt_settling();
            self.advance_count = 0;
        }
        self.last_sample = Some(now);
        let valid = rect.is_some() && sig.is_some_and(Signature::is_finite);

        // Geometric advance evidence (the hazard fix).
        let mut geometric_advance = false;
        if self.captured_sig.is_some()
            && valid
            && confident
            && let Some(r) = rect
            && self.displaced(&r)
        {
            self.shift_evidence += 1;
            if self.shift_evidence >= self.p.advance_frames {
                self.captured_sig = None;
                self.captured_rect = None;
                self.shift_evidence = 0;
                geometric_advance = true;
            }
        }

        if self.phase == AutoPhase::WaitAdvance {
            let advanced = valid && sig_diff(sig, self.captured_sig.as_ref()) > self.p.advance_thresh;
            self.advance_count = if advanced { self.advance_count + 1 } else { 0 };
            if geometric_advance || self.advance_count >= self.p.advance_frames {
                self.phase = AutoPhase::Searching;
                self.hold_since = None;
                self.interrupt_settling();
                return AutoDecision::new(false, self.phase, "advance detected — hold steady");
            }
            return self.wait_decision(now, "captured — advance film");
        }

        let (Some(rect), Some(sig)) = (rect, sig) else {
            self.phase = AutoPhase::Searching;
            self.interrupt_settling();
            return AutoDecision::new(false, AutoPhase::Searching, "searching for a frame");
        };
        if !valid || !confident {
            self.phase = AutoPhase::Searching;
            self.interrupt_settling();
            let msg = if valid { "check frame edges or lock framing" } else { "searching for a frame" };
            return AutoDecision::new(false, AutoPhase::Searching, msg);
        }

        let tol = (f64::from(rect.width.min(rect.height)) * 0.004).max(1.0);
        let geometry_stable = self.anchor_rect.is_some_and(|a| {
            [(rect.x, a.x), (rect.y, a.y), (rect.width, a.width), (rect.height, a.height)].iter().all(|&(p, q)| f64::from(p.abs_diff(q)) <= tol)
        });
        let stable = geometry_stable
            && sig_diff(Some(sig), self.last_sig.as_ref()) < self.p.settle_tol
            && sig_diff(Some(sig), self.anchor_sig.as_ref()) < self.p.settle_tol;
        if stable {
            self.stable += 1;
        } else {
            self.stable = 1;
            self.settle_since = Some(now);
            self.anchor_sig = Some(sig.clone());
            self.anchor_rect = Some(rect);
        }
        self.last_sig = Some(sig.clone());
        self.phase = AutoPhase::Settling;
        let elapsed = now - self.settle_since.unwrap_or(now);
        // Do not mistake a transient obstruction for a new photo that returned
        // to the same captured contents while settling.
        if self.captured_sig.is_some() && sig_diff(Some(sig), self.captured_sig.as_ref()) <= self.p.advance_thresh {
            self.phase = AutoPhase::WaitAdvance;
            self.advance_count = 0;
            self.interrupt_settling();
            return self.wait_decision(now, "captured — advance film");
        }
        if self.stable >= self.p.settle_frames && elapsed >= self.p.settle_seconds {
            return AutoDecision::new(true, AutoPhase::Settling, "steady — capturing");
        }
        AutoDecision::new(false, AutoPhase::Settling, format!("hold steady — {:.1} s", (self.p.settle_seconds - elapsed).max(0.0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(v: f32) -> Signature {
        Signature::filled(16, v)
    }

    fn ramp() -> Signature {
        Signature { size: 32, data: (0..1024).map(|i| i as f32 / 8.0).collect() }
    }

    const RECT: Rect = Rect { x: 100, y: 80, width: 600, height: 400 };

    fn ac(f: impl FnOnce(&mut AutoCaptureParams)) -> AutoCapture {
        let mut p = AutoCaptureParams::default();
        f(&mut p);
        AutoCapture::new(p)
    }

    // tests/test_cameras.py:304-357 (implicit `now` was time.monotonic(); use 10 Hz).
    #[test]
    fn fires_when_frame_holds_steady() {
        let mut a = ac(|p| {
            p.settle_seconds = 0.0;
            p.advance_frames = 1;
            p.settle_frames = 5;
            p.settle_tol = 2.0;
            p.advance_thresh = 10.0;
        });
        let r = Rect::new(0, 0, 10, 10);
        assert!((0..5).any(|i| a.update(Some(r), Some(&sig(0.0)), f64::from(i) * 0.1, true).should_fire));
    }

    #[test]
    fn no_frame_never_fires() {
        let mut a = ac(|p| {
            p.settle_seconds = 0.0;
            p.settle_frames = 3;
        });
        for i in 0..10 {
            let d = a.update(None, None, f64::from(i) * 0.1, true);
            assert!(!d.should_fire);
            assert_eq!(d.phase, AutoPhase::Searching);
        }
    }

    #[test]
    fn moving_frame_never_fires() {
        let mut a = ac(|p| {
            p.settle_seconds = 0.0;
            p.settle_frames = 3;
            p.settle_tol = 2.0;
        });
        let r = Rect::new(0, 0, 10, 10);
        assert!(!(0..12).any(|i| a.update(Some(r), Some(&sig(i as f32 * 20.0)), f64::from(i) * 0.1, true).should_fire));
    }

    #[test]
    fn waits_for_advance_then_rearms() {
        let mut a = ac(|p| {
            p.settle_seconds = 0.0;
            p.advance_frames = 1;
            p.settle_frames = 3;
            p.settle_tol = 2.0;
            p.advance_thresh = 10.0;
        });
        let r = Rect::new(0, 0, 10, 10);
        let mut t = 0.0;
        let mut d = None;
        for _ in 0..3 {
            t += 0.1;
            d = Some(a.update(Some(r), Some(&sig(0.0)), t, true));
        }
        assert!(d.unwrap().should_fire);
        a.notify_captured(Some(&sig(0.0)), Some(r));
        for _ in 0..5 {
            t += 0.1;
            let d = a.update(Some(r), Some(&sig(0.0)), t, true);
            assert!(!d.should_fire);
            assert_eq!(d.phase, AutoPhase::WaitAdvance);
        }
        t += 0.1;
        assert_eq!(a.update(Some(r), Some(&sig(120.0)), t, true).phase, AutoPhase::Searching);
        assert!((0..4).any(|_| {
            t += 0.1;
            a.update(Some(r), Some(&sig(120.0)), t, true).should_fire
        }));
    }

    // tests/test_autocapture_stability.py:12-68.
    #[test]
    fn settling_requires_elapsed_time_not_sample_count() {
        let mut a = ac(|p| p.settle_frames = 5);
        let s = ramp();
        for i in 0..100 {
            assert!(!a.update(Some(RECT), Some(&s), f64::from(i) * 0.001, true).should_fire);
        }
        let mut t = 0.2;
        while t < 2.0 - 1e-9 {
            assert!(!a.update(Some(RECT), Some(&s), t, true).should_fire);
            t += 0.1;
        }
        assert!(a.update(Some(RECT), Some(&s), 2.1, true).should_fire);
    }

    #[test]
    fn slow_drift_restarts_settling() {
        for move_box in [false, true] {
            let mut a = ac(|p| p.settle_tol = 2.5);
            for i in 0..50 {
                let r = if move_box { Rect::new(100 + i, 80, 600, 400) } else { RECT };
                let s = if move_box { ramp() } else { ramp().offset(i as f32 * 0.4) };
                assert!(!a.update(Some(r), Some(&s), f64::from(i) * 0.1, true).should_fire);
            }
        }
    }

    #[test]
    fn uncertain_edges_stall_and_nan_cannot_fire() {
        let mut a = AutoCapture::new(AutoCaptureParams::default());
        let s = ramp();
        for i in 0..30 {
            assert!(!a.update(Some(RECT), Some(&s), f64::from(i) / 10.0, false).should_fire);
        }
        for i in 0..19 {
            assert!(!a.update(Some(RECT), Some(&s), 3.0 + f64::from(i) / 10.0, true).should_fire);
        }
        assert!(!a.update(Some(RECT), Some(&s), 7.0, true).should_fire);
        assert!(!a.update(Some(RECT), Some(&s), 7.5, true).should_fire);
        let nan = Signature { size: 32, data: vec![f32::NAN; 1024] };
        assert!(!a.update(Some(RECT), Some(&nan), 7.6, true).should_fire);
    }

    #[test]
    fn clock_regression_restarts_settling() {
        let mut a = ac(|p| {
            p.settle_frames = 3;
            p.settle_seconds = 0.2;
        });
        let s = ramp();
        for i in 0..3 {
            a.update(Some(RECT), Some(&s), 1.0 + f64::from(i) * 0.1, true);
        }
        // Time goes backwards: settling restarts, so no fire right away.
        assert!(!a.update(Some(RECT), Some(&s), 0.5, true).should_fire);
    }

    #[test]
    fn obstruction_and_return_do_not_duplicate() {
        let mut a = AutoCapture::new(AutoCaptureParams::default());
        let s = ramp();
        a.notify_captured(Some(&s), Some(RECT));
        for i in 0..6 {
            assert!(!a.update(None, None, f64::from(i) / 10.0, true).should_fire);
        }
        for i in 6..12 {
            assert!(!a.update(Some(RECT), Some(&s.offset(80.0)), f64::from(i) / 10.0, true).should_fire);
        }
        for i in 12..45 {
            assert!(!a.update(Some(RECT), Some(&s), f64::from(i) / 10.0, true).should_fire);
        }
        assert_eq!(a.phase(), AutoPhase::WaitAdvance);
        let fired = (45..72).any(|i| a.update(Some(RECT), Some(&s.offset(50.0)), f64::from(i) / 10.0, true).should_fire);
        assert!(fired);
    }

    /// The stuck-frame hazard: the next frame looks like the last one. The
    /// strip visibly slides through the gate, then the new (similar) frame
    /// settles where the old one was. Waiting forever held forever; we fire.
    #[test]
    fn similar_next_frame_after_physical_advance_fires() {
        let mut a = AutoCapture::new(AutoCaptureParams::app());
        let blank = Signature::filled(32, 40.0);
        a.notify_captured(Some(&blank), Some(RECT));
        let mut t = 0.0;
        let mut step = |a: &mut AutoCapture, r: Option<Rect>, s: Option<&Signature>| {
            t += 0.1;
            a.update(r, s, t, true)
        };
        // Still in the gate.
        for _ in 0..10 {
            assert_eq!(step(&mut a, Some(RECT), Some(&blank)).phase, AutoPhase::WaitAdvance);
        }
        // Advance: the frame slides left, with detection dropouts.
        for k in 1..=12 {
            let r = if k % 3 == 0 { None } else { Some(Rect::new(100 - 30 * k, 80, 600, 400)) };
            let s = r.map(|_| blank.offset(3.0));
            assert!(!step(&mut a, r, s.as_ref()).should_fire);
        }
        // New blank frame lands where the last one was and holds.
        let mut fired = false;
        for _ in 0..40 {
            fired |= step(&mut a, Some(RECT), Some(&blank.offset(1.0))).should_fire;
        }
        assert!(fired, "should re-arm after the strip physically moved");
    }

    /// Without the geometric evidence the same scene
    /// stays held; the controller raises `stuck` and manual release works.
    #[test]
    fn similar_frame_without_motion_is_held_with_stuck_hint() {
        let mut a = AutoCapture::new(AutoCaptureParams::app());
        let blank = Signature::filled(32, 40.0);
        a.notify_captured(Some(&blank), Some(RECT));
        let mut last = None;
        for i in 0..100 {
            let d = a.update(Some(RECT), Some(&blank.offset(1.0)), f64::from(i) * 0.1, true);
            assert!(!d.should_fire);
            last = Some(d);
        }
        let d = last.unwrap();
        assert_eq!(d.phase, AutoPhase::WaitAdvance);
        assert!(d.stuck);
        a.release_hold();
        let fired = (100..140).any(|i| a.update(Some(RECT), Some(&blank.offset(1.0)), f64::from(i) * 0.1, true).should_fire);
        assert!(fired);
    }

    /// A hand that drags a confident-but-shifted detection for fewer samples
    /// than `advance_frames` does not release the hold.
    #[test]
    fn brief_shift_does_not_release_hold() {
        let mut a = AutoCapture::new(AutoCaptureParams::app());
        let s = ramp();
        a.notify_captured(Some(&s), Some(RECT));
        let mut t = 0.0;
        for k in 0..4 {
            t += 0.1;
            a.update(Some(Rect::new(400 + k, 80, 600, 400)), Some(&s), t, true);
        }
        for _ in 0..60 {
            t += 0.1;
            assert!(!a.update(Some(RECT), Some(&s), t, true).should_fire);
        }
    }

    #[test]
    fn signature_of_uniform_and_gradient_crops() {
        let (w, h) = (64usize, 48usize);
        let mut rgb = vec![0u8; w * h * 3];
        for y in 0..h {
            for x in 0..w {
                let p = (y * w + x) * 3;
                rgb[p] = (x * 4) as u8;
                rgb[p + 1] = (x * 4) as u8;
                rgb[p + 2] = (x * 4) as u8;
            }
        }
        let s = frame_signature(&rgb, w, h, Some(Rect::new(0, 0, 64, 48)), 32).unwrap();
        assert_eq!(s.data.len(), 1024);
        // 2:1 horizontal box: mean of x*4 and (x+1)*4 for x = 2*ox.
        assert!((s.data[0] - 2.0).abs() < 1e-4, "{}", s.data[0]);
        assert!((s.data[31] - (62.0 * 4.0 + 2.0)).abs() < 1e-3);
        assert!(frame_signature(&rgb, w, h, None, 32).is_none());
        assert!(frame_signature(&rgb, w, h, Some(Rect::new(70, 0, 10, 10)), 32).is_none());
        assert!(frame_signature(&rgb, w, h, Some(Rect::new(-5, -5, 0, 10)), 32).is_none());
        assert!(frame_signature(&rgb[..10], w, h, Some(Rect::new(0, 0, 4, 4)), 32).is_none());
        assert!(frame_signature(&rgb, w, h, Some(Rect::new(0, 0, 4, 4)), 0).is_none());
        // Non-integer ratio and partially out-of-bounds rect.
        let s = frame_signature(&rgb, w, h, Some(Rect::new(-10, 5, 57, 1000)), 32).unwrap();
        assert!(s.is_finite());
        assert!(s.data.iter().all(|v| (0.0..=255.0).contains(v)));
    }

    #[test]
    fn sig_diff_edge_cases() {
        assert!(sig_diff(None, Some(&sig(0.0))).is_infinite());
        assert!(sig_diff(Some(&sig(0.0)), Some(&Signature::filled(8, 0.0))).is_infinite());
        assert_eq!(sig_diff(Some(&sig(1.0)), Some(&sig(3.0))), 2.0);
        assert!(sig_diff(Some(&Signature::filled(0, 0.0)), Some(&Signature::filled(0, 0.0))).is_infinite());
    }
}
