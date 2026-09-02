// SPDX-License-Identifier: GPL-3.0-or-later

use std::{rc::Rc, sync::Arc};

use gtk::prelude::*;

use super::{
    DocumentSelection, PreviewUnit, SelectionPoint, SourceUnit, VirtualPreviewState,
    bounded_text_prefix, code_block_copy_text, drag_threshold_crossed, highlighted_code_language,
    local_selection, matching_link, plain_text_view, rendered_document, selection_text,
    source_units, styled_markup, use_virtual_source, vertical_distance,
};
use crate::services::{
    DocumentLayout, DocumentSpan, DocumentSpanStyle, DocumentUnit, DocumentUnitKind,
};

#[test]
fn source_units_bound_normal_rows_and_isolate_pathological_lines() {
    let content = format!(
        "{}{}\ntail\n",
        "short\n".repeat(300),
        "x".repeat(super::PATHOLOGICAL_TEXT_UNIT_BYTES + 1)
    );
    let (units, split_lines) = source_units(&content);

    assert!(split_lines);
    assert!(units.iter().all(|unit| {
        unit.source.len() <= super::SOURCE_UNIT_BYTES && unit.line_count <= super::SOURCE_UNIT_LINES
    }));
    let long = units
        .iter()
        .filter(|unit| unit.first_line == 301)
        .collect::<Vec<_>>();
    assert_eq!(long.len(), 2);
    assert!(
        long.iter()
            .all(|unit| unit.display.len() <= super::PATHOLOGICAL_TEXT_UNIT_BYTES)
    );
    assert!(long[1].continuation);
    assert_eq!(units.iter().map(|unit| unit.line_count).sum::<usize>(), 302);
    assert_eq!(units.last().map(|unit| unit.first_line), Some(302));
    assert_eq!(
        units
            .iter()
            .map(|unit| unit.source.as_str())
            .collect::<String>(),
        content
    );
}

#[test]
fn small_pathological_lines_use_the_virtual_source_path() {
    assert!(use_virtual_source(
        &"x".repeat(super::PATHOLOGICAL_TEXT_UNIT_BYTES + 1)
    ));
    assert!(!use_virtual_source("ordinary source\n"));
}

#[test]
fn maximum_source_lines_stay_bounded_and_complete() {
    let content = format!("{}\n", "x".repeat(64 * 1024 - 1)).repeat(16);
    let (units, split_lines) = source_units(&content);

    assert!(split_lines);
    assert_eq!(units.len(), 512);
    assert!(
        units
            .iter()
            .all(|unit| unit.display.len() <= super::PATHOLOGICAL_TEXT_UNIT_BYTES)
    );
    assert_eq!(
        units
            .iter()
            .map(|unit| unit.source.as_str())
            .collect::<String>(),
        content
    );
}

#[test]
fn cross_row_selection_copies_full_middle_units_from_the_model() {
    let units = Rc::new(vec![
        source("first line\n", "first line\n"),
        source("visible", "visible plus an unrendered tail\n"),
        source("last line", "last line"),
    ]);
    let state = VirtualPreviewState {
        units,
        selection: std::cell::Cell::new(Some(DocumentSelection {
            anchor: SelectionPoint { unit: 0, offset: 6 },
            focus: SelectionPoint { unit: 2, offset: 4 },
        })),
        bound: std::cell::RefCell::default(),
        dragging: std::cell::Cell::new(false),
        press: std::cell::Cell::new((0.0, 0.0)),
        pointer: std::cell::Cell::new((0.0, 0.0)),
        drag_generation: std::cell::Cell::new(0),
        hovered: std::cell::Cell::new(None),
        pressed_link: std::cell::RefCell::new(None),
    };

    assert_eq!(
        selection_text(&state).as_deref(),
        Some("line\nvisible plus an unrendered tail\nlast")
    );
    assert_eq!(local_selection(state.selection.get(), 1, 7), Some((0, 7)));
}

#[test]
fn selection_does_not_invent_newlines_between_line_chunks() {
    let units = Rc::new(vec![source("abcd", "abcd"), source("ef", "ef\n")]);
    let state = VirtualPreviewState {
        units,
        selection: std::cell::Cell::new(Some(DocumentSelection {
            anchor: SelectionPoint { unit: 0, offset: 2 },
            focus: SelectionPoint { unit: 1, offset: 1 },
        })),
        bound: std::cell::RefCell::default(),
        dragging: std::cell::Cell::new(false),
        press: std::cell::Cell::new((0.0, 0.0)),
        pointer: std::cell::Cell::new((0.0, 0.0)),
        drag_generation: std::cell::Cell::new(0),
        hovered: std::cell::Cell::new(None),
        pressed_link: std::cell::RefCell::new(None),
    };

    assert_eq!(selection_text(&state).as_deref(), Some("cde"));
}

#[test]
fn bounded_table_text_keeps_utf8_boundaries() {
    assert_eq!(bounded_text_prefix("abλcd", 3), "ab");
}

#[test]
fn table_selection_is_atomic_and_copies_tsv() {
    let units = Rc::new(vec![PreviewUnit::Document(DocumentUnit {
        kind: DocumentUnitKind::Table {
            list_depth: None,
            rows: Vec::new(),
        },
        text: String::new(),
        copy_text: "A\tB\n1\t2\n".to_owned(),
        spans: Vec::new(),
        wrap: false,
        first: true,
        last: true,
    })]);
    let state = VirtualPreviewState {
        units,
        selection: std::cell::Cell::new(Some(DocumentSelection {
            anchor: SelectionPoint { unit: 0, offset: 0 },
            focus: SelectionPoint { unit: 0, offset: 1 },
        })),
        bound: std::cell::RefCell::default(),
        dragging: std::cell::Cell::new(false),
        press: std::cell::Cell::new((0.0, 0.0)),
        pointer: std::cell::Cell::new((0.0, 0.0)),
        drag_generation: std::cell::Cell::new(0),
        hovered: std::cell::Cell::new(None),
        pressed_link: std::cell::RefCell::new(None),
    };

    assert_eq!(selection_text(&state).as_deref(), Some("A\tB\n1\t2\n"));
}

#[test]
fn visible_table_markup_remains_balanced_with_overlapping_styles() {
    let spans = vec![
        DocumentSpan {
            range: 0..4,
            style: DocumentSpanStyle::Bold,
        },
        DocumentSpan {
            range: 2..6,
            style: DocumentSpanStyle::Link(Arc::from("https://example.test")),
        },
    ];
    let markup = styled_markup("abcdef", &spans);
    let pango_markup = markup
        .replace("<a href=\"https://example.test\">", "<u>")
        .replace("</a>", "</u>");
    let (_, plain, _) =
        gtk::pango::parse_markup(&pango_markup, '\0').expect("balanced Pango markup");
    assert_eq!(plain, "abcdef");
    assert!(markup.contains("href=\"https://example.test\""));
}

#[test]
fn links_activate_only_when_press_and_release_match() {
    assert_eq!(
        matching_link(Some("https://example.test"), Some("https://example.test")),
        Some("https://example.test")
    );
    assert_eq!(
        matching_link(Some("https://first.test"), Some("https://second.test")),
        None
    );
    assert_eq!(matching_link(None, Some("https://example.test")), None);
    assert_eq!(matching_link(Some("https://example.test"), None), None);
}

#[test]
fn gaps_resolve_to_the_nearest_adjacent_row() {
    let rows = [
        gtk::graphene::Rect::new(0.0, 0.0, 100.0, 10.0),
        gtk::graphene::Rect::new(0.0, 20.0, 100.0, 10.0),
        gtk::graphene::Rect::new(0.0, 200.0, 100.0, 10.0),
    ];
    let nearest = |y| {
        rows.iter()
            .enumerate()
            .min_by(|(_, left), (_, right)| {
                vertical_distance(left, y).total_cmp(&vertical_distance(right, y))
            })
            .map(|(index, _)| index)
    };

    assert_eq!(nearest(14.0), Some(0));
    assert_eq!(nearest(16.0), Some(1));
}

#[test]
fn code_copy_reassembles_one_virtualized_block() {
    let code = |text: &str, first, last| {
        PreviewUnit::Document(DocumentUnit {
            kind: DocumentUnitKind::Code {
                list_depth: None,
                language: Some("rust"),
            },
            text: text.trim_end_matches('\n').to_owned(),
            copy_text: text.to_owned(),
            spans: Vec::new(),
            wrap: false,
            first,
            last,
        })
    };
    let units = vec![
        code("first\n", true, false),
        code("second\n", false, true),
        code("separate\n", true, true),
    ];

    assert_eq!(
        code_block_copy_text(&units, 1).as_deref(),
        Some("first\nsecond\n")
    );
    assert_eq!(
        code_block_copy_text(&units, 2).as_deref(),
        Some("separate\n")
    );
    assert_eq!(highlighted_code_language(document(&units[2])), Some("rust"));
    assert_eq!(highlighted_code_language(document(&units[0])), None);
}

#[test]
fn rendered_preview_releases_its_widget_tree() {
    if gtk::init().is_err() {
        return;
    }
    let nul_view = plain_text_view("before\0after", true);
    let nul_buffer = nul_view.buffer();
    assert_eq!(
        nul_buffer.text(&nul_buffer.start_iter(), &nul_buffer.end_iter(), false),
        "before�after"
    );
    drop(nul_view);

    let long_view = plain_text_view(&"x".repeat(2_048), true);
    assert!(long_view.hexpands());
    assert!(long_view.measure(gtk::Orientation::Horizontal, -1).0 > 16);
    drop(long_view);

    let threshold = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    assert!(!drag_threshold_crossed(&threshold, 10.0, 10.0, 10.0, 10.0));
    assert!(drag_threshold_crossed(&threshold, 10.0, 10.0, 100.0, 100.0));

    let unit = |text: &str| DocumentUnit {
        kind: DocumentUnitKind::Paragraph,
        text: text.to_owned(),
        copy_text: format!("{text}\n"),
        spans: Vec::new(),
        wrap: true,
        first: true,
        last: true,
    };
    let root = rendered_document(
        DocumentLayout {
            units: vec![unit("first"), unit("second")],
        },
        Vec::new(),
    );
    let weak = root.downgrade();

    drop(root);
    while gtk::glib::MainContext::default().pending() {
        gtk::glib::MainContext::default().iteration(false);
    }

    assert!(weak.upgrade().is_none());
}

fn document(unit: &PreviewUnit) -> &DocumentUnit {
    let PreviewUnit::Document(unit) = unit else {
        panic!("expected document unit");
    };
    unit
}

fn source(display: &str, source: &str) -> PreviewUnit {
    PreviewUnit::Source(SourceUnit {
        display: display.strip_suffix('\n').unwrap_or(display).to_owned(),
        source: source.to_owned(),
        first_line: 1,
        line_count: 1,
        continuation: false,
    })
}
