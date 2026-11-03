//! SilverCraft desktop app.
//!
//! `silvercraft [--simulate-scanner] [--prescan]`: `--simulate-scanner` uses the built-in V800
//! simulator instead of USB (to try the workflow without hardware); `--prescan` connects to the
//! scanner and prescans the bed on launch.

#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::sync::Arc;

use silvercraft_engine::hardware;
use silvercraft_film::profiles::ProfileRegistry;
use silvercraft_ui_egui::titlebar::Controls;
use silvercraft_ui_egui::{AppSettings, Services, SilverApp};

const APP_ID: &str = "silvercraft";

/// The window and taskbar icon, drawn here (no image asset): a silver disc with a film strip.
fn app_icon() -> egui::IconData {
    const N: usize = 128;
    let mut rgba = vec![0u8; N * N * 4];
    let c = (N as f32 - 1.0) / 2.0;
    for y in 0..N {
        for x in 0..N {
            let (dx, dy) = (x as f32 - c, y as f32 - c);
            let r = (dx * dx + dy * dy).sqrt();
            let i = (y * N + x) * 4;
            if r > 60.0 {
                continue;
            }
            // brushed silver, lighter towards the top left
            let shade = (200.0 - (dx + dy) * 0.6).clamp(120.0, 235.0) as u8;
            let mut px = [shade, shade, shade.saturating_add(6), 255];
            // a dark film strip across the middle with sprocket holes
            if dy.abs() < 22.0 && r < 56.0 {
                px = [34, 34, 36, 255];
                let hole = (dy.abs() > 14.0 && dy.abs() < 19.0) && ((x as i32 + 4) % 12) < 6;
                if hole {
                    px = [shade, shade, shade, 255];
                }
            }
            // anti-aliased rim
            let a = ((60.0 - r) * 2.0).clamp(0.0, 1.0);
            px[3] = (a * 255.0) as u8;
            rgba[i..i + 4].copy_from_slice(&px);
        }
    }
    egui::IconData { rgba, width: N as u32, height: N as u32 }
}

/// What `--prescan` still has to do.
#[derive(PartialEq)]
enum Launch {
    Done,
    Connect,
    Prescan,
}

struct App {
    app: SilverApp,
    launch: Launch,
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        match self.launch {
            Launch::Connect => {
                let services = self.app.services.clone();
                self.app.scanner.connect(&services, &self.app.shared, ui.ctx());
                self.launch = Launch::Prescan;
            }
            Launch::Prescan if self.app.scanner.error.is_some() => self.launch = Launch::Done,
            Launch::Prescan if self.app.scanner.connected && self.app.scanner.busy.is_none() => {
                self.app.scanner.prescan();
                self.launch = Launch::Done;
            }
            _ => {}
        }
        self.app.ui(ui);
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        silvercraft_ui_egui::titlebar::release_after_handover(ctx, raw_input);
    }
}

fn main() -> eframe::Result<()> {
    let simulate = std::env::args().any(|a| a == "--simulate-scanner");
    let launch = if std::env::args().any(|a| a == "--prescan") { Launch::Connect } else { Launch::Done };
    let settings_path = silvercraft_config::config_dir().map(|d| d.join("app.json"));
    let settings = settings_path.as_deref().map(AppSettings::load).unwrap_or_default();
    let registry = ProfileRegistry::with_user_dirs();
    for d in registry.diagnostics() {
        eprintln!("{d}");
    }
    // frame detection follows the user's config file ([detect]), like developing does
    let detect_cfg = match silvercraft_engine::ConfigSources::default().resolve(None, false) {
        Ok((r, _)) => r.config.detect,
        Err(diags) => {
            for d in diags {
                eprintln!("{d}");
            }
            silvercraft_detect::DetectConfig::default()
        }
    };
    let bed_cfg = detect_cfg.clone();
    let services = Services {
        scanner: if simulate { hardware::simulated_scanner(silvercraft_epson::SimScanner::default()) } else { hardware::epson_scanner() },
        camera: hardware::sony_camera(),
        frame_measure: Arc::new(move || silvercraft_engine::frames::live_measure(detect_cfg.clone())),
        detect: Some(Arc::new(move |p| silvercraft_engine::frames::bed_frames(p, &bed_cfg).0)),
        settings_path,
    };
    // the window as LightCraft opens its own: same size and limits; on macOS the native title bar
    // is hidden and the top bar takes its place
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("SilverCraft")
            .with_inner_size([1600.0, 1000.0])
            .with_min_inner_size([900.0, 560.0])
            .with_fullsize_content_view(true)
            .with_titlebar_shown(false)
            .with_title_shown(false)
            .with_icon(app_icon())
            // Linux: the app draws its own dark title bar (GNOME's frames follow the desktop theme)
            .with_decorations(!cfg!(target_os = "linux"))
            // Wayland matches the window to a desktop file by this id
            .with_app_id(APP_ID),
        ..Default::default()
    };
    eframe::run_native(
        "SilverCraft",
        options,
        Box::new(move |cc| {
            silvercraft_ui_egui::theme::apply(&cc.egui_ctx);
            let mut app = SilverApp::new(services, settings, registry);
            app.integrated_titlebar = cfg!(any(target_os = "macos", target_os = "linux"));
            if cfg!(target_os = "linux") {
                app.window_controls = Some(button_layout().map(|l| Controls::parse(&l)).unwrap_or_default());
            }
            Ok(Box::new(App { app, launch }))
        }),
    )
}

/// The desktop's window-button layout (GNOME's `button-layout`), if it has one.
fn button_layout() -> Option<String> {
    let out = std::process::Command::new("gsettings").args(["get", "org.gnome.desktop.wm.preferences", "button-layout"]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}
