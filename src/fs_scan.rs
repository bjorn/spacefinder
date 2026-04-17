use chrono::{DateTime, Local};
use humansize::{format_size, BINARY};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
    pub modified: SystemTime,
    pub hidden: bool,
}

impl Entry {
    pub fn size_text(&self) -> String {
        if self.is_dir {
            // Don't enumerate directory contents (slow).
            String::new()
        } else {
            format_size(self.size, BINARY)
        }
    }

    pub fn modified_text(&self) -> String {
        let dt: DateTime<Local> = self.modified.into();
        dt.format("%b %-d, %Y %H:%M").to_string()
    }
}

pub fn scan(dir: &Path) -> std::io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy().to_string();
        let hidden = name.starts_with('.');
        let Ok(meta) = entry.metadata() else { continue };
        let is_dir = meta.is_dir();
        let size = if is_dir { 0 } else { meta.len() };
        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        out.push(Entry {
            name,
            path,
            is_dir,
            size,
            modified,
            hidden,
        });
    }
    Ok(out)
}

#[derive(Copy, Clone)]
pub enum SortCol {
    Name,
    Modified,
    Size,
}

impl SortCol {
    pub fn from_int(i: i32) -> Self {
        match i {
            1 => Self::Modified,
            2 => Self::Size,
            _ => Self::Name,
        }
    }
}
