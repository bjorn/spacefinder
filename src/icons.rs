use rustc_hash::FxHashMap;
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use std::cell::RefCell;
use std::path::{Path, PathBuf};

/// Icon cache. Resolves MIME/icon names via the freedesktop icon theme
/// (`freedesktop-icons`) and the `xdg-mime` crate for MIME detection.
pub struct Icons {
    mime_db: xdg_mime::SharedMimeInfo,
    cache: RefCell<FxHashMap<String, Image>>,
    folder: Image,
    file: Image,
    trash: Image,
    home: Image,
    drive: Image,
}

impl Icons {
    pub fn new() -> Self {
        Self {
            mime_db: xdg_mime::SharedMimeInfo::new(),
            cache: RefCell::default(),
            folder: load_builtin(include_bytes!("../ui/icons/folder.svg")),
            file: load_builtin(include_bytes!("../ui/icons/file.svg")),
            trash: load_builtin(include_bytes!("../ui/icons/trash.svg")),
            home: load_builtin(include_bytes!("../ui/icons/home.svg")),
            drive: load_builtin(include_bytes!("../ui/icons/drive.svg")),
        }
    }

    pub fn folder(&self) -> Image {
        self.folder.clone()
    }

    pub fn trash(&self) -> Image {
        self.trash.clone()
    }

    pub fn home(&self) -> Image {
        self.home.clone()
    }

    pub fn drive(&self) -> Image {
        self.drive.clone()
    }

    /// Icon for a filesystem entry. Uses bundled fallbacks for speed.
    /// Theme-based per-MIME lookup is too slow to do synchronously for a
    /// full directory listing (~275ms per miss via freedesktop-icons).
    pub fn for_path(&self, _path: &Path, is_dir: bool) -> Image {
        if is_dir {
            self.folder.clone()
        } else {
            self.file.clone()
        }
    }

    /// Lookup a freedesktop icon by name; returns None if not found in any
    /// theme on this system.
    pub fn by_name(&self, name: &str) -> Option<Image> {
        if let Some(img) = self.cache.borrow().get(name).cloned() {
            return Some(img);
        }
        let path = freedesktop_icons::lookup(name)
            .with_size(48)
            .with_cache()
            .find()?;
        let img = load_path(&path)?;
        self.cache
            .borrow_mut()
            .insert(name.to_string(), img.clone());
        Some(img)
    }
}

fn load_path(path: &PathBuf) -> Option<Image> {
    // Slint can load PNG/SVG from disk directly.
    Image::load_from_path(path).ok()
}

fn load_builtin(bytes: &'static [u8]) -> Image {
    // Bundled SVGs: write to a temp-memory buffer Slint can parse. Simplest path:
    // decode at runtime via `slint::Image::load_from_svg_data`.
    Image::load_from_svg_data(bytes).unwrap_or_else(|_| {
        // Fallback: empty 1x1 image.
        let buf = SharedPixelBuffer::<Rgba8Pixel>::new(1, 1);
        Image::from_rgba8(buf)
    })
}
