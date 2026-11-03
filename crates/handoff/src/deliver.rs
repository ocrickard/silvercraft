//! Getting finished frames into LightCraft, best route first:
//!
//! 1. the control channel of a running LightCraft (roll album, keywords, stock preset, opens the
//!    first frame in Develop);
//! 2. LightCraft's watched folder, when one is configured (metadata travels in the XMP);
//! 3. leaving the files where they are for File → Import (the XMP still carries everything).
//!
//! The library is never written directly: LightCraft holds an exclusive lock on it while open.

use std::path::{Path, PathBuf};

use crate::config::Route;
use crate::lightcraft::{self, Client, ImportRequest, LookPreset};
use crate::write::write_atomic;
use crate::{HandoffError, Result};

#[derive(Clone, Debug)]
pub struct DeliveryOptions {
    pub route: Route,
    pub port: u16,
    /// LightCraft library folder, for finding its watched folder.
    pub library: Option<PathBuf>,
    pub album: Option<String>,
    pub keywords: Vec<String>,
    pub preset: Option<String>,
    /// The stock's look, installed and applied when `preset` is unset (control channel only).
    pub look: Option<LookPreset>,
    /// Copy into LightCraft's library folders instead of adding in place (control channel only).
    pub copy: bool,
    /// Open the first delivered frame in LightCraft's Develop view.
    pub reveal: bool,
}

impl Default for DeliveryOptions {
    fn default() -> Self {
        Self {
            route: Route::Auto,
            port: lightcraft::DEFAULT_PORT,
            library: lightcraft::default_library(),
            album: None,
            keywords: vec![],
            preset: None,
            look: None,
            copy: false,
            reveal: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delivered {
    /// Imported over the control channel; photo ids, and the album when one was used.
    Imported { ids: Vec<u64>, album: Option<u64> },
    /// Copied into the watched folder; LightCraft picks them up within a few seconds.
    WatchedFolder(PathBuf),
    /// Nothing to talk to: the files are ready to import by hand.
    LeftInPlace { reason: String },
}

/// Deliver `dngs` (each with its `<stem>.xmp` beside it).
pub fn deliver(dngs: &[PathBuf], opts: &DeliveryOptions) -> Result<Delivered> {
    if dngs.is_empty() {
        return Err(HandoffError::Invalid("nothing to deliver".into()));
    }
    if opts.route == Route::None {
        return Ok(Delivered::LeftInPlace { reason: "handoff route is `none`".into() });
    }
    let control_err = if opts.route == Route::WatchedFolder {
        "the handoff route skips the control channel".to_string()
    } else {
        match Client::connect(opts.port) {
            Ok(mut c) => return via_control(&mut c, dngs, opts),
            Err(e) if opts.route == Route::Control => return Err(e),
            Err(e) => e.to_string(),
        }
    };
    if let Some(folder) = opts.library.as_deref().and_then(lightcraft::auto_import_folder)
        && folder.is_dir()
    {
        for dng in dngs {
            copy_with_sidecar(dng, &folder)?;
        }
        return Ok(Delivered::WatchedFolder(folder));
    }
    if opts.route == Route::WatchedFolder {
        return Err(HandoffError::Unavailable("LightCraft has no watched folder set (Settings ▸ Import ▸ Auto Import)".into()));
    }
    Ok(Delivered::LeftInPlace {
        reason: format!("{control_err}; start LightCraft with --control {} (or set a watched folder) to send frames automatically", opts.port),
    })
}

fn via_control(c: &mut Client, dngs: &[PathBuf], opts: &DeliveryOptions) -> Result<Delivered> {
    let album = opts.album.as_deref().map(|name| c.ensure_album(name)).transpose()?;
    let preset = match (&opts.preset, &opts.look) {
        (Some(p), _) => Some(p.clone()),
        (None, Some(look)) => match c.install_preset(look) {
            Ok(id) => Some(id),
            Err(e) => {
                // a missing look must not stop the roll from arriving
                log::warn!("stock look not installed: {e}");
                None
            }
        },
        (None, None) => None,
    };
    let mut ids = Vec::new();
    // one frame per request: imports run on LightCraft's UI thread under a 60 s timeout
    for dng in dngs {
        let req = ImportRequest { paths: vec![dng.clone()], copy: opts.copy, album, keywords: opts.keywords.clone(), preset: preset.clone() };
        ids.extend(c.import(&req)?);
    }
    if opts.reveal
        && let Some(&first) = ids.first()
    {
        c.reveal(first)?;
    }
    Ok(Delivered::Imported { ids, album })
}

/// The sidecar goes first so it is there when the watcher sees the DNG.
fn copy_with_sidecar(dng: &Path, folder: &Path) -> Result<()> {
    let name = dng.file_name().ok_or_else(|| HandoffError::Invalid(format!("{} has no file name", dng.display())))?;
    let xmp = dng.with_extension("xmp");
    if xmp.is_file()
        && let Some(xname) = xmp.file_name()
    {
        write_atomic(&folder.join(xname), &std::fs::read(&xmp)?)?;
    }
    write_atomic(&folder.join(name), &std::fs::read(dng)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    #[test]
    fn falls_back_to_the_watched_folder_then_to_leaving_files() {
        let root = std::env::temp_dir().join(format!("sc-deliver-{}", std::process::id()));
        let (lib, watch, roll) = (root.join("lib"), root.join("watch"), root.join("roll"));
        for d in [&lib, &watch, &roll] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(roll.join("f1.dng"), b"dng").unwrap();
        std::fs::write(roll.join("f1.xmp"), b"xmp").unwrap();
        let dngs = vec![roll.join("f1.dng")];
        let mut opts = DeliveryOptions { port: free_port(), library: Some(lib.clone()), ..Default::default() };

        let left = deliver(&dngs, &opts).unwrap();
        assert!(matches!(left, Delivered::LeftInPlace { .. }), "{left:?}");

        std::fs::write(lib.join("prefs.json"), format!(r#"{{"import":{{"autoFolder":{:?}}}}}"#, watch.to_string_lossy())).unwrap();
        assert_eq!(deliver(&dngs, &opts).unwrap(), Delivered::WatchedFolder(watch.clone()));
        assert_eq!(std::fs::read(watch.join("f1.dng")).unwrap(), b"dng");
        assert_eq!(std::fs::read(watch.join("f1.xmp")).unwrap(), b"xmp");

        opts.route = Route::Control;
        assert!(matches!(deliver(&dngs, &opts), Err(HandoffError::Unavailable(_))));
        opts.route = Route::None;
        assert!(matches!(deliver(&dngs, &opts).unwrap(), Delivered::LeftInPlace { .. }));
        opts.route = Route::WatchedFolder;
        opts.library = None;
        assert!(deliver(&dngs, &opts).is_err());
        opts.route = Route::Auto;
        assert!(matches!(deliver(&dngs, &opts).unwrap(), Delivered::LeftInPlace { .. }));
        assert!(deliver(&[], &opts).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
