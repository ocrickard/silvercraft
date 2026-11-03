//! A roll delivered to a real LightCraft over its control channel: album, keywords, the stock's
//! look preset installed and applied. Needs a LightCraft build and a display, so it is ignored by
//! default:
//!
//! ```sh
//! SILVERCRAFT_LIGHTCRAFT_BIN=$PWD/../lightcraft/target/release/lightcraft \
//!     cargo test -p silvercraft-engine --test lightcraft_live -- --ignored
//! ```

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use silvercraft_engine::{ConfigSources, FrameSource, RollRequest, develop_roll};

struct LightCraft {
    child: Child,
    port: u16,
}

impl LightCraft {
    fn start(bin: &str, dir: &std::path::Path) -> Self {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let child = Command::new(bin)
            .args(["--memory", "--control", &port.to_string()])
            .env("HOME", dir)
            .env("XDG_CONFIG_HOME", dir.join(".config"))
            .spawn()
            .unwrap();
        let end = Instant::now() + Duration::from_secs(60);
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < end, "LightCraft did not open its control port");
            std::thread::sleep(Duration::from_millis(200));
        }
        Self { child, port }
    }

    fn execute(&self, command: &str, params: Value) -> Value {
        let s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        let mut w = s.try_clone().unwrap();
        writeln!(w, "{}", json!({"id": 1, "method": "engine.execute", "params": {"command": command, "params": params}})).unwrap();
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["ok"], true, "{command}: {v}");
        v["result"].clone()
    }
}

impl Drop for LightCraft {
    fn drop(&mut self) {
        if TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
            let _ = std::panic::catch_unwind(|| self.execute("app.quit", json!({})));
        }
        let end = Instant::now() + Duration::from_secs(20);
        while Instant::now() < end {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let _ = self.child.kill();
    }
}

fn write_negative(path: &std::path::Path) {
    let (w, h) = (96u32, 64u32);
    let mut data = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let scene = 0.1 + 0.8 * (x as f32 / w as f32) * (0.4 + 0.6 * y as f32 / h as f32);
            for b in [0.62f32, 0.55, 0.66] {
                data.push((b * 10f32.powf(-1.1 * scene) * 65535.0) as u16);
            }
        }
    }
    let f = std::fs::File::create(path).unwrap();
    tiff::encoder::TiffEncoder::new(f).unwrap().write_image::<tiff::encoder::colortype::RGB16>(w, h, &data).unwrap();
}

#[test]
#[ignore = "needs SILVERCRAFT_LIGHTCRAFT_BIN and a display"]
fn roll_arrives_in_lightcraft_with_album_keywords_and_stock_look() {
    let Ok(bin) = std::env::var("SILVERCRAFT_LIGHTCRAFT_BIN") else {
        eprintln!("SILVERCRAFT_LIGHTCRAFT_BIN not set; skipping");
        return;
    };
    let dir: PathBuf = std::env::temp_dir().join(format!("sc-lc-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let lc = LightCraft::start(&bin, &dir);
    let frame = dir.join("f1.tif");
    write_negative(&frame);
    let req = RollRequest {
        roll: "Live Roll".into(),
        stock: "ilford-xp2-super".into(),
        frames: vec![FrameSource::whole(&frame)],
        sources: ConfigSources {
            no_user: true,
            sets: vec![
                format!("output.dir={:?}", dir.join("out").to_string_lossy()),
                "handoff.send=true".into(),
                "handoff.route=\"control\"".into(),
                format!("handoff.port={}", lc.port),
            ],
            ..Default::default()
        },
    };
    let out = develop_roll(&req, &AtomicBool::new(false), &mut |_| {}).unwrap();
    assert!(out.delivered.unwrap().contains("imported 1"));

    let presets = lc.execute("presets.list", json!({}));
    assert!(presets.as_array().unwrap().iter().any(|p| p["id"] == "silvercraft.ilford-xp2-super"), "{presets}");
    let albums = lc.execute("albums.list", json!({}));
    let album = albums.as_array().unwrap().iter().find(|a| a["name"] == "Live Roll").expect("roll album");
    assert_eq!(album["count"], 1);
    let id = lc.execute("library.state", json!({}))["selection"]["active"].as_u64().expect("the frame is revealed");
    let dev = lc.execute("develop.get", json!({"id": id}));
    assert_eq!(dev["treatment"], "bw", "the XP2 look (black and white) is applied");
    let kw = lc.execute("keyword.list", json!({}));
    assert!(kw.to_string().contains("film:Ilford XP2 Super"), "{kw}");
}
