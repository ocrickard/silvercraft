//! Talking to a running LightCraft over its control channel (`lightcraft --control <port>`): JSON
//! lines on 127.0.0.1, one request per line, replies in order (LightCraft `docs/control-protocol.md`).
//!
//! The channel answers on LightCraft's UI thread and `library.import` runs synchronously there, so
//! frames go one request at a time. `albumName` makes a new album on every call, so the roll album
//! is resolved by name here (`albums.list`, then `album.create`).

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::{HandoffError, Result};

pub const DEFAULT_PORT: u16 = 7980;
/// LightCraft gives up on a request after 60 s; wait a little longer than that.
const READ_TIMEOUT: Duration = Duration::from_secs(75);
/// Longest reply line accepted (a photo list for a big library is the largest thing we ask for).
const MAX_REPLY: usize = 64 << 20;

pub struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    next_id: u64,
}

impl Client {
    pub fn connect(port: u16) -> Result<Self> {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
            .map_err(|e| HandoffError::Unavailable(format!("LightCraft control port {port}: {e}")))?;
        stream.set_read_timeout(Some(READ_TIMEOUT))?;
        stream.set_nodelay(true)?;
        Ok(Self { reader: BufReader::new(stream.try_clone()?), writer: stream, next_id: 1 })
    }

    /// One request; returns `result` or the channel's error text.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = serde_json::to_string(&json!({"id": id, "method": method, "params": params}))?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        let mut reply = String::new();
        let n = std::io::Read::take(&mut self.reader, MAX_REPLY as u64).read_line(&mut reply)?;
        if n == 0 {
            return Err(HandoffError::Unavailable("LightCraft closed the control connection".into()));
        }
        if !reply.ends_with('\n') {
            return Err(HandoffError::Protocol("reply too long or truncated".into()));
        }
        let v: Value = serde_json::from_str(&reply)?;
        parse_reply(&v, id)
    }

    pub fn execute(&mut self, command: &str, params: Value) -> Result<Value> {
        self.call("engine.execute", json!({"command": command, "params": params}))
    }

    /// The id of the top-level regular album called `name`, created when missing.
    pub fn ensure_album(&mut self, name: &str) -> Result<u64> {
        let albums = self.execute("albums.list", json!({}))?;
        if let Some(id) = find_album(&albums, name) {
            return Ok(id);
        }
        let r = self.execute("album.create", json!({"name": name}))?;
        r["id"].as_u64().ok_or_else(|| HandoffError::Protocol(format!("album.create returned {r}")))
    }

    /// Import one or more files; returns the new (or restored) photo ids.
    pub fn import(&mut self, req: &ImportRequest) -> Result<Vec<u64>> {
        let mut params = json!({
            "paths": req.paths.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
            "mode": if req.copy { "copy" } else { "add" },
        });
        if let Some(a) = req.album {
            params["album"] = json!(a);
        }
        if !req.keywords.is_empty() {
            params["keywords"] = json!(req.keywords);
        }
        if let Some(p) = &req.preset {
            params["preset"] = json!(p);
        }
        let r = self.execute("library.import", params)?;
        let failed = r["failed"].as_array().map(Vec::len).unwrap_or(0);
        let ids = imported_ids(&r);
        if ids.is_empty() && failed > 0 {
            return Err(HandoffError::Rejected(format!("LightCraft could not import: {}", r["failed"])));
        }
        Ok(ids)
    }

    /// Install `look` as a LightCraft preset and return its id. A preset with the same id is
    /// replaced (SilverCraft owns its stock presets), so a changed stock profile updates its look.
    pub fn install_preset(&mut self, look: &LookPreset) -> Result<String> {
        let list = self.execute("presets.list", json!({}))?;
        if list.as_array().into_iter().flatten().any(|p| p["id"].as_str() == Some(look.id.as_str())) {
            self.execute("preset.delete", json!({"id": look.id}))?;
        }
        let r = self.execute("preset.import", json!({"paths": [look.file.to_string_lossy()]}))?;
        if let Some(id) = r["imported"].as_array().and_then(|a| a.first()).and_then(|p| p["id"].as_str()) {
            return Ok(id.to_string());
        }
        // skipped: an identical preset (same name, group and settings) already exists
        let list = self.execute("presets.list", json!({}))?;
        list.as_array()
            .into_iter()
            .flatten()
            .find(|p| p["name"].as_str() == Some(look.name.as_str()) && p["group"].as_str() == Some(look.group.as_str()))
            .and_then(|p| p["id"].as_str())
            .map(str::to_string)
            .ok_or_else(|| HandoffError::Rejected(format!("LightCraft did not import the look preset: {}", r["failed"])))
    }

    /// Show `id` in LightCraft's Develop view.
    pub fn reveal(&mut self, id: u64) -> Result<()> {
        self.execute("library.select", json!({"ids": [id], "active": id}))?;
        self.execute("view.detail", json!({}))?;
        self.call("ui.set", json!({"right": "edit"}))?;
        Ok(())
    }
}

/// A LightCraft preset file SilverCraft installs (one per film stock).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LookPreset {
    pub id: String,
    pub name: String,
    pub group: String,
    /// The `.lcpreset` file (LightCraft reads it itself).
    pub file: PathBuf,
}

#[derive(Clone, Debug, Default)]
pub struct ImportRequest {
    pub paths: Vec<PathBuf>,
    /// Copy into LightCraft's library folder structure instead of adding in place.
    pub copy: bool,
    pub album: Option<u64>,
    pub keywords: Vec<String>,
    /// A LightCraft preset id applied on import (one per stock: `silvercraft.<stock>`).
    pub preset: Option<String>,
}

fn parse_reply(v: &Value, id: u64) -> Result<Value> {
    if v["id"].as_u64() != Some(id) {
        return Err(HandoffError::Protocol(format!("reply for request {} while waiting for {id}", v["id"])));
    }
    if v["ok"].as_bool() == Some(true) {
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    } else {
        Err(HandoffError::Rejected(v["error"].as_str().unwrap_or("unknown error").to_string()))
    }
}

fn find_album(albums: &Value, name: &str) -> Option<u64> {
    albums
        .as_array()?
        .iter()
        .find(|a| a["name"].as_str() == Some(name) && a["folder"].as_bool() != Some(true) && a["smart"].as_bool() != Some(true))?["id"]
        .as_u64()
}

fn imported_ids(r: &Value) -> Vec<u64> {
    ["imported", "restored"].iter().flat_map(|k| r[*k].as_array().into_iter().flatten().filter_map(Value::as_u64)).collect()
}

/// The watched folder LightCraft's library auto-imports from, read from the library's `prefs.json`
/// (read-only; LightCraft rewrites that file from memory).
pub fn auto_import_folder(library: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(library.join("prefs.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let folder = v.pointer("/import/autoFolder")?.as_str()?;
    (!folder.is_empty()).then(|| PathBuf::from(folder))
}

/// LightCraft's library folder: `$LIGHTCRAFT_LIBRARY`, else `~/Pictures/LightCraft Library`.
pub fn default_library() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("LIGHTCRAFT_LIBRARY") {
        return Some(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join("Pictures").join("LightCraft Library"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn replies_are_matched_and_errors_surface() {
        assert_eq!(parse_reply(&json!({"id": 3, "ok": true, "result": {"a": 1}}), 3).unwrap(), json!({"a": 1}));
        assert_eq!(parse_reply(&json!({"id": 3, "ok": true}), 3).unwrap(), Value::Null);
        assert!(matches!(parse_reply(&json!({"id": 3, "ok": false, "error": "nope"}), 3), Err(HandoffError::Rejected(e)) if e == "nope"));
        assert!(matches!(parse_reply(&json!({"id": 2, "ok": true}), 3), Err(HandoffError::Protocol(_))));
    }

    #[test]
    fn albums_resolve_by_name_skipping_folders_and_smart_albums() {
        let list = json!([
            {"id": 1, "name": "Roll 042", "folder": true},
            {"id": 2, "name": "Roll 042", "folder": false, "smart": true},
            {"id": 3, "name": "Roll 042", "folder": false},
        ]);
        assert_eq!(find_album(&list, "Roll 042"), Some(3));
        assert_eq!(find_album(&list, "Roll 043"), None);
        assert_eq!(find_album(&json!(null), "x"), None);
    }

    /// A fake LightCraft that answers the requests a roll delivery makes.
    #[test]
    fn delivery_sequence_against_a_fake_control_port() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut w = s;
            let mut seen = Vec::new();
            let mut line = String::new();
            while r.read_line(&mut line).unwrap() > 0 {
                let req: Value = serde_json::from_str(&line).unwrap();
                let cmd = req["params"]["command"].as_str().unwrap_or(req["method"].as_str().unwrap()).to_string();
                let result = match cmd.as_str() {
                    "albums.list" => json!([{"id": 9, "name": "Other", "folder": false}]),
                    "album.create" => json!({"id": 10}),
                    "library.import" => {
                        assert_eq!(req["params"]["params"]["album"], json!(10));
                        assert_eq!(req["params"]["params"]["keywords"], json!(["roll:042"]));
                        json!({"imported": [77], "duplicates": [], "failed": []})
                    }
                    _ => Value::Null,
                };
                seen.push(cmd);
                writeln!(w, "{}", json!({"id": req["id"], "ok": true, "result": result})).unwrap();
                line.clear();
            }
            seen
        });
        let mut c = Client::connect(port).unwrap();
        let album = c.ensure_album("Roll 042").unwrap();
        assert_eq!(album, 10);
        let ids = c
            .import(&ImportRequest { paths: vec!["/tmp/a.dng".into()], album: Some(album), keywords: vec!["roll:042".into()], ..Default::default() })
            .unwrap();
        assert_eq!(ids, vec![77]);
        c.reveal(77).unwrap();
        drop(c);
        let seen = server.join().unwrap();
        assert_eq!(seen, ["albums.list", "album.create", "library.import", "library.select", "view.detail", "ui.set"]);
    }

    /// One fake-LightCraft connection answering with `answer(command, params)`; returns the
    /// commands seen.
    fn fake(answer: impl Fn(&str, &Value) -> Value + Send + 'static) -> (u16, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut w = s;
            let mut seen = Vec::new();
            let mut line = String::new();
            while r.read_line(&mut line).unwrap() > 0 {
                let req: Value = serde_json::from_str(&line).unwrap();
                let cmd = req["params"]["command"].as_str().unwrap_or(req["method"].as_str().unwrap()).to_string();
                let result = answer(&cmd, &req["params"]["params"]);
                seen.push(cmd);
                writeln!(w, "{}", json!({"id": req["id"], "ok": true, "result": result})).unwrap();
                line.clear();
            }
            seen
        });
        (port, h)
    }

    fn look() -> LookPreset {
        LookPreset {
            id: "silvercraft.ilford-xp2-super".into(),
            name: "Ilford XP2 Super".into(),
            group: "SilverCraft Film Stocks".into(),
            file: "/r/x.lcpreset".into(),
        }
    }

    #[test]
    fn install_preset_replaces_our_previous_version() {
        let (port, h) = fake(|cmd, p| match cmd {
            "presets.list" => json!([{"id": "silvercraft.ilford-xp2-super", "name": "Ilford XP2 Super", "group": "SilverCraft Film Stocks"}]),
            "preset.import" => {
                assert_eq!(p["paths"], json!(["/r/x.lcpreset"]));
                json!({"imported": [{"id": "silvercraft.ilford-xp2-super"}], "skipped": 0, "failed": []})
            }
            _ => Value::Null,
        });
        let mut c = Client::connect(port).unwrap();
        assert_eq!(c.install_preset(&look()).unwrap(), "silvercraft.ilford-xp2-super");
        drop(c);
        assert_eq!(h.join().unwrap(), ["presets.list", "preset.delete", "preset.import"]);
    }

    #[test]
    fn install_preset_finds_an_identical_existing_preset() {
        let (port, h) = fake(|cmd, _| match cmd {
            "presets.list" => {
                json!([{"id": "user.silvercraft-film-stocks-ilford-xp2-super", "name": "Ilford XP2 Super", "group": "SilverCraft Film Stocks"}])
            }
            "preset.import" => json!({"imported": [], "skipped": 1, "failed": []}),
            _ => Value::Null,
        });
        let mut c = Client::connect(port).unwrap();
        assert_eq!(c.install_preset(&look()).unwrap(), "user.silvercraft-film-stocks-ilford-xp2-super");
        drop(c);
        assert_eq!(h.join().unwrap(), ["presets.list", "preset.import", "presets.list"]);
    }

    #[test]
    fn closed_port_is_unavailable() {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert!(matches!(Client::connect(port), Err(HandoffError::Unavailable(_))));
    }

    #[test]
    fn auto_import_folder_is_read_from_prefs() {
        let dir = std::env::temp_dir().join(format!("sc-prefs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(auto_import_folder(&dir), None);
        std::fs::write(dir.join("prefs.json"), r#"{"import":{"autoFolder":"/scans/in"}}"#).unwrap();
        assert_eq!(auto_import_folder(&dir), Some(PathBuf::from("/scans/in")));
        std::fs::write(dir.join("prefs.json"), "not json").unwrap();
        assert_eq!(auto_import_folder(&dir), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
