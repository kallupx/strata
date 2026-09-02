// SPDX-License-Identifier: GPL-3.0-or-later

use std::{ffi::OsString, path::Path};

use ashpd::desktop::file_chooser::FileFilter;

use super::*;
use crate::model::{EntryKind, MetadataValue};

fn entry(name: &str, kind: EntryKind) -> FileEntry {
    FileEntry {
        location: Location::local(Path::new("/tmp").join(name)),
        native_name: OsString::from(name),
        display_name: name.to_owned(),
        kind,
        size: MetadataValue::Unknown,
        modified_unix_seconds: MetadataValue::Unknown,
    }
}

#[test]
#[ignore = "requires a GTK display and exclusive main context"]
fn native_filters_match_globs_and_mime_types_without_hiding_directories() {
    gtk::init().expect("GTK display");
    let filter = gtk::FileFilter::new();
    filter.add_pattern("*.txt");
    filter.add_mime_type("image/jpeg");

    assert!(file_filter_matches(
        &filter,
        &entry("notes.txt", EntryKind::File)
    ));
    assert!(file_filter_matches(
        &filter,
        &entry("photo.jpg", EntryKind::File)
    ));
    assert!(!file_filter_matches(
        &filter,
        &entry("archive.zip", EntryKind::File)
    ));
    assert!(file_filter_matches(
        &filter,
        &entry("folder.zip", EntryKind::Directory)
    ));
}

#[test]
fn portal_filters_select_the_requested_filter_or_the_first_one() {
    let images = FileFilter::new("Images").glob("*.png");
    let text = FileFilter::new("Text").glob("*.txt");

    let (filters, selected) = normalize_portal_filters(std::slice::from_ref(&images), None);
    assert_eq!(selected, Some(0));
    assert_eq!(filters[0], images);

    let (filters, selected) = normalize_portal_filters(std::slice::from_ref(&images), Some(&text));
    assert_eq!(selected, Some(1));
    assert_eq!(filters[1], text);
}

#[test]
fn chooser_locations_reject_remote_uris_before_io() {
    let source = ChooserFileSource::new();
    let error = source
        .validate_location(&Location::uri("smb://server/share"))
        .expect_err("remote locations are unavailable");
    assert!(matches!(
        error,
        LocationValidationError::UnsupportedScheme(_)
    ));
    assert!(error.to_string().contains("local files and folders only"));
}

#[test]
fn chooser_watches_local_directories() {
    let root = tempfile::tempdir().expect("temporary directory");
    let source = ChooserFileSource::new();

    let watch = source.watch(Location::local(root.path()), true, Rc::new(|_| {}));

    assert!(watch.is_some());
}

#[test]
fn chooser_previews_supported_files_but_not_directories() {
    assert!(
        chooser_preview_target(Some(entry("notes.txt", EntryKind::File))).is_some(),
        "supported files should be previewable"
    );
    assert!(
        chooser_preview_target(Some(entry("folder", EntryKind::Directory))).is_none(),
        "folders should remain navigation targets"
    );
}

#[test]
fn chooser_selection_excludes_navigation_items() {
    let file = entry("notes.txt", EntryKind::File);
    let folder = entry("folder", EntryKind::Directory);

    assert_eq!(
        eligible_open_entries(vec![folder.clone(), file.clone()], false),
        [file]
    );
    assert_eq!(
        eligible_open_entries(vec![folder.clone(), folder.clone()], true),
        [folder.clone(), folder]
    );
}

#[test]
fn folder_accept_shortcut_requires_control_and_enter() {
    let control = gtk::gdk::ModifierType::CONTROL_MASK;
    let shift = gtk::gdk::ModifierType::SHIFT_MASK;
    let alt = gtk::gdk::ModifierType::ALT_MASK;

    assert!(is_folder_accept_shortcut(gtk::gdk::Key::Return, control));
    assert!(is_folder_accept_shortcut(gtk::gdk::Key::KP_Enter, control));
    assert!(!is_folder_accept_shortcut(
        gtk::gdk::Key::Return,
        gtk::gdk::ModifierType::empty()
    ));
    assert!(!is_folder_accept_shortcut(
        gtk::gdk::Key::Return,
        control | shift
    ));
    assert!(!is_folder_accept_shortcut(
        gtk::gdk::Key::Return,
        control | alt
    ));
}
