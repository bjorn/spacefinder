//! File-type categories for color-coding the views.
//!
//! Categorization is cross-platform: it derives from the file name's
//! extension via a curated map, falling back to `mime_guess` (which
//! maps extensions to MIME types with no OS support files) for anything
//! the map does not cover. No Linux-only MIME database is consulted, so
//! coloring works identically on every platform.

use std::path::Path;

/// Broad file-type category used to pick a tile color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileCategory {
    Code,
    Image,
    Video,
    Audio,
    Archive,
    Document,
    Database,
    Executable,
    Font,
    Config,
    Data,
    Other,
}

impl FileCategory {
    /// Determine a file's category from its path, using the extension.
    ///
    /// A curated extension map handles the common cases (and resolves
    /// ambiguities such as `.ts`, which is TypeScript here rather than a
    /// MPEG transport stream). Anything not in the map falls back to the
    /// top-level type reported by `mime_guess`.
    pub fn from_path(path: &Path) -> Self {
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            let ext = ext.to_ascii_lowercase();
            if let Some(cat) = Self::from_ext(&ext) {
                return cat;
            }
        }
        Self::from_mime_fallback(path)
    }

    /// Curated extension to category map. Returns `None` for unknown
    /// extensions so the caller can fall back to `mime_guess`.
    fn from_ext(ext: &str) -> Option<Self> {
        let cat = match ext {
            // Code and markup
            "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "go" | "c" | "cpp" | "cc" | "h" | "hpp"
            | "java" | "kt" | "swift" | "rb" | "php" | "cs" | "scala" | "lua" | "r" | "m"
            | "mm" | "pl" | "pm" | "sh" | "bash" | "zsh" | "fish" | "ps1" | "bat" | "cmd"
            | "zig" | "asm" | "s" | "v" | "sv" | "vhd" | "vhdl" | "elm" | "ex" | "exs" | "erl"
            | "hs" | "ml" | "mli" | "clj" | "cljs" | "lisp" | "el" | "dart" | "vue" | "svelte"
            | "css" | "scss" | "sass" | "less" | "html" | "htm" | "sql" | "graphql" | "gql"
            | "proto" | "thrift" | "wasm" | "wat" => Self::Code,

            // Images
            "png" | "jpg" | "jpeg" | "gif" | "bmp" | "svg" | "ico" | "webp" | "tiff" | "tif"
            | "psd" | "ai" | "eps" | "raw" | "cr2" | "nef" | "arw" | "dng" | "heic" | "heif"
            | "avif" | "jxl" => Self::Image,

            // Video (".ts" is matched as Code above on purpose)
            "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "m4v" | "mpg" | "mpeg"
            | "3gp" | "ogv" => Self::Video,

            // Audio
            "mp3" | "flac" | "wav" | "aac" | "ogg" | "wma" | "m4a" | "opus" | "aiff" | "ape"
            | "alac" | "mid" | "midi" => Self::Audio,

            // Archives and packages
            "zip" | "tar" | "gz" | "bz2" | "xz" | "zst" | "lz4" | "7z" | "rar" | "cab" | "iso"
            | "dmg" | "deb" | "rpm" | "pkg" | "msi" | "appimage" | "snap" | "flatpak" | "tgz"
            | "tbz2" | "txz" => Self::Archive,

            // Documents
            "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "odp"
            | "rtf" | "txt" | "md" | "rst" | "tex" | "latex" | "epub" | "mobi" | "pages"
            | "numbers" | "key" | "csv" => Self::Document,

            // Databases
            "db" | "sqlite" | "sqlite3" | "mdb" | "accdb" | "dbf" | "ldb" => Self::Database,

            // Executables and object files
            "exe" | "dll" | "so" | "dylib" | "a" | "lib" | "o" | "obj" | "class" | "pyc"
            | "pyo" | "elc" | "beam" => Self::Executable,

            // Fonts
            "ttf" | "otf" | "woff" | "woff2" | "eot" => Self::Font,

            // Config and structured text
            "json" | "yaml" | "yml" | "toml" | "ini" | "cfg" | "conf" | "env" | "xml" | "plist"
            | "properties" | "reg" => Self::Config,

            // Generic / serialized data
            "bin" | "dat" | "parquet" | "arrow" | "avro" | "msgpack" | "cbor" | "pb" | "npy"
            | "npz" | "hdf5" | "h5" | "nc" | "fits" => Self::Data,

            _ => return None,
        };
        Some(cat)
    }

    /// Map an extension we do not recognize to a category via the
    /// top-level type that `mime_guess` infers. Unknown types land in
    /// `Other`.
    fn from_mime_fallback(path: &Path) -> Self {
        let guess = mime_guess::from_path(path);
        let Some(mime) = guess.first() else {
            return Self::Other;
        };
        match mime.type_().as_str() {
            "image" => Self::Image,
            "video" => Self::Video,
            "audio" => Self::Audio,
            "font" => Self::Font,
            "text" => Self::Document,
            _ => Self::Other,
        }
    }

    /// Every variant, for building a legend.
    pub const ALL: &[FileCategory] = &[
        Self::Code,
        Self::Image,
        Self::Video,
        Self::Audio,
        Self::Archive,
        Self::Document,
        Self::Database,
        Self::Executable,
        Self::Font,
        Self::Config,
        Self::Data,
        Self::Other,
    ];

    /// RGB fill color for this category, tuned for the dark theme.
    pub fn rgb(&self) -> (u8, u8, u8) {
        match self {
            Self::Code => (86, 156, 214),      // blue
            Self::Image => (206, 145, 52),     // amber
            Self::Video => (214, 86, 86),      // red
            Self::Audio => (156, 86, 214),     // purple
            Self::Archive => (86, 214, 156),   // teal
            Self::Document => (214, 206, 86),  // yellow
            Self::Database => (214, 130, 86),  // orange
            Self::Executable => (180, 86, 86), // dark red
            Self::Font => (150, 150, 150),     // gray
            Self::Config => (120, 180, 120),   // green
            Self::Data => (86, 136, 214),      // steel blue
            Self::Other => (120, 120, 140),    // muted blue-gray
        }
    }

    /// Short human-readable label, used by the legend.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Code => "Code",
            Self::Image => "Images",
            Self::Video => "Video",
            Self::Audio => "Audio",
            Self::Archive => "Archives",
            Self::Document => "Documents",
            Self::Database => "Databases",
            Self::Executable => "Executables",
            Self::Font => "Fonts",
            Self::Config => "Config",
            Self::Data => "Data",
            Self::Other => "Other",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn cat(name: &str) -> FileCategory {
        FileCategory::from_path(Path::new(name))
    }

    #[test]
    fn maps_common_extensions() {
        assert_eq!(cat("main.rs"), FileCategory::Code);
        assert_eq!(cat("photo.JPG"), FileCategory::Image);
        assert_eq!(cat("clip.mp4"), FileCategory::Video);
        assert_eq!(cat("song.flac"), FileCategory::Audio);
        assert_eq!(cat("backup.tar.gz"), FileCategory::Archive);
        assert_eq!(cat("report.pdf"), FileCategory::Document);
        assert_eq!(cat("data.sqlite"), FileCategory::Database);
        assert_eq!(cat("lib.so"), FileCategory::Executable);
        assert_eq!(cat("Inter.woff2"), FileCategory::Font);
        assert_eq!(cat("settings.toml"), FileCategory::Config);
        assert_eq!(cat("weights.npy"), FileCategory::Data);
    }

    #[test]
    fn extension_is_case_insensitive() {
        assert_eq!(cat("IMAGE.PNG"), FileCategory::Image);
        assert_eq!(cat("Archive.ZIP"), FileCategory::Archive);
    }

    #[test]
    fn ts_is_code_not_video() {
        // ".ts" is overwhelmingly TypeScript in practice; the curated
        // map must win over any MIME transport-stream guess.
        assert_eq!(cat("module.ts"), FileCategory::Code);
    }

    #[test]
    fn unknown_extension_falls_back_to_other() {
        assert_eq!(cat("mystery.qzx"), FileCategory::Other);
    }

    #[test]
    fn no_extension_is_other() {
        assert_eq!(cat("README"), FileCategory::Other);
        assert_eq!(cat("Makefile"), FileCategory::Other);
    }

    #[test]
    fn mime_fallback_catches_unlisted_image() {
        // ".jpe" is a valid JPEG extension not in the curated map;
        // mime_guess should still classify it as an image.
        assert_eq!(cat("scan.jpe"), FileCategory::Image);
    }

    #[test]
    fn all_variants_have_distinct_colors() {
        let mut seen = std::collections::HashSet::new();
        for c in FileCategory::ALL {
            assert!(seen.insert(c.rgb()), "duplicate color for {:?}", c);
        }
    }
}
