// SPDX-License-Identifier: GPL-3.0-or-later

use super::{
    DocumentTextPosition, DocumentView, MEDIA_PLUGIN_INSTALL_COMMAND, PDF_MAX_ZOOM, PDF_MIN_ZOOM,
    accepts_preview_event, document_selection_range, document_view_action, format_file_size,
    initial_document_view, media_error_feedback, pdf_zoom_after_scroll,
    preview_width_for_empty_space,
};
use crate::services::PreviewRequestId;

#[test]
fn formats_preview_file_sizes() {
    assert_eq!(format_file_size(999), "999 B");
    assert_eq!(format_file_size(1_200), "1.2 kB");
    assert_eq!(format_file_size(2_500_000), "2.5 MB");
}

#[test]
fn media_errors_explain_missing_runtime_plugins() {
    let (title, detail, command) =
        media_error_feedback("Your GStreamer installation is missing a plug-in.");
    assert_eq!(title, "Additional media support required");
    assert!(detail.contains("GStreamer plugins"));
    assert_eq!(command, Some(MEDIA_PLUGIN_INSTALL_COMMAND));
    assert_eq!(
        command,
        Some("sudo pacman -S --needed gst-plugins-good gst-libav")
    );

    let (title, detail, command) = media_error_feedback("The media data is corrupt");
    assert_eq!(title, "Preview unavailable");
    assert!(detail.contains("The media data is corrupt"));
    assert_eq!(command, None);
}

#[test]
fn initial_preview_uses_most_of_the_unoccupied_width() {
    assert_eq!(preview_width_for_empty_space(2_000, 500), 1_350);
    assert_eq!(preview_width_for_empty_space(700, 650), 280);
}

#[test]
fn pdf_scroll_zoom_stays_within_its_supported_range() {
    assert!(pdf_zoom_after_scroll(1.0, -1.0) > 1.0);
    assert!(pdf_zoom_after_scroll(2.0, 1.0) < 2.0);
    assert_eq!(pdf_zoom_after_scroll(PDF_MIN_ZOOM, 100.0), PDF_MIN_ZOOM);
    assert_eq!(pdf_zoom_after_scroll(PDF_MAX_ZOOM, -100.0), PDF_MAX_ZOOM);
}

#[test]
fn each_document_uses_the_current_default_and_unavailable_rendering_forces_source() {
    assert_eq!(initial_document_view(true, true), DocumentView::Rendered);
    assert_eq!(initial_document_view(false, true), DocumentView::Source);
    assert_eq!(initial_document_view(true, false), DocumentView::Source);
}

#[test]
fn document_view_action_describes_its_destination() {
    assert_eq!(
        document_view_action(DocumentView::Rendered),
        ("View source", crate::assets::icons::FILE_CODE)
    );
    assert_eq!(
        document_view_action(DocumentView::Source),
        ("View rendered", crate::assets::icons::DOCUMENTS)
    );
}

#[test]
fn document_selection_spans_labels_in_both_directions() {
    let first = DocumentTextPosition {
        label: 0,
        offset: 2,
    };
    let last = DocumentTextPosition {
        label: 2,
        offset: 1,
    };
    for (anchor, cursor) in [(first, last), (last, first)] {
        assert_eq!(document_selection_range(0, 5, anchor, cursor), Some((2, 5)));
        assert_eq!(document_selection_range(1, 6, anchor, cursor), Some((0, 6)));
        assert_eq!(document_selection_range(2, 4, anchor, cursor), Some((0, 1)));
        assert_eq!(document_selection_range(3, 3, anchor, cursor), None);
    }
    assert_eq!(
        document_selection_range(
            1,
            6,
            DocumentTextPosition {
                label: 1,
                offset: 5,
            },
            DocumentTextPosition {
                label: 1,
                offset: 2,
            },
        ),
        Some((2, 5))
    );
}

#[test]
fn stale_preview_responses_are_rejected() {
    let current = PreviewRequestId(2);
    assert!(accepts_preview_event(Some(current), current, current));
    assert!(!accepts_preview_event(
        Some(current),
        PreviewRequestId(1),
        PreviewRequestId(1)
    ));
    assert!(!accepts_preview_event(
        Some(current),
        current,
        PreviewRequestId(1)
    ));
    assert!(!accepts_preview_event(None, current, current));
}
