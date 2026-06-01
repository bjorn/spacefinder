use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use log::warn;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Config {
    pub view_mode: ViewMode,
    pub sort_col: SortCol,
    pub sort_asc: bool,
    pub show_hidden: bool,
    pub folders_first: bool,
    pub window_size: [u32; 2],
    pub last_location: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ViewMode {
    List,
    Grid,
    Columns,
    Treemap,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum SortCol {
    Name,
    Modified,
    Size,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            view_mode: ViewMode::List,
            sort_col: SortCol::Name,
            sort_asc: true,
            show_hidden: false,
            folders_first: true,
            window_size: [1024, 720],
            last_location: None,
        }
    }
}

pub fn config_path() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from(".config"));
    base.join("spacefinder").join("config.json")
}

pub fn load() -> Config {
    load_from(&config_path())
}

pub fn save(config: &Config) {
    save_to(&config_path(), config);
}

fn load_from(path: &Path) -> Config {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Config::default(),
        Err(e) => {
            warn!("failed to read config at {}: {}", path.display(), e);
            return Config::default();
        }
    };
    match serde_json::from_slice::<Config>(&bytes) {
        Ok(cfg) => cfg,
        Err(e) => {
            warn!("failed to parse config at {}: {}", path.display(), e);
            Config::default()
        }
    }
}

fn save_to(path: &Path, config: &Config) {
    let Some(parent) = path.parent() else {
        warn!("config path has no parent: {}", path.display());
        return;
    };
    if let Err(e) = fs::create_dir_all(parent) {
        warn!("failed to create config dir {}: {}", parent.display(), e);
        return;
    }
    let json = match serde_json::to_vec_pretty(config) {
        Ok(j) => j,
        Err(e) => {
            warn!("failed to serialize config: {}", e);
            return;
        }
    };
    let tmp = path.with_extension("json.tmp");
    match fs::File::create(&tmp) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(&json).and_then(|_| f.sync_all()) {
                warn!("failed to write tmp config {}: {}", tmp.display(), e);
                let _ = fs::remove_file(&tmp);
                return;
            }
        }
        Err(e) => {
            warn!("failed to open tmp config {}: {}", tmp.display(), e);
            return;
        }
    }
    // atomic rename; must be on same filesystem
    if let Err(e) = fs::rename(&tmp, path) {
        warn!(
            "failed to rename {} -> {}: {}",
            tmp.display(),
            path.display(),
            e
        );
        let _ = fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn cfg_path(dir: &TempDir) -> PathBuf {
        dir.path().join("spacefinder").join("config.json")
    }

    #[test]
    fn load_missing_returns_default() {
        let dir = TempDir::new().unwrap();
        let cfg = load_from(&cfg_path(&dir));
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = cfg_path(&dir);
        let mut cfg = Config::default();
        cfg.view_mode = ViewMode::Grid;
        cfg.sort_col = SortCol::Size;
        cfg.sort_asc = false;
        cfg.show_hidden = true;
        cfg.folders_first = false;
        cfg.window_size = [1600, 900];
        cfg.last_location = Some(PathBuf::from("/tmp/example"));
        save_to(&path, &cfg);
        let loaded = load_from(&path);
        assert_eq!(loaded, cfg);
    }

    #[test]
    fn load_corrupt_returns_default() {
        let dir = TempDir::new().unwrap();
        let path = cfg_path(&dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{not valid json at all").unwrap();
        let cfg = load_from(&path);
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn load_partial_json_fills_defaults() {
        let dir = TempDir::new().unwrap();
        let path = cfg_path(&dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, br#"{"view_mode":"Grid"}"#).unwrap();
        let cfg = load_from(&path);
        let mut expected = Config::default();
        expected.view_mode = ViewMode::Grid;
        assert_eq!(cfg, expected);
    }

    /// Regression guard: pre-Columns configs that name only `"List"` or
    /// `"Grid"` must still deserialize correctly after the `Columns`
    /// variant was added.
    #[test]
    fn legacy_view_mode_values_still_deserialize() {
        let dir = TempDir::new().unwrap();
        let path = cfg_path(&dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        fs::write(&path, br#"{"view_mode":"Grid"}"#).unwrap();
        assert_eq!(load_from(&path).view_mode, ViewMode::Grid);

        fs::write(&path, br#"{"view_mode":"List"}"#).unwrap();
        assert_eq!(load_from(&path).view_mode, ViewMode::List);

        fs::write(&path, br#"{"view_mode":"Columns"}"#).unwrap();
        assert_eq!(load_from(&path).view_mode, ViewMode::Columns);
    }

    #[test]
    fn atomic_save_does_not_leave_tmpfile_on_success() {
        let dir = TempDir::new().unwrap();
        let path = cfg_path(&dir);
        save_to(&path, &Config::default());
        let parent = path.parent().unwrap();
        let entries: Vec<_> = fs::read_dir(parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        assert!(entries.iter().any(|n| n == "config.json"));
        assert!(
            !entries.iter().any(|n| n == "config.json.tmp"),
            "tmp file should not exist after a successful save"
        );
    }

    #[test]
    fn load_unknown_fields_are_ignored() {
        let dir = TempDir::new().unwrap();
        let path = cfg_path(&dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            br#"{"view_mode":"List","future_field":42,"another":"x"}"#,
        )
        .unwrap();
        let cfg = load_from(&path);
        assert_eq!(cfg, Config::default());
    }
}
