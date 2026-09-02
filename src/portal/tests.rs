// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    ffi::{OsStr, OsString},
    os::unix::ffi::OsStringExt as _,
    sync::atomic::Ordering,
};

use ashpd::desktop::file_chooser::{Choice, FileFilter, OpenFileOptions};

use super::*;
use crate::model::{EntryKind, FileEntry, MetadataValue};

fn entry(path: &Path, directory: bool) -> FileEntry {
    FileEntry {
        location: Location::local(path),
        native_name: path.file_name().unwrap_or_default().to_owned(),
        display_name: path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        kind: if directory {
            EntryKind::Directory
        } else {
            EntryKind::File
        },
        size: MetadataValue::Unknown,
        modified_unix_seconds: MetadataValue::Unknown,
    }
}

#[test]
fn open_defaults_match_the_portal_contract() {
    let request = open_request(HandleToken::default(), None, "", OpenFileOptions::default());
    assert!(request.modal);
    assert_eq!(request.title, "Open Files");
    assert_eq!(request.accept_label, "Open");
    assert!(matches!(
        request.kind,
        ChooserKind::Open {
            directory: false,
            multiple: false
        }
    ));
}

#[test]
fn current_file_takes_precedence_over_folder_and_name() {
    let current = tempfile::tempdir().expect("current directory");
    let ignored = tempfile::tempdir().expect("ignored directory");
    let file = current.path().join("existing.txt");
    std::fs::write(&file, b"data").expect("fixture file");

    let suggestion = save_file_suggestion(Some(&file), Some(ignored.path()), Some("ignored.txt"));
    assert_eq!(suggestion.0, current.path());
    assert_eq!(suggestion.1.as_deref(), Some(OsStr::new("existing.txt")));
}

#[test]
fn current_file_preserves_a_non_utf8_filename() {
    let current = tempfile::tempdir().expect("current directory");
    let name = OsString::from_vec(vec![b'n', 0xff]);
    let file = current.path().join(&name);
    std::fs::write(&file, b"data").expect("fixture file");

    let suggestion = save_file_suggestion(Some(&file), None, None);
    assert_eq!(suggestion, (current.path().to_path_buf(), Some(name)));
}

#[test]
fn invalid_current_file_falls_back_without_using_lower_priority_suggestions() {
    let ignored = tempfile::tempdir().expect("ignored directory");
    let suggestion = save_file_suggestion(
        Some(Path::new("relative/missing.txt")),
        Some(ignored.path()),
        Some("ignored.txt"),
    );
    assert_eq!(suggestion, (crate::ui::home_directory(), None));
}

#[test]
fn local_uri_preserves_non_utf8_path_bytes() {
    let path = PathBuf::from("/tmp").join(OsString::from_vec(vec![b'n', 0xff]));
    let uri = local_uri(&path).expect("local URI");
    assert!(uri.as_str().starts_with("file://"));
    assert!(uri.as_str().to_ascii_uppercase().contains("%FF"));
}

#[test]
fn save_filenames_must_be_safe_basenames() {
    for unsafe_name in [
        OsStr::new(""),
        OsStr::new("."),
        OsStr::new(".."),
        OsStr::new("a/b"),
    ] {
        assert!(!safe_filename(unsafe_name), "{unsafe_name:?}");
    }
    assert!(safe_filename(OsStr::new("report.txt")));
    assert!(safe_filename(&OsString::from_vec(vec![b'n', 0xff])));
    assert!(validate_save_filenames(&[]).is_err());
    assert!(validate_save_filenames(&[OsString::from("../bad")]).is_err());
}

#[test]
fn save_files_preserve_order_and_report_collisions_once() {
    let folder = tempfile::tempdir().expect("destination");
    std::fs::write(folder.path().join("second"), b"existing").expect("collision");
    let names = vec![OsString::from("first"), OsString::from("second")];
    let checked = check_destinations(folder.path(), &names).expect("safe destinations");
    assert_eq!(
        checked.paths,
        vec![folder.path().join("first"), folder.path().join("second")]
    );
    assert!(checked.existing_files);
}

#[test]
fn save_files_block_directory_collisions() {
    let folder = tempfile::tempdir().expect("destination");
    std::fs::create_dir(folder.path().join("reserved")).expect("collision directory");
    assert!(
        check_destinations(folder.path(), &[OsString::from("reserved")])
            .expect_err("directory collision")
            .contains("folder")
    );
}

#[test]
fn filters_and_choices_keep_input_order_and_current_filter() {
    let images = FileFilter::new("Images")
        .glob("*.png")
        .mimetype("image/jpeg");
    let text = FileFilter::new("Text").glob("*.txt");
    let encoding = Choice::new("encoding", "Encoding", "utf8")
        .insert("utf8", "UTF-8")
        .insert("latin1", "Latin-1");
    let request = open_request(
        HandleToken::default(),
        None,
        "Choose",
        OpenFileOptions::default()
            .set_filters([images.clone(), text.clone()])
            .set_current_filter(Some(text.clone()))
            .set_choices([Choice::boolean("readonly", "Read only", true), encoding]),
    );
    assert_eq!(request.filters, [images, text.clone()]);
    assert_eq!(request.current_filter, Some(text));
    assert_eq!(
        request.choices.iter().map(Choice::id).collect::<Vec<_>>(),
        ["readonly", "encoding"]
    );
}

#[test]
fn readonly_state_maps_to_writable_result() {
    assert!(!writable_from_read_only(true));
    assert!(writable_from_read_only(false));
}

#[test]
fn open_selection_validates_kind_cardinality_and_locality() {
    let current = Location::local("/tmp");
    let file = entry(Path::new("/tmp/file"), false);
    let folder = entry(Path::new("/tmp/folder"), true);
    assert_eq!(
        open_selection(std::slice::from_ref(&file), &current, false, false).expect("single file"),
        [PathBuf::from("/tmp/file")]
    );
    assert!(open_selection(&[file.clone(), folder.clone()], &current, false, true).is_err());
    assert!(open_selection(&[file.clone(), file], &current, false, false).is_err());
    assert_eq!(
        open_selection(&[], &current, true, false).expect("current folder"),
        [PathBuf::from("/tmp")]
    );
    let remote = FileEntry {
        location: Location::uri("smb://server/share/file"),
        ..folder
    };
    assert!(open_selection(&[remote], &current, true, false).is_err());
}

#[test]
fn cancellation_before_presentation_is_sticky_and_cleanup_is_race_safe() {
    let tracker = Arc::new(RequestTracker::default());
    let first = tracker.begin("same".into());
    assert!(tracker.cancel("same"));
    assert!(first.cancelled.load(Ordering::SeqCst));

    let replacement = tracker.begin("same".into());
    drop(first);
    assert!(tracker.cancel("same"));
    assert!(replacement.cancelled.load(Ordering::SeqCst));
    drop(replacement);
    assert!(!tracker.cancel("same"));
}

#[test]
fn backend_version_and_success_uri_scheme_are_fixed() {
    assert_eq!(FILE_CHOOSER_VERSION, 4);
    for path in [Path::new("/tmp/a"), Path::new("/tmp/a b")] {
        assert!(
            local_uri(path)
                .expect("valid URI")
                .as_str()
                .starts_with("file://")
        );
    }
}
