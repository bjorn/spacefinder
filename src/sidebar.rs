use crate::i18n::tr;
use crate::icons::Icons;
use crate::SidebarItem;
use slint::SharedString;
use std::path::PathBuf;

pub const TRASH_TAG: &str = "__trash__";

pub fn build(icons: &Icons) -> Vec<SidebarItem> {
    let mut items: Vec<SidebarItem> = Vec::new();

    let home = dirs::home_dir();
    push_header(&mut items, &tr("Places"));
    if let Some(home) = &home {
        items.push(item(&tr("Home"), icons.home(), home));
    }
    if let Some(p) = dirs::desktop_dir() {
        items.push(item(&tr("Desktop"), icons.folder(), &p));
    }
    if let Some(p) = dirs::document_dir() {
        items.push(item(&tr("Documents"), icons.folder(), &p));
    }
    if let Some(p) = dirs::download_dir() {
        items.push(item(&tr("Downloads"), icons.folder(), &p));
    }
    if let Some(p) = dirs::audio_dir() {
        items.push(item(&tr("Music"), icons.folder(), &p));
    }
    if let Some(p) = dirs::picture_dir() {
        items.push(item(&tr("Pictures"), icons.folder(), &p));
    }
    if let Some(p) = dirs::video_dir() {
        items.push(item(&tr("Videos"), icons.folder(), &p));
    }
    items.push(separator());
    items.push(SidebarItem {
        label: tr("Trash").into(),
        icon: icons.trash(),
        path: TRASH_TAG.into(),
        is_separator: false,
        is_header: false,
    });

    push_header(&mut items, &tr("Drives"));
    items.push(item(&tr("Root"), icons.drive(), &PathBuf::from("/")));

    items
}

fn push_header(items: &mut Vec<SidebarItem>, label: &str) {
    items.push(SidebarItem {
        label: label.into(),
        icon: slint::Image::default(),
        path: SharedString::default(),
        is_separator: false,
        is_header: true,
    });
}

fn separator() -> SidebarItem {
    SidebarItem {
        label: SharedString::default(),
        icon: slint::Image::default(),
        path: SharedString::default(),
        is_separator: true,
        is_header: false,
    }
}

fn item(label: &str, icon: slint::Image, path: &std::path::Path) -> SidebarItem {
    SidebarItem {
        label: label.into(),
        icon,
        path: path.to_string_lossy().to_string().into(),
        is_separator: false,
        is_header: false,
    }
}
