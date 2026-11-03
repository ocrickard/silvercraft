//! Rolls: every capture session in the rolls folder (listed in the sidebar). The canvas shows the
//! selected roll's frames as a grid (LightCraft's photo grid); the right panel develops it into
//! scene-linear DNGs and sends it to LightCraft.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::thread::JoinHandle;

use egui::{Align2, Rect, RichText, Sense, Stroke, StrokeKind, pos2, vec2};
use silvercraft_engine::manifest::RollManifest;
use silvercraft_engine::{ConfigSources, Event, RollOutcome, develop_roll_frames};

use crate::theme::tokens;
use crate::viewer::{Request, ViewFrame, Viewer};
use crate::widgets::{self, section};
use crate::{Shared, Status};

pub struct RollEntry {
    pub dir: PathBuf,
    pub manifest: RollManifest,
}

/// A develop running on a background thread.
struct Job {
    dir: PathBuf,
    /// The frames being developed again (all when `None`).
    only: Option<Vec<String>>,
    rx: Receiver<Result<Event, String>>,
    cancel: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<RollOutcome, String>>>,
}

#[derive(Default)]
pub struct RollsTab {
    pub rolls: Vec<RollEntry>,
    pub selected: Option<PathBuf>,
    /// Extra `key=value` config overrides, one per line.
    pub overrides: String,
    job: Option<Job>,
    pub progress: Option<(usize, usize, String)>,
    pub log: Vec<String>,
    pub outcome: Option<Result<RollOutcome, String>>,
    /// When the list was last read (egui time).
    refreshed_at: Option<f64>,
    /// A "Send again" running on a background thread (result receiver + handle).
    send_job: Option<(Receiver<String>, JoinHandle<()>)>,
    /// The result of the last "Send again".
    pub sent: Option<String>,
    /// Developed frames of the selected roll, as thumbnails (label, texture).
    pub thumbnails: Vec<(String, egui::TextureHandle)>,
    /// The roll `thumbnails` belong to.
    thumbs_of: Option<PathBuf>,
    /// A frame open at full size (double-click in the grid), over the grid.
    pub viewer: Option<Viewer>,
    /// Frames to develop again (their crop changed) once nothing else is developing.
    pub redevelop: Vec<(PathBuf, String)>,
}

/// Roll folders (with a `roll.json`) directly inside `root`, newest first.
pub fn find_rolls(root: &Path) -> Vec<RollEntry> {
    let mut out: Vec<(std::time::SystemTime, RollEntry)> = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else { return Vec::new() };
    for e in rd.flatten() {
        let dir = e.path();
        if !dir.is_dir() {
            continue;
        }
        if let Ok(manifest) = RollManifest::load(&dir) {
            let t = std::fs::metadata(dir.join(silvercraft_engine::manifest::FILE_NAME)).and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            out.push((t, RollEntry { dir, manifest }));
        }
    }
    out.sort_by_key(|e| std::cmp::Reverse(e.0));
    out.into_iter().map(|(_, r)| r).collect()
}

/// Where a roll's frame thumbnails are kept (written as each frame develops).
pub fn thumbs_dir(roll: &Path) -> PathBuf {
    roll.join(".thumbs")
}

/// A thumbnail as binary PPM (P6): tiny, no codec needed.
pub fn write_ppm(path: &Path, width: usize, height: usize, rgb: &[u8]) -> std::io::Result<()> {
    if rgb.len() != width * height * 3 {
        return Err(std::io::Error::other("thumbnail size mismatch"));
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut bytes = format!("P6\n{width} {height}\n255\n").into_bytes();
    bytes.extend_from_slice(rgb);
    std::fs::write(path, bytes)
}

/// Read a binary PPM written by [`write_ppm`]; `None` for anything else (hostile input).
pub fn read_ppm(bytes: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    // header: "P6" ws width ws height ws maxval single-ws, no comments (we write none)
    let mut fields = Vec::new();
    let mut i = 0;
    while fields.len() < 4 {
        while bytes.get(i)?.is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while !bytes.get(i)?.is_ascii_whitespace() {
            i += 1;
        }
        fields.push(std::str::from_utf8(bytes.get(start..i)?).ok()?);
    }
    i += 1; // the single whitespace after maxval
    if fields.first() != Some(&"P6") || fields.get(3) != Some(&"255") {
        return None;
    }
    let w: usize = fields.get(1)?.parse().ok()?;
    let h: usize = fields.get(2)?.parse().ok()?;
    if w == 0 || h == 0 || w > 4096 || h > 4096 {
        return None;
    }
    let data = bytes.get(i..i + w * h * 3)?;
    Some((w, h, data.to_vec()))
}

impl RollsTab {
    pub fn refresh(&mut self, shared: &Shared) {
        self.rolls = find_rolls(&shared.settings.rolls_dir);
    }

    /// Re-read the rolls folder every few seconds (new scans appear without a click).
    pub fn refresh_if_stale(&mut self, shared: &Shared, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        if self.refreshed_at.is_none_or(|t| now - t > 3.0 || now < t) {
            self.refresh(shared);
            self.refreshed_at = Some(now);
        }
    }

    pub fn running(&self) -> bool {
        self.job.is_some()
    }

    /// The roll being developed.
    pub fn job_dir(&self) -> Option<&Path> {
        self.job.as_ref().map(|j| j.dir.as_path())
    }

    pub fn select(&mut self, dir: PathBuf) {
        if self.selected.as_ref() != Some(&dir) {
            self.sent = None;
            self.viewer = None;
            if !self.running() {
                self.outcome = None;
            }
        }
        self.selected = Some(dir);
    }

    /// Select `dir` (refreshing the list) and develop it.
    pub fn develop_dir(&mut self, dir: PathBuf, shared: &Shared) {
        self.refresh(shared);
        self.select(dir);
        self.develop(shared);
    }

    /// Develop `dir` without sending it to LightCraft or leaving the current view (frames
    /// developing while the roll is still being captured).
    pub fn develop_in_background(&mut self, dir: PathBuf, shared: &Shared) {
        self.refresh(shared);
        self.select(dir.clone());
        self.develop_with(dir, shared, false, None);
    }

    /// Nothing is developing, and selecting `dir` wouldn't pull another roll out from under the
    /// open viewer.
    pub fn can_develop_in_background(&self, dir: &Path) -> bool {
        !self.running() && (self.viewer.is_none() || self.selected.as_deref() == Some(dir))
    }

    pub fn develop(&mut self, shared: &Shared) {
        let Some(dir) = self.selected.clone() else { return };
        self.develop_with(dir, shared, true, None);
    }

    /// Develop again the frames queued in [`Self::redevelop`] (one roll at a time), when nothing
    /// else is developing.
    pub fn develop_queued(&mut self, shared: &Shared) {
        let Some((dir, _)) = self.redevelop.first().cloned().filter(|_| !self.running()) else { return };
        let (these, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.redevelop).into_iter().partition(|(d, _)| *d == dir);
        self.redevelop = rest;
        let mut labels: Vec<String> = these.into_iter().map(|(_, l)| l).collect();
        labels.dedup();
        self.develop_with(dir, shared, false, Some(labels));
    }

    /// Develop roll `dir` (only the frames labelled `only`, when given); `send`: as the settings
    /// say, else never to LightCraft.
    fn develop_with(&mut self, dir: PathBuf, shared: &Shared, send: bool, only: Option<Vec<String>>) {
        if self.job.is_some() {
            return;
        }
        let req = match self.request_for(&dir, shared) {
            Ok(mut r) if !send => {
                r.sources.sets.push("handoff.send=false".into());
                r
            }
            Ok(r) => r,
            Err(e) => {
                self.outcome = Some(Err(e));
                return;
            }
        };
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        self.log.clear();
        if only.is_none() {
            if self.selected.as_ref() == Some(&dir) {
                self.thumbnails.clear();
                self.thumbs_of = Some(dir.clone());
            }
            let _ = std::fs::remove_dir_all(thumbs_dir(&dir));
        }
        self.outcome = None;
        self.progress = Some((0, req.frames.len() * 2, "starting".into()));
        let labels = only.clone();
        let thread = std::thread::Builder::new()
            .name("silvercraft-develop".into())
            .spawn(move || {
                let r = develop_roll_frames(&req, labels.as_deref(), &c, &mut |e| {
                    let _ = tx.send(Ok(e));
                })
                .map_err(|e| e.to_string());
                if let Err(e) = &r {
                    let _ = tx.send(Err(e.clone()));
                }
                r
            })
            .ok();
        self.job = Some(Job { dir, only, rx, cancel, thread });
    }

    fn request_for(&self, dir: &Path, shared: &Shared) -> Result<silvercraft_engine::RollRequest, String> {
        let manifest = RollManifest::load(dir).map_err(|e| e.to_string())?;
        let sources = ConfigSources {
            texts: vec![("app settings".into(), shared.settings.config_layer())],
            sets: self.overrides.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(String::from).collect(),
            ..Default::default()
        };
        let mut req = manifest.to_request(dir, sources);
        // developed frames go beside the captures unless the config says otherwise
        if !req.sources.sets.iter().any(|s| s.starts_with("output.dir")) {
            req.sources.sets.insert(0, format!("output.dir={:?}", dir.join("developed").to_string_lossy()));
        }
        Ok(req)
    }

    /// Whether a "Send again" is in flight.
    pub fn sending(&self) -> bool {
        self.send_job.is_some()
    }

    /// Send the selected roll's developed frames to LightCraft, on a background
    /// thread: each frame is a separate control request that can take over a
    /// minute against a busy or hung LightCraft, which must never freeze the UI.
    /// The outcome lands in [`sent`](Self::sent) via [`poll`](Self::poll). (There is
    /// no cancel: a blocked control read cannot be interrupted without closing the
    /// connection from another thread, which the client does not support yet.)
    pub fn send_again(&mut self, shared: &Shared) {
        if self.job.is_some() || self.send_job.is_some() {
            return;
        }
        let Some(dir) = self.selected.clone() else { return };
        let req = match self.request_for(&dir, shared) {
            Ok(mut r) => {
                r.sources.sets.push("handoff.send=true".into());
                r
            }
            Err(e) => {
                self.sent = Some(e);
                return;
            }
        };
        self.sent = None;
        let (tx, rx) = channel();
        let spawned = std::thread::Builder::new().name("silvercraft-send".into()).spawn(move || {
            let msg = silvercraft_engine::roll::send_developed(&req).unwrap_or_else(|e| e.to_string());
            let _ = tx.send(msg);
        });
        match spawned {
            Ok(thread) => self.send_job = Some((rx, thread)),
            Err(e) => self.sent = Some(format!("could not start the send: {e}")),
        }
    }

    pub fn cancel(&self) {
        if let Some(j) = &self.job {
            j.cancel.store(true, Ordering::Relaxed);
        }
    }

    pub fn poll(&mut self, ctx: &egui::Context) {
        let send_done = match &mut self.send_job {
            Some((rx, _)) => match rx.try_recv() {
                Ok(msg) => Some(msg),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Some("sending to LightCraft crashed".into()),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
            },
            None => None,
        };
        match send_done {
            Some(msg) => {
                self.sent = Some(msg);
                if let Some((_, t)) = self.send_job.take() {
                    let _ = t.join();
                }
            }
            None if self.send_job.is_some() => ctx.request_repaint_after(std::time::Duration::from_millis(250)),
            None => {}
        }
        let Some(job) = &mut self.job else { return };
        while let Ok(m) = job.rx.try_recv() {
            match m {
                Ok(Event::Diagnostic(d)) => self.log.push(d.to_string()),
                Ok(Event::Config { layers }) => self.log.push(format!("config: {}", layers.join(" < "))),
                Ok(Event::Measured { index, total, label, .. }) => self.progress = Some((index + 1, total * 2, format!("measuring {label}"))),
                Ok(Event::Fitted { summary }) => self.log.push(summary),
                Ok(Event::Developed { index, total, label, exposure_ev, clipped_fraction, preview, .. }) => {
                    if let Some(t) = preview {
                        let file = thumbs_dir(&job.dir).join(format!("{}.ppm", crate::safe_name(&label)));
                        if let Err(e) = write_ppm(&file, t.width, t.height, &t.rgb) {
                            log::warn!("{}: {e}", file.display());
                        }
                        if self.thumbs_of.as_ref() == Some(&job.dir) {
                            let img = egui::ColorImage::from_rgb([t.width, t.height], &t.rgb);
                            let tex = ctx.load_texture(format!("thumb-{}-{label}", job.dir.display()), img, egui::TextureOptions::LINEAR);
                            match self.thumbnails.iter_mut().find(|(l, _)| *l == label) {
                                Some(slot) => slot.1 = tex,
                                None => self.thumbnails.push((label.clone(), tex)),
                            }
                        }
                    }
                    self.progress = Some((total + index + 1, total * 2, format!("developed {label}")));
                    self.log.push(format!("{label}: EV {exposure_ev:+.2}, clipped {:.3}%", clipped_fraction * 100.0));
                }
                Ok(Event::Finished { frames, seconds }) => self.log.push(format!("developed {frames} frames in {seconds:.1} s")),
                Ok(Event::Delivered { message }) => self.log.push(message),
                Ok(Event::ScanChecked { label, lines_repaired, banding_repaired, warnings }) => {
                    if lines_repaired > 0 || banding_repaired {
                        self.log.push(format!(
                            "{label}: scanner fixes: {lines_repaired} line defects{}",
                            if banding_repaired { ", banding" } else { "" }
                        ));
                    }
                    self.log.extend(warnings.into_iter().map(|w| format!("{label}: {w}")));
                }
                Ok(Event::Healed { label, coverage_pct, defects, line_scratches, warnings, .. }) => {
                    self.log.push(format!("{label}: repaired {defects} defects ({coverage_pct:.2}%), {line_scratches} long scratches"));
                    self.log.extend(warnings.into_iter().map(|w| format!("{label}: {w}")));
                }
                Ok(other) => self.log.push(format!("{other:?}")),
                Err(e) => self.log.push(format!("error: {e}")),
            }
        }
        if job.thread.as_ref().is_some_and(|t| t.is_finished()) {
            let r = job.thread.take().map(|t| t.join().unwrap_or_else(|_| Err("the develop thread crashed".into())));
            self.outcome = r;
            if job.only.is_some()
                && let Some(v) = &mut self.viewer
            {
                v.reload_developed();
            }
            self.job = None;
            self.progress = None;
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    /// Load the selected roll's saved thumbnails (once per selection, not while it develops).
    fn load_thumbnails(&mut self, ctx: &egui::Context) {
        let Some(dir) = self.selected.clone() else { return };
        if self.thumbs_of.as_ref() == Some(&dir) || self.job_dir() == Some(dir.as_path()) {
            return;
        }
        self.thumbnails.clear();
        self.thumbs_of = Some(dir.clone());
        let Some(entry) = self.rolls.iter().find(|r| r.dir == dir) else { return };
        for f in &entry.manifest.frames {
            let file = thumbs_dir(&dir).join(format!("{}.ppm", crate::safe_name(&f.label)));
            if let Some((w, h, rgb)) = std::fs::read(&file).ok().and_then(|b| read_ppm(&b)) {
                let img = egui::ColorImage::from_rgb([w, h], &rgb);
                self.thumbnails
                    .push((f.label.clone(), ctx.load_texture(format!("thumb-{}-{}", dir.display(), f.label), img, egui::TextureOptions::LINEAR)));
            }
        }
    }

    /// Open frame `index` of the selected roll at full size.
    pub fn open_viewer(&mut self, index: usize, shared: &Shared) {
        let Some(dir) = self.selected.clone() else { return };
        let req = match self.request_for(&dir, shared) {
            Ok(r) => r,
            Err(e) => {
                self.log.push(format!("viewer: {e}"));
                return;
            }
        };
        // where the develop puts each frame; without it the viewer still shows the scans
        let developed = silvercraft_engine::developed_paths(&req).unwrap_or_else(|e| {
            self.log.push(format!("viewer: {e}"));
            Vec::new()
        });
        let frames = req
            .frames
            .iter()
            .map(|f| ViewFrame {
                label: f.label.clone(),
                scan: f.path.clone(),
                frame_box: f.frame_box.map(|b| (b, f.box_space.unwrap_or((0, 0)))),
                developed: developed.iter().find(|(l, _)| *l == f.label).map(|(_, p)| p.clone()),
                rotation: f.rotation,
                crop: f.crop,
            })
            .collect();
        self.viewer = Some(Viewer::new(frames, index));
    }

    /// Save the crop of the open viewer's frame `index` in the roll (`None`: back to its frame
    /// box) and queue the frame to develop again.
    pub fn set_crop(&mut self, index: usize, crop: Option<[f64; 4]>) {
        let (Some(dir), Some(v)) = (self.selected.clone(), &mut self.viewer) else { return };
        let Some(label) = v.frames.get(index).map(|f| f.label.clone()) else { return };
        let saved = RollManifest::load(&dir).and_then(|mut m| {
            let Some(f) = m.frames.iter_mut().find(|f| f.label == label) else {
                return Err(format!("frame {label} is no longer in the roll").into());
            };
            f.crop = crop;
            m.save(&dir)
        });
        match saved {
            Ok(_) => {
                v.frames[index].crop = crop;
                if let Some(e) = self.rolls.iter_mut().find(|r| r.dir == dir)
                    && let Some(f) = e.manifest.frames.iter_mut().find(|f| f.label == label)
                {
                    f.crop = crop;
                }
                self.sent = None;
                self.redevelop.push((dir, label));
            }
            Err(e) => self.outcome = Some(Err(format!("crop not saved: {e}"))),
        }
    }

    /// The top bar's status.
    pub fn status(&self) -> Status {
        let t = tokens();
        if self.sending() {
            return Status { color: t.caution, text: "Sending to LightCraft…".into(), progress: None };
        }
        if let Some((done, total, what)) = &self.progress {
            let name = self.job_dir().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            return Status { color: t.caution, text: format!("Developing {name} · {what}"), progress: Some(*done as f32 / (*total).max(1) as f32) };
        }
        match &self.outcome {
            Some(Ok(o)) => Status {
                color: t.ok,
                text: format!("{} frames developed{}", o.written.len(), if o.delivered.is_some() { " and sent" } else { "" }),
                progress: None,
            },
            Some(Err(e)) => Status { color: t.error, text: e.clone(), progress: None },
            None => Status { color: t.text_disabled, text: format!("{} rolls", self.rolls.len()), progress: None },
        }
    }

    fn entry(&self) -> Option<&RollEntry> {
        let dir = self.selected.as_ref()?;
        self.rolls.iter().find(|r| &r.dir == dir)
    }

    /// The right panel: develop settings and the result.
    pub fn panel(&mut self, ui: &mut egui::Ui, shared: &mut Shared) {
        let t = tokens();
        widgets::panel_title(ui, "Develop", 24.0);
        if self.entry().is_none() {
            widgets::padded(ui, |ui| {
                ui.label(RichText::new("Select a roll in the sidebar.").size(12.5).color(t.text_dim));
            });
            return;
        }
        section(ui, "develop", "Output", true, |ui| {
            ui.checkbox(&mut shared.settings.send_to_lightcraft, "Send to LightCraft");
            ui.checkbox(&mut shared.settings.half_float, "Half-float DNGs (half the size)");
        });
        section(ui, "overrides", "Pipeline Overrides", false, |ui| {
            ui.label(RichText::new("One key = value per line, e.g. heal.scratches = true").size(12.0).color(t.text_dim));
            ui.add(egui::TextEdit::multiline(&mut self.overrides).desired_rows(4).code_editor().desired_width(f32::INFINITY));
        });
        section(ui, "result", "Result", true, |ui| {
            if let Some((done, total, what)) = &self.progress {
                widgets::progress(ui, *done as f32 / (*total).max(1) as f32, what);
            }
            match &self.outcome {
                Some(Ok(o)) => {
                    widgets::status_line(ui, t.ok, &format!("{} frames written", o.written.len()));
                    ui.label(RichText::new(o.dir.display().to_string()).size(12.0).color(t.text_dim));
                    if let Some(d) = &o.delivered {
                        ui.label(RichText::new(d).size(12.5));
                    }
                }
                Some(Err(e)) => widgets::status_line(ui, t.error, e),
                None if self.progress.is_none() => {
                    ui.label(RichText::new("Not developed in this session.").size(12.5).color(t.text_dim));
                }
                None => {}
            }
            if let Some(m) = &self.sent {
                ui.label(RichText::new(m).size(12.5));
            }
        });
    }

    /// The Info panel: the roll's folder, stock and capture.
    pub fn info(&mut self, ui: &mut egui::Ui, shared: &Shared) {
        widgets::panel_title(ui, "Info", 24.0);
        let Some(e) = self.entry() else { return };
        let stock = shared.registry.get(&e.manifest.stock).map(|p| p.name.clone()).unwrap_or_else(|| format!("{} (unknown)", e.manifest.stock));
        widgets::padded(ui, |ui| {
            widgets::label_row(ui, "Roll", &e.manifest.roll);
            widgets::label_row(ui, "Stock", &stock);
            widgets::label_row(ui, "Frames", &e.manifest.frames.len().to_string());
            widgets::label_row(ui, "Captured by", &format!("{:?}", e.manifest.source));
            widgets::label_row(ui, "Folder", &e.dir.display().to_string());
        });
    }

    /// The bottom bar's actions.
    pub fn actions(&mut self, ui: &mut egui::Ui, shared: &Shared) {
        let has = self.entry().is_some();
        if self.running() {
            if widgets::pill_button(ui, "Cancel", true, false).clicked() {
                self.cancel();
            }
            return;
        }
        if self.sending() {
            widgets::pill_button(ui, "Sending…", false, false);
            return;
        }
        if widgets::pill_button(ui, "Send Again", has, false)
            .on_hover_text("Send the developed frames to LightCraft without developing again")
            .clicked()
        {
            self.send_again(shared);
        }
        if widgets::pill_button(ui, "Develop Roll", has, true).clicked() {
            self.develop(shared);
        }
    }

    /// The canvas: the selected roll's frames as a grid, or one at full size.
    pub fn canvas(&mut self, ui: &mut egui::Ui, shared: &Shared) {
        let t = tokens();
        self.load_thumbnails(ui.ctx());
        if let Some(v) = &mut self.viewer {
            let roll = self.selected.as_ref().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            v.poll(ui.ctx(), shared.settings.show_positive);
            let label = v.frame().map(|f| f.label.clone());
            let thumb = self.thumbnails.iter().find(|(l, _)| Some(l) == label.as_ref()).map(|(_, t)| t);
            let keys = v.keys(ui.ctx());
            match v.canvas(ui, &roll, thumb).or(keys) {
                Some(Request::Close) => self.viewer = None,
                Some(Request::Crop { index, crop }) => self.set_crop(index, crop),
                None => {}
            }
            return;
        }
        let rect = ui.max_rect();
        let Some(entry) = self.entry() else {
            let body = if self.rolls.is_empty() { "Scan or capture a roll to start" } else { "Select a roll in the sidebar" };
            widgets::empty_message(ui, rect, "No roll selected", body);
            return;
        };
        let stock = shared.registry.get(&entry.manifest.stock).map(|p| p.name.clone()).unwrap_or_else(|| entry.manifest.stock.clone());
        let labels: Vec<String> = entry.manifest.frames.iter().map(|f| f.label.clone()).collect();
        let developing = self.job_dir() == Some(entry.dir.as_path());
        let mut open = None;
        let title = entry.manifest.roll.clone();
        // header: roll name left, count right (LightCraft's "All Photos … 24 photos")
        let header = Rect::from_min_size(rect.min, vec2(rect.width(), 52.0));
        ui.painter().rect_filled(header, 0.0, t.canvas);
        ui.painter().text(pos2(header.left() + 24.0, header.center().y), Align2::LEFT_CENTER, &title, t.semibold(18.0), t.text);
        let g = ui.painter().layout_no_wrap(format!("{stock} · {} frames", labels.len()), t.font(13.0), t.text_dim);
        ui.painter().galley(pos2(header.right() - 20.0 - g.size().x, header.center().y - g.size().y / 2.0), g, t.text_dim);
        let body = Rect::from_min_max(pos2(rect.left() + 4.0, header.bottom() + 4.0), rect.max - vec2(4.0, 0.0));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(body));
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(&mut child, |ui| {
            let gap = 6.0;
            let target = 220.0;
            let w = ui.available_width();
            let per_row = ((w + gap) / (target + gap)).floor().max(1.0);
            let cell = ((w - gap * (per_row - 1.0)) / per_row).floor();
            ui.spacing_mut().item_spacing = vec2(gap, gap);
            ui.horizontal_wrapped(|ui| {
                for (i, label) in labels.iter().enumerate() {
                    let (r, resp) = ui.allocate_exact_size(vec2(cell, cell * 0.8), Sense::click());
                    let resp = resp.on_hover_text("Double-click to view full size");
                    if resp.double_clicked() {
                        open = Some(i);
                    }
                    ui.painter().rect_filled(r, 0.0, if resp.hovered() { t.cell_selected } else { t.cell });
                    let img_area = Rect::from_min_max(r.min + vec2(12.0, 26.0), r.max - vec2(12.0, 12.0));
                    ui.painter().text(r.min + vec2(10.0, 8.0), Align2::LEFT_TOP, label, t.font(12.0), t.text_label);
                    match self.thumbnails.iter().find(|(l, _)| l == label) {
                        Some((_, tex)) => {
                            let s = tex.size_vec2();
                            let k = (img_area.width() / s.x.max(1.0)).min(img_area.height() / s.y.max(1.0));
                            let ir = Rect::from_center_size(img_area.center(), s * k);
                            ui.painter().image(tex.id(), ir, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), egui::Color32::WHITE);
                        }
                        None => {
                            ui.painter().rect_stroke(img_area.shrink(8.0), 2.0, Stroke::new(1.0, t.field_border), StrokeKind::Inside);
                            let what = if developing { "developing…" } else { "not developed" };
                            ui.painter().text(img_area.center(), Align2::CENTER_CENTER, what, t.font(12.0), t.text_disabled);
                        }
                    }
                }
            });
        });
        if let Some(i) = open {
            self.open_viewer(i, shared);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ppm_round_trip_and_hostile_input() {
        let d = std::env::temp_dir().join(format!("sc-ppm-{}", std::process::id()));
        let p = d.join("01.ppm");
        let rgb: Vec<u8> = (0..2 * 3 * 3).map(|i| i as u8).collect();
        write_ppm(&p, 2, 3, &rgb).unwrap();
        assert_eq!(read_ppm(&std::fs::read(&p).unwrap()), Some((2, 3, rgb)));
        assert!(write_ppm(&p, 5, 5, &[0; 3]).is_err(), "size mismatch");
        for bad in
            [&b""[..], b"P6", b"P6\n2 3\n255\n\x00", b"P5\n1 1\n255\n\x00", b"P6\n0 1\n255\n", b"P6\n99999 1\n255\n", b"P6\n1 1\n65535\n\0\0\0"]
        {
            assert_eq!(read_ppm(bad), None, "{bad:?}");
        }
    }
}
