//! The Scanner tab: connect to the flatbed, prescan the bed, detect and edit frames, scan them
//! (with infrared for dust repair) into a roll folder.

use std::path::PathBuf;
use std::sync::Arc;

use egui::{RichText, TextureHandle, TextureOptions};
use silvercraft_engine::manifest::ManifestFrame;
use silvercraft_engine::scanner::{ScanJob, ScannerCommand, ScannerEvent, ScannerWorker};
use silvercraft_epson::session::{Pass, Phase, PrescanOptions, Preview};

use crate::display::{Linear, Look};
use crate::frames::FrameEditor;
use crate::theme::tokens;
use crate::widgets::{self, section};
use crate::{Services, Shared, Status};

/// Common scan resolutions offered (the V800/V850 optical maximum is 6400).
pub const DPI_CHOICES: &[u32] = &[1200, 2400, 3200, 4800, 6400];

#[derive(Default)]
pub struct ScannerTab {
    worker: Option<ScannerWorker>,
    pub connected: bool,
    pub busy: Option<String>,
    progress: Option<(f32, String)>,
    pub preview: Option<Arc<Preview>>,
    preview_tex: Option<TextureHandle>,
    preview_look: Option<Look>,
    pub editor: FrameEditor,
    pub roll: String,
    pub scanned: Vec<ManifestFrame>,
    pub last_manifest: Option<PathBuf>,
    pub error: Option<String>,
    pub power_cycle: bool,
    pub notes: Vec<String>,
    /// Set when a scan finished and the roll should be developed (picked up by the app).
    pub finished_roll: Option<PathBuf>,
}

impl ScannerTab {
    pub fn with_roll(roll: String) -> Self {
        Self { roll, ..Default::default() }
    }

    pub fn connect(&mut self, services: &Services, shared: &Shared, ctx: &egui::Context) {
        self.error = None;
        self.power_cycle = false;
        let c = ctx.clone();
        let work = shared.settings.rolls_dir.join(".work");
        self.worker = Some((services.scanner)(work, Arc::new(move || c.request_repaint())));
    }

    pub fn disconnect(&mut self) {
        self.worker = None; // cancels and waits for the scanner to be idle
        self.connected = false;
        self.busy = None;
        self.progress = None;
    }

    pub fn send(&mut self, cmd: ScannerCommand) {
        if let Some(w) = &self.worker {
            w.send(cmd);
        }
    }

    pub fn prescan(&mut self) {
        self.send(ScannerCommand::Prescan(PrescanOptions::default()));
    }

    pub fn cancel(&self) {
        if let Some(w) = &self.worker {
            w.cancel();
        }
    }

    pub fn roll_dir(&self, shared: &Shared) -> PathBuf {
        shared.settings.rolls_dir.join(crate::safe_name(&self.roll))
    }

    /// Scan the frames in the editor.
    pub fn scan(&mut self, shared: &Shared) {
        let Some(p) = &self.preview else { return };
        let s = &shared.settings;
        let job = ScanJob {
            frames: self.editor.frames.clone(),
            preview_size: (p.image.width() as u32, p.image.height() as u32),
            preview_dpi: p.dpi.get(),
            dpi: s.scan_dpi,
            infrared: s.infrared,
            padding_mm: s.padding_mm,
            dir: self.roll_dir(shared),
            roll: self.roll.clone(),
            stock: shared.stock.clone(),
        };
        self.scanned.clear();
        self.send(ScannerCommand::Scan(job));
    }

    /// Apply worker events; call every frame.
    pub fn poll(&mut self, services: &Services, develop_after_capture: bool) {
        let Some(w) = &self.worker else { return };
        let events = w.poll();
        let alive = w.alive();
        for e in events {
            match e {
                ScannerEvent::Connected => {
                    self.connected = true;
                    self.notes.push("scanner connected".into());
                }
                ScannerEvent::Busy { what } => {
                    self.busy = Some(what);
                    self.progress = None;
                }
                ScannerEvent::Idle => {
                    self.busy = None;
                    self.progress = None;
                }
                ScannerEvent::Status { clean_idle, text } => self.notes.push(format!("status: {}{text}", if clean_idle { "idle · " } else { "" })),
                ScannerEvent::Recovered { clean_idle, text } => {
                    self.notes.push(format!("recovery: {text}{}", if clean_idle { " · idle" } else { "" }))
                }
                ScannerEvent::Phase(p) => self.progress = Some(phase_progress(&p)),
                ScannerEvent::Preview { preview, .. } => {
                    if let Some(detect) = &services.detect {
                        self.editor.set_frames(detect(&preview));
                        self.editor.renumber();
                    }
                    self.preview = Some(preview);
                    self.preview_tex = None;
                }
                ScannerEvent::FrameScanned { index, total, frame, manifest } => {
                    self.notes.push(format!("scanned {} ({}/{total})", frame.label, index + 1));
                    self.scanned.push(frame);
                    self.last_manifest = Some(manifest);
                }
                ScannerEvent::ScanFinished { manifest, frames } => {
                    self.notes.push(format!("scan finished: {frames} frames"));
                    if develop_after_capture {
                        self.finished_roll = manifest.parent().map(PathBuf::from);
                    }
                }
                ScannerEvent::Cancelled => self.notes.push("cancelled".into()),
                ScannerEvent::Failed { message, power_cycle } => {
                    self.error = Some(message);
                    self.power_cycle |= power_cycle;
                }
            }
        }
        if !alive && self.worker.is_some() {
            // The worker thread ended — before connecting (its Failed event says
            // why) or after (a crash). Either way the tab must not stay wedged in
            // "connected/busy" forever: drop back to the connect state.
            self.worker = None;
            let was_connected = self.connected;
            self.connected = false;
            self.busy = None;
            self.progress = None;
            if self.error.is_none() {
                self.error = Some(if was_connected {
                    "the scanner worker stopped; connect again".into()
                } else {
                    "the scanner worker stopped before connecting".into()
                });
            }
        }
    }

    /// No worker yet (not connected or connecting).
    pub fn worker_absent(&self) -> bool {
        self.worker.is_none()
    }

    /// The top bar's status.
    pub fn status(&self) -> Status {
        let t = tokens();
        if let Some(e) = &self.error {
            let text = if self.power_cycle { format!("{e} · switch the scanner off and on, then connect") } else { e.clone() };
            return Status { color: t.error, text, progress: None };
        }
        if let Some(b) = &self.busy {
            let (f, text) = self.progress.clone().unwrap_or((0.0, b.clone()));
            return Status { color: t.caution, text: format!("Scanner · {text}"), progress: Some(f) };
        }
        match (&self.worker, self.connected) {
            (None, _) => Status { color: t.text_disabled, text: "Scanner not connected".into(), progress: None },
            (Some(_), false) => Status { color: t.caution, text: "Connecting to the scanner…".into(), progress: None },
            (Some(_), true) if self.preview.is_none() => Status { color: t.ok, text: "Scanner ready · prescan the bed".into(), progress: None },
            (Some(_), true) => {
                let n = self.editor.frames.len();
                Status { color: t.ok, text: format!("Scanner ready · {n} frame{} on the bed", if n == 1 { "" } else { "s" }), progress: None }
            }
        }
    }

    /// The right panel: scanner, frames and scan settings.
    pub fn panel(&mut self, ui: &mut egui::Ui, services: &Services, shared: &mut Shared) {
        let t = tokens();
        widgets::panel_title(ui, "Scan", 24.0);
        section(ui, "scanner", "Scanner", true, |ui| {
            if let Some(e) = &self.error {
                widgets::status_line(ui, t.error, e);
                if self.power_cycle {
                    ui.label(RichText::new("Switch the scanner off and on, then connect again.").size(12.5).color(t.caution));
                }
            }
            match (&self.worker, self.connected) {
                (None, _) => widgets::status_line(ui, t.text_disabled, "Not connected"),
                (Some(_), false) => widgets::status_line(ui, t.caution, "Connecting…"),
                (Some(_), true) => widgets::status_line(ui, t.ok, "Epson Perfection V800/V850"),
            }
            let ready = self.connected && self.busy.is_none();
            ui.horizontal(|ui| {
                if self.worker.is_none() {
                    if widgets::text_button(ui, "Connect", true, true).clicked() {
                        self.connect(services, shared, ui.ctx());
                    }
                } else if widgets::text_button(ui, "Disconnect", self.busy.is_none(), false).clicked() {
                    self.disconnect();
                }
                if widgets::text_button(ui, "Status", ready, false).clicked() {
                    self.send(ScannerCommand::Status);
                }
                if widgets::text_button(ui, "Recover", ready, false).on_hover_text("Bring a confused scanner back to idle").clicked() {
                    self.send(ScannerCommand::Recover);
                }
            });
            if let Some(b) = &self.busy {
                let (f, text) = self.progress.clone().unwrap_or((0.0, b.clone()));
                widgets::progress(ui, f, &text);
            }
        });
        section(ui, "frames", "Frames", true, |ui| {
            if self.preview.is_none() {
                ui.label(RichText::new("Prescan the bed to find the frames.").size(12.5).color(t.text_dim));
                return;
            }
            widgets::label_row(ui, "Frames", &self.editor.frames.len().to_string());
            ui.label(RichText::new("Drag on the film to add a frame, click to select, drag to move, Delete to remove.").size(12.0).color(t.text_dim));
            ui.horizontal_wrapped(|ui| {
                if let (Some(detect), Some(p)) = (&services.detect, &self.preview)
                    && widgets::text_button(ui, "Detect", true, false).on_hover_text("Find the frames again").clicked()
                {
                    self.editor.set_frames(detect(p));
                    self.editor.renumber();
                }
                if widgets::text_button(ui, "Renumber", true, false).on_hover_text("Strip by strip, top to bottom").clicked() {
                    self.editor.renumber();
                }
                if widgets::text_button(ui, "Delete", self.editor.selected.is_some(), false).clicked() {
                    self.editor.delete_selected();
                }
                if widgets::text_button(ui, "Clear", !self.editor.frames.is_empty(), false).clicked() {
                    self.editor.set_frames(Vec::new());
                }
            });
        });
        section(ui, "scan", "Scan", true, |ui| {
            crate::roll_fields(ui, &mut self.roll, shared);
            let s = &mut shared.settings;
            let dpis: Vec<String> = DPI_CHOICES.iter().map(|d| format!("{d} dpi")).collect();
            if let Some(i) = widgets::choice_row(ui, "Resolution", &format!("{} dpi", s.scan_dpi), &dpis) {
                s.scan_dpi = DPI_CHOICES.get(i).copied().unwrap_or(s.scan_dpi);
            }
            ui.checkbox(&mut s.infrared, "Infrared pass (dust repair)");
            ui.checkbox(&mut s.develop_after_capture, "Develop when the scan finishes");
        });
        if !self.scanned.is_empty() {
            section(ui, "scanned", "Scanned", true, |ui| {
                for f in &self.scanned {
                    let name = f.capture.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    widgets::label_row(ui, &f.label, &name);
                }
                if let Some(m) = &self.last_manifest
                    && self.busy.is_none()
                    && widgets::text_button(ui, "Develop This Roll", true, false).clicked()
                {
                    self.finished_roll = m.parent().map(PathBuf::from);
                }
            });
        }
    }

    /// The Info panel: where scans go.
    pub fn info(&mut self, ui: &mut egui::Ui, shared: &Shared) {
        widgets::panel_title(ui, "Info", 24.0);
        widgets::padded(ui, |ui| {
            widgets::label_row(ui, "Scanner", if self.connected { "Epson V800/V850" } else { "not connected" });
            widgets::label_row(ui, "Roll folder", &self.roll_dir(shared).display().to_string());
            if let Some(p) = &self.preview {
                widgets::label_row(ui, "Prescan", &format!("{} × {} px at {} dpi", p.image.width(), p.image.height(), p.dpi.get()));
            }
            widgets::label_row(ui, "Scan", &format!("{} dpi{}", shared.settings.scan_dpi, if shared.settings.infrared { " + infrared" } else { "" }));
        });
    }

    /// The bottom bar's actions.
    pub fn actions(&mut self, ui: &mut egui::Ui, services: &Services, shared: &Shared) {
        let ready = self.connected && self.busy.is_none();
        if self.worker.is_none() {
            if widgets::pill_button(ui, "Connect Scanner", true, true).clicked() {
                self.connect(services, shared, ui.ctx());
            }
            return;
        }
        if self.busy.is_some() {
            if widgets::pill_button(ui, "Cancel", true, false).clicked() {
                self.cancel();
            }
            return;
        }
        let prescan_primary = self.preview.is_none();
        if widgets::pill_button(ui, if self.preview.is_none() { "Prescan Bed" } else { "Prescan Again" }, ready, prescan_primary).clicked() {
            self.prescan();
        }
        let n = self.editor.frames.len();
        let can_scan = ready && self.preview.is_some() && n > 0 && !self.roll.trim().is_empty();
        let label = if n == 0 { "Scan Frames".to_string() } else { format!("Scan {n} Frame{}", if n == 1 { "" } else { "s" }) };
        if widgets::pill_button(ui, &label, can_scan, !prescan_primary).clicked() {
            self.scan(shared);
        }
    }

    /// The canvas: the bed preview with the frames over it.
    pub fn canvas(&mut self, ui: &mut egui::Ui, shared: &Shared) {
        let rect = ui.max_rect();
        let Some(p) = self.preview.clone() else {
            let (title, body) = if self.connected {
                ("No prescan yet", "Prescan the bed to see the film and find the frames")
            } else {
                ("Flatbed scanner", "Connect the Epson V800/V850 to start")
            };
            widgets::empty_message(ui, rect, title, body);
            return;
        };
        let look = if shared.settings.show_positive { Look::Positive } else { Look::AsIs };
        if self.preview_tex.is_none() || self.preview_look != Some(look) {
            if let Some(lin) = Linear::from_rgb16(p.image.width(), p.image.height(), p.image.data(), 2048) {
                self.preview_tex = Some(ui.ctx().load_texture("bed-preview", lin.render(look), TextureOptions::LINEAR));
            }
            self.preview_look = Some(look);
        }
        let size = (p.image.width() as u32, p.image.height() as u32);
        if let Some(tex) = self.preview_tex.clone() {
            let inner = rect.shrink(24.0);
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(egui::Layout::top_down(egui::Align::Center)));
            self.editor.show(&mut child, &tex, size);
        }
    }
}

/// Progress fraction and label for a driver phase.
pub fn phase_progress(p: &Phase) -> (f32, String) {
    let pass = |p: &Pass| match p {
        Pass::Visible => "visible",
        Pass::Infrared => "infrared",
    };
    match p {
        Phase::Preflight => (0.0, "checking the scanner".into()),
        Phase::Recovering => (0.0, "recovering".into()),
        Phase::Priming { cycle, of } => (*cycle as f32 / (*of).max(1) as f32 * 0.1, format!("priming {cycle}/{of}")),
        Phase::Setup { pass: p, tile, tiles } => (0.1, format!("{} pass · tile {}/{tiles}", pass(p), tile + 1)),
        Phase::WarmingUp { pass: p, poll, .. } => (0.1, format!("{} lamp warming up ({poll})", pass(p))),
        Phase::Reading { pass: p, bytes_done, bytes_total, tile, tiles } => {
            let f = if *bytes_total > 0 { *bytes_done as f32 / *bytes_total as f32 } else { 0.0 };
            (f, format!("{} pass · {:.0}% · tile {}/{tiles}", pass(p), f * 100.0, tile + 1))
        }
        Phase::Handshake { .. } => (0.5, "arming infrared".into()),
        Phase::Postflight => (1.0, "finishing".into()),
        Phase::Assembling => (1.0, "assembling".into()),
        Phase::Stitching => (1.0, "stitching".into()),
        Phase::Registering => (1.0, "registering infrared".into()),
    }
}
