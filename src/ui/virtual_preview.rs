// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeSet, HashMap},
    rc::Rc,
    time::Duration,
};

use gtk::{gio, glib, prelude::*};
use sourceview5::prelude::*;

use crate::{
    model::Location,
    services::{
        DocumentLayout, DocumentListChildKind, DocumentSpan, DocumentSpanStyle,
        DocumentTableCellLayout, DocumentUnit, DocumentUnitKind, has_web_scheme,
        normalize_preview_text,
    },
};

const SOURCE_UNIT_BYTES: usize = 16 * 1024;
const SOURCE_UNIT_LINES: usize = 256;
const PATHOLOGICAL_TEXT_UNIT_BYTES: usize = 2 * 1024;
const TABLE_CELL_DISPLAY_BYTES: usize = 512;
const VIRTUAL_ROW_MIN_HEIGHT: i32 = 16;
const AUTOSCROLL_EDGE: f64 = 36.0;

#[derive(Clone)]
enum PreviewUnit {
    Document(DocumentUnit),
    Source(SourceUnit),
}

impl PreviewUnit {
    fn display_text(&self) -> &str {
        match self {
            Self::Document(unit) => &unit.text,
            Self::Source(unit) => &unit.display,
        }
    }

    fn copy_text(&self) -> &str {
        match self {
            Self::Document(unit) => &unit.copy_text,
            Self::Source(unit) => &unit.source,
        }
    }

    fn selection_len(&self) -> usize {
        match self {
            Self::Document(DocumentUnit {
                kind: DocumentUnitKind::Table { .. },
                ..
            }) => 1,
            _ => self.display_text().chars().count(),
        }
    }
}

#[derive(Clone)]
struct SourceUnit {
    display: String,
    source: String,
    first_line: usize,
    line_count: usize,
    continuation: bool,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct SelectionPoint {
    unit: usize,
    offset: usize,
}

#[derive(Clone, Copy)]
struct DocumentSelection {
    anchor: SelectionPoint,
    focus: SelectionPoint,
}

impl DocumentSelection {
    fn normalized(self) -> (SelectionPoint, SelectionPoint) {
        if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }

    fn is_empty(self) -> bool {
        self.anchor == self.focus
    }
}

#[derive(Default)]
struct BoundRow {
    root: glib::WeakRef<gtk::Box>,
    view: glib::WeakRef<super::document_view::DocumentTextView>,
    table: glib::WeakRef<gtk::Grid>,
}

struct VirtualPreviewState {
    units: Rc<Vec<PreviewUnit>>,
    selection: Cell<Option<DocumentSelection>>,
    bound: RefCell<HashMap<usize, BoundRow>>,
    dragging: Cell<bool>,
    press: Cell<(f64, f64)>,
    pointer: Cell<(f64, f64)>,
    drag_generation: Cell<u64>,
    hovered: Cell<Option<(usize, usize)>>,
    pressed_link: RefCell<Option<String>>,
}

struct DocumentTextTags {
    headings: [gtk::TextTag; 6],
    quote: gtk::TextTag,
    code: gtk::TextTag,
    code_start: gtk::TextTag,
    code_end: gtk::TextTag,
    rule: gtk::TextTag,
    lists: Vec<gtk::TextTag>,
    list_children: Vec<gtk::TextTag>,
    dim: gtk::TextTag,
    accent: gtk::TextTag,
    link: gtk::TextTag,
    bold: gtk::TextTag,
    italic: gtk::TextTag,
    strikethrough: gtk::TextTag,
    monospace: gtk::TextTag,
    underline: gtk::TextTag,
}

pub(super) fn rendered_document(layout: DocumentLayout, warnings: Vec<String>) -> gtk::Box {
    let units = layout
        .units
        .into_iter()
        .map(PreviewUnit::Document)
        .collect();
    virtual_preview(units, warnings, false).0
}

pub(super) fn source_document(content: &str, truncated: bool) -> gtk::Box {
    let content = normalize_preview_text(content);
    let (source, split_lines) = source_units(&content);
    let units = source.into_iter().map(PreviewUnit::Source).collect();
    let (container, _) = virtual_preview(units, Vec::new(), true);
    if truncated || split_lines {
        let message = match (truncated, split_lines) {
            (true, true) => {
                "Long source lines are split into virtual rows for responsive scrolling; file reading was limited to the first 1 MB. Copy preserves the complete loaded text and original line breaks."
            }
            (true, false) => "Preview limited to the first 1 MB.",
            (false, true) => {
                "Long source lines are split into virtual rows for responsive scrolling. Copy preserves the complete text and original line breaks."
            }
            (false, false) => unreachable!(),
        };
        let notice = gtk::Label::new(Some(message));
        notice.add_css_class("preview-note");
        notice.set_wrap(true);
        container.append(&notice);
    }
    container
}

pub(super) fn use_virtual_source(content: &str) -> bool {
    content.len() > super::preview::SOURCE_HIGHLIGHT_BYTE_LIMIT
        || content.lines().count() > super::preview::SOURCE_HIGHLIGHT_LINE_LIMIT
        || content
            .lines()
            .any(|line| line.len() > PATHOLOGICAL_TEXT_UNIT_BYTES)
}

fn virtual_preview(
    units: Vec<PreviewUnit>,
    warnings: Vec<String>,
    source: bool,
) -> (gtk::Box, Rc<VirtualPreviewState>) {
    let state = Rc::new(VirtualPreviewState {
        units: Rc::new(units),
        selection: Cell::new(None),
        bound: RefCell::new(HashMap::new()),
        dragging: Cell::new(false),
        press: Cell::new((0.0, 0.0)),
        pointer: Cell::new((0.0, 0.0)),
        drag_generation: Cell::new(0),
        hovered: Cell::new(None),
        pressed_link: RefCell::new(None),
    });
    let model = gio::ListStore::new::<glib::BoxedAnyObject>();
    let selection = gtk::NoSelection::new(Some(model.clone()));
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let row = gtk::Box::new(gtk::Orientation::Vertical, 0);
        row.add_css_class("preview-virtual-row");
        row.set_size_request(-1, VIRTUAL_ROW_MIN_HEIGHT);
        row.set_hexpand(true);
        item.set_child(Some(&row));
    });

    let state_for_bind = state.clone();
    factory.connect_bind(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(row) = item.child().and_downcast::<gtk::Box>() else {
            return;
        };
        clear_box(&row);
        let index = item.position() as usize;
        let Some(unit) = state_for_bind.units.get(index) else {
            return;
        };
        let bound = bind_unit(
            &row,
            unit,
            index,
            state_for_bind.units.len(),
            state_for_bind.units.clone(),
        );
        let mut bound_rows = state_for_bind.bound.borrow_mut();
        bound_rows.retain(|_, bound| {
            bound
                .root
                .upgrade()
                .is_some_and(|bound_row| bound_row != row)
        });
        bound_rows.insert(index, bound);
        drop(bound_rows);
        update_bound_selection(&state_for_bind, index);
        if let Some(hovered) = state_for_bind
            .hovered
            .get()
            .filter(|(unit, _)| *unit == index)
        {
            set_link_hover(&state_for_bind, hovered, true);
        }
    });

    let state_for_unbind = state.clone();
    factory.connect_unbind(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        if let Some(row) = item.child().and_downcast::<gtk::Box>() {
            state_for_unbind.bound.borrow_mut().retain(|_, bound| {
                bound
                    .root
                    .upgrade()
                    .is_some_and(|bound_row| bound_row != row)
            });
            clear_box(&row);
        }
    });

    let list = gtk::ListView::new(Some(selection), Some(factory));
    list.add_css_class("preview-virtual-list");
    list.add_css_class(if source {
        "preview-virtual-source"
    } else {
        "preview-document"
    });
    list.set_accessible_role(gtk::AccessibleRole::Document);
    list.set_focusable(true);
    list.set_hexpand(true);
    list.set_vexpand(true);
    let items = (0..state.units.len())
        .map(glib::BoxedAnyObject::new)
        .collect::<Vec<_>>();
    model.splice(0, 0, &items);

    let scroll = gtk::ScrolledWindow::builder()
        .child(&list)
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .hexpand(true)
        .vexpand(true)
        .build();
    install_pointer_selection(&list, &scroll, state.clone());
    install_keyboard_selection(&list, state.clone());

    let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    container.set_vexpand(true);
    for warning in warnings {
        container.append(&super::preview::document_notice(&warning));
    }
    container.append(&scroll);
    (container, state)
}

fn bind_unit(
    row: &gtk::Box,
    unit: &PreviewUnit,
    index: usize,
    unit_count: usize,
    units: Rc<Vec<PreviewUnit>>,
) -> BoundRow {
    row.set_margin_top(if index == 0 { 12 } else { 0 });
    row.set_margin_bottom(if index + 1 == unit_count { 20 } else { 0 });
    row.set_margin_start(0);
    row.set_margin_end(0);
    row.set_size_request(-1, VIRTUAL_ROW_MIN_HEIGHT);

    match unit {
        PreviewUnit::Document(DocumentUnit {
            kind: DocumentUnitKind::Gap,
            ..
        }) => {
            row.set_size_request(-1, 10);
            let bound = BoundRow::default();
            bound.root.set(Some(row));
            bound
        }
        PreviewUnit::Document(DocumentUnit {
            kind: DocumentUnitKind::Table { rows, list_depth },
            ..
        }) => {
            row.set_margin_start(16 + list_indent(*list_depth));
            row.set_margin_end(16);
            row.set_margin_bottom(if index + 1 == unit_count { 20 } else { 10 });
            let table = document_table(rows);
            row.append(&table);
            let bound = BoundRow::default();
            bound.root.set(Some(row));
            bound.table.set(Some(&table));
            bound
        }
        PreviewUnit::Document(unit) => {
            row.set_margin_bottom(if index + 1 == unit_count {
                20
            } else if !unit.last {
                0
            } else if document_unit_uses_list_gap(unit) {
                4
            } else {
                10
            });
            let view = document_text_view(unit);
            if matches!(unit.kind, DocumentUnitKind::Code { .. }) {
                let overlay = gtk::Overlay::new();
                overlay.add_css_class("preview-code-overlay");
                overlay.set_child(Some(&view));
                if unit.first {
                    overlay.add_overlay(&code_copy_button(units, index));
                }
                row.append(&overlay);
            } else {
                row.append(&view);
            }
            let bound = BoundRow::default();
            bound.root.set(Some(row));
            bound.view.set(Some(&view));
            bound
        }
        PreviewUnit::Source(unit) => {
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            line.add_css_class("preview-source-row");
            let numbers = source_line_numbers_view(unit);
            let view = plain_text_view(&unit.display, true);
            line.append(&numbers);
            line.append(&view);
            row.append(&line);
            let bound = BoundRow::default();
            bound.root.set(Some(row));
            bound.view.set(Some(&view));
            bound
        }
    }
}

fn source_line_numbers_view(unit: &SourceUnit) -> gtk::TextView {
    let text = source_line_numbers(unit);
    let buffer = gtk::TextBuffer::new(None);
    buffer.set_text(&text);
    let view = gtk::TextView::with_buffer(&buffer);
    view.set_editable(false);
    view.set_cursor_visible(false);
    view.set_accepts_tab(false);
    view.set_wrap_mode(gtk::WrapMode::None);
    view.set_can_target(false);
    view.set_focusable(false);
    view.set_accessible_role(gtk::AccessibleRole::Presentation);
    view.add_css_class("preview-source-lines");
    view.set_left_margin(2);
    view.set_right_margin(6);
    let (width, _) = view.create_pango_layout(Some(&text)).pixel_size();
    view.set_size_request(
        width.saturating_add(view.left_margin() + view.right_margin()),
        -1,
    );
    view
}

fn clear_box(box_: &gtk::Box) {
    while let Some(child) = box_.first_child() {
        box_.remove(&child);
    }
}

fn plain_text_view(text: &str, source: bool) -> super::document_view::DocumentTextView {
    let text = normalize_preview_text(text);
    let buffer = gtk::TextBuffer::new(None);
    buffer.set_text(&text);
    assert!(
        buffer
            .tag_table()
            .add(&gtk::TextTag::builder().name("document-selection").build())
    );
    super::theme::register_document_buffer(&buffer);
    let view = super::document_view::DocumentTextView::new(&buffer, Vec::new());
    view.set_editable(false);
    view.set_cursor_visible(false);
    view.set_accepts_tab(false);
    view.set_wrap_mode(gtk::WrapMode::None);
    view.set_left_margin(if source { 0 } else { 16 });
    view.set_right_margin(16);
    view.set_can_target(false);
    view.set_focusable(false);
    view.add_css_class(if source {
        "preview-text"
    } else {
        "preview-document"
    });
    super::theme::register_document_view(&view);
    let (text_width, _) = view.create_pango_layout(Some(&text)).pixel_size();
    view.set_size_request(text_width.saturating_add(view.right_margin()), -1);
    view.set_hexpand(true);
    view
}

fn document_text_view(unit: &DocumentUnit) -> super::document_view::DocumentTextView {
    let source_buffer = highlighted_code_language(unit).and_then(|language| {
        let language = sourceview5::LanguageManager::default().language(language)?;
        let buffer = sourceview5::Buffer::new(None);
        buffer.set_language(Some(&language));
        buffer.set_highlight_syntax(true);
        super::theme::register_source_buffer(&buffer);
        buffer.set_text(&normalize_preview_text(&unit.text));
        buffer.ensure_highlight(&buffer.start_iter(), &buffer.end_iter());
        Some(buffer)
    });
    let buffer = source_buffer.map_or_else(
        || {
            let buffer = gtk::TextBuffer::new(None);
            buffer.set_text(&normalize_preview_text(&unit.text));
            buffer
        },
        |buffer| buffer.upcast::<gtk::TextBuffer>(),
    );
    let tags = document_text_tags(&buffer);
    apply_document_unit_tags(&buffer, &tags, unit);
    super::theme::register_document_buffer(&buffer);

    let is_code = matches!(unit.kind, DocumentUnitKind::Code { .. });
    let code = is_code.then(|| super::document_view::DocumentCodeBlock {
        start: 0,
        end: buffer.char_count(),
        first: unit.first,
        last: unit.last,
    });
    let view = super::document_view::DocumentTextView::new(&buffer, code.into_iter().collect());
    view.set_editable(false);
    view.set_cursor_visible(false);
    view.set_accepts_tab(false);
    view.set_wrap_mode(if is_code || !unit.wrap {
        gtk::WrapMode::None
    } else {
        gtk::WrapMode::WordChar
    });
    view.set_left_margin(16);
    view.set_right_margin(16);
    view.set_can_target(false);
    view.set_focusable(false);
    view.add_css_class("preview-document");
    set_document_accessibility(&view, unit);
    super::theme::register_document_view(&view);
    view
}

fn highlighted_code_language(unit: &DocumentUnit) -> Option<&'static str> {
    match unit.kind {
        DocumentUnitKind::Code {
            language: Some(language),
            ..
        } if unit.first && unit.last => Some(language),
        _ => None,
    }
}

fn code_copy_button(units: Rc<Vec<PreviewUnit>>, index: usize) -> gtk::Button {
    let button = gtk::Button::builder()
        .tooltip_text("Copy code")
        .halign(gtk::Align::End)
        .valign(gtk::Align::Start)
        .build();
    button.add_css_class("preview-code-copy");
    button.set_has_frame(false);
    button.set_cursor_from_name(Some("pointer"));
    button.update_property(&[gtk::accessible::Property::Label("Copy code")]);
    let icon = crate::assets::primary_icon(crate::assets::icons::COPY, 15);
    button.set_child(Some(&icon));

    let feedback_generation = Rc::new(Cell::new(0_u64));
    button.connect_clicked(move |button| {
        let Some(text) = code_block_copy_text(&units, index) else {
            return;
        };
        button.clipboard().set_text(&text);
        let generation = feedback_generation.get().wrapping_add(1);
        feedback_generation.set(generation);
        crate::assets::set_primary_icon(&icon, crate::assets::icons::CHECK);
        button.set_tooltip_text(Some("Code copied"));
        button.update_property(&[gtk::accessible::Property::Label("Code copied")]);

        let weak_button = button.downgrade();
        let weak_icon = icon.downgrade();
        let feedback_generation = feedback_generation.clone();
        glib::timeout_add_local_once(Duration::from_secs(2), move || {
            if feedback_generation.get() != generation {
                return;
            }
            if let Some(icon) = weak_icon.upgrade() {
                crate::assets::set_primary_icon(&icon, crate::assets::icons::COPY);
            }
            if let Some(button) = weak_button.upgrade() {
                button.set_tooltip_text(Some("Copy code"));
                button.update_property(&[gtk::accessible::Property::Label("Copy code")]);
            }
        });
    });
    button
}

fn code_block_copy_text(units: &[PreviewUnit], index: usize) -> Option<String> {
    let code = |index| match units.get(index) {
        Some(PreviewUnit::Document(
            unit @ DocumentUnit {
                kind: DocumentUnitKind::Code { list_depth, .. },
                ..
            },
        )) => Some((unit, *list_depth)),
        _ => None,
    };
    let (_, depth) = code(index)?;
    let mut start = index;
    while !code(start)?.0.first {
        start = start.checked_sub(1)?;
        if code(start)?.1 != depth {
            return None;
        }
    }

    let mut text = String::new();
    for index in start..units.len() {
        let (unit, unit_depth) = code(index)?;
        if unit_depth != depth {
            return None;
        }
        text.push_str(&unit.copy_text);
        if unit.last {
            return Some(text);
        }
    }
    None
}

fn apply_document_unit_tags(
    buffer: &gtk::TextBuffer,
    tags: &DocumentTextTags,
    unit: &DocumentUnit,
) {
    let start = buffer.start_iter();
    let end = buffer.end_iter();
    let apply = |tag: &gtk::TextTag| buffer.apply_tag(tag, &start, &end);
    match &unit.kind {
        DocumentUnitKind::Heading(level) => {
            apply(&tags.headings[usize::from(*level).saturating_sub(1).min(5)]);
            apply(&tags.accent);
        }
        DocumentUnitKind::ListItem { depth } => {
            apply(if unit.first {
                document_list_tag(tags, *depth)
            } else {
                document_list_child_tag(tags, *depth)
            });
        }
        DocumentUnitKind::ListChild { depth, kind } => {
            apply(document_list_child_tag(tags, *depth));
            match kind {
                DocumentListChildKind::Heading(level) => {
                    apply(&tags.headings[usize::from(*level).saturating_sub(1).min(5)]);
                    apply(&tags.accent);
                }
                DocumentListChildKind::Quote => {
                    apply(&tags.quote);
                    apply(&tags.dim);
                }
                DocumentListChildKind::Paragraph => {}
                DocumentListChildKind::Code(_) => unreachable!(),
            }
        }
        DocumentUnitKind::Quote => {
            apply(&tags.quote);
            apply(&tags.dim);
        }
        DocumentUnitKind::Code { list_depth, .. } => {
            apply(&tags.code);
            if let Some(depth) = list_depth {
                apply(document_list_child_tag(tags, *depth));
            }
            if unit.first && buffer.char_count() > 0 {
                buffer.apply_tag(&tags.code_start, &start, &buffer.iter_at_offset(1));
            }
            if unit.last && buffer.char_count() > 0 {
                let mut last = end;
                last.set_line_offset(0);
                buffer.apply_tag(&tags.code_end, &last, &end);
            }
        }
        DocumentUnitKind::Rule { list_depth } => {
            apply(&tags.rule);
            apply(&tags.dim);
            if let Some(depth) = list_depth {
                apply(document_list_child_tag(tags, *depth));
            }
        }
        DocumentUnitKind::Paragraph | DocumentUnitKind::Table { .. } | DocumentUnitKind::Gap => {}
    }

    for span in &unit.spans {
        let start = buffer.iter_at_offset(i32::try_from(span.range.start).unwrap_or(i32::MAX));
        let end = buffer.iter_at_offset(i32::try_from(span.range.end).unwrap_or(i32::MAX));
        let tag = match span.style {
            DocumentSpanStyle::Accent => &tags.accent,
            DocumentSpanStyle::Bold => &tags.bold,
            DocumentSpanStyle::Italic => &tags.italic,
            DocumentSpanStyle::Strikethrough => &tags.strikethrough,
            DocumentSpanStyle::Monospace => &tags.monospace,
            DocumentSpanStyle::Underline => &tags.underline,
            DocumentSpanStyle::Link(_) => &tags.link,
        };
        buffer.apply_tag(tag, &start, &end);
        if matches!(span.style, DocumentSpanStyle::Link(_)) {
            buffer.apply_tag(&tags.accent, &start, &end);
        }
    }
}

fn set_document_accessibility(view: &super::document_view::DocumentTextView, unit: &DocumentUnit) {
    match unit.kind {
        DocumentUnitKind::Heading(level)
        | DocumentUnitKind::ListChild {
            kind: DocumentListChildKind::Heading(level),
            ..
        } => {
            view.set_accessible_role(gtk::AccessibleRole::Heading);
            view.update_property(&[gtk::accessible::Property::Level(i32::from(level))]);
        }
        DocumentUnitKind::ListItem { .. } => {
            view.set_accessible_role(gtk::AccessibleRole::ListItem);
        }
        _ => view.set_accessible_role(gtk::AccessibleRole::Generic),
    }
}

fn document_unit_uses_list_gap(unit: &DocumentUnit) -> bool {
    matches!(
        unit.kind,
        DocumentUnitKind::ListItem { .. }
            | DocumentUnitKind::ListChild { .. }
            | DocumentUnitKind::Code {
                list_depth: Some(_),
                ..
            }
            | DocumentUnitKind::Rule {
                list_depth: Some(_)
            }
            | DocumentUnitKind::Table {
                list_depth: Some(_),
                ..
            }
    )
}

fn list_indent(depth: Option<usize>) -> i32 {
    depth.map_or(0, |depth| 20 + depth.min(32) as i32 * 18)
}

fn document_text_tags(buffer: &gtk::TextBuffer) -> DocumentTextTags {
    let add = |tag: gtk::TextTag| {
        assert!(buffer.tag_table().add(&tag));
        tag
    };
    let scales = [1.6, 1.35, 1.18, 1.04, 1.04, 1.04];
    let headings = std::array::from_fn(|index| {
        add(gtk::TextTag::builder()
            .name(format!("document-heading-{}", index + 1))
            .weight(700)
            .scale(scales[index])
            .build())
    });
    let quote = add(gtk::TextTag::builder()
        .name("document-quote")
        .style(gtk::pango::Style::Italic)
        .left_margin(12)
        .right_margin(8)
        .line_height(1.2)
        .background_full_height(true)
        .build());
    let code = add(gtk::TextTag::builder()
        .name("document-code")
        .family("monospace")
        .indent(10)
        .right_margin(10)
        .line_height(1.15)
        .wrap_mode(gtk::WrapMode::None)
        .build());
    let code_start = add(gtk::TextTag::builder()
        .name("document-code-start")
        .pixels_above_lines(super::document_view::CODE_TOP_SPACE)
        .build());
    let code_end = add(gtk::TextTag::builder()
        .name("document-code-end")
        .pixels_below_lines(super::document_view::CODE_BOTTOM_SPACE)
        .build());
    let rule = add(gtk::TextTag::builder().name("document-rule").build());
    let lists = (0..=32)
        .map(|depth| {
            add(gtk::TextTag::builder()
                .name(format!("document-list-{depth}"))
                .left_margin(20 + depth * 18)
                .indent(-18)
                .build())
        })
        .collect();
    let list_children = (0..=32)
        .map(|depth| {
            add(gtk::TextTag::builder()
                .name(format!("document-list-child-{depth}"))
                .left_margin(20 + depth * 18)
                .build())
        })
        .collect();
    let dim = add(gtk::TextTag::builder().name("document-dim").build());
    let accent = add(gtk::TextTag::builder().name("document-accent").build());
    let link = add(gtk::TextTag::builder()
        .name("document-link")
        .underline(gtk::pango::Underline::Single)
        .build());
    let bold = add(gtk::TextTag::builder().weight(700).build());
    let italic = add(gtk::TextTag::builder()
        .style(gtk::pango::Style::Italic)
        .build());
    let strikethrough = add(gtk::TextTag::builder().strikethrough(true).build());
    let monospace = add(gtk::TextTag::builder().family("monospace").build());
    let underline = add(gtk::TextTag::builder()
        .underline(gtk::pango::Underline::Single)
        .build());
    add(gtk::TextTag::builder().name("document-link-hover").build());
    add(gtk::TextTag::builder().name("document-selection").build());
    DocumentTextTags {
        headings,
        quote,
        code,
        code_start,
        code_end,
        rule,
        lists,
        list_children,
        dim,
        accent,
        link,
        bold,
        italic,
        strikethrough,
        monospace,
        underline,
    }
}

fn document_list_tag(tags: &DocumentTextTags, depth: usize) -> &gtk::TextTag {
    &tags.lists[depth.min(tags.lists.len().saturating_sub(1))]
}

fn document_list_child_tag(tags: &DocumentTextTags, depth: usize) -> &gtk::TextTag {
    &tags.list_children[depth.min(tags.list_children.len().saturating_sub(1))]
}

fn document_table_widget() -> gtk::Grid {
    let table = gtk::Grid::builder()
        .column_homogeneous(true)
        .column_spacing(1)
        .row_spacing(1)
        .hexpand(true)
        .build();
    table.add_css_class("preview-document-table");
    table.set_accessible_role(gtk::AccessibleRole::Table);
    table.set_margin_top(super::document_view::DOCUMENT_PANEL_MARGIN);
    table.set_margin_bottom(super::document_view::DOCUMENT_PANEL_MARGIN);
    table
}

fn document_table(rows: &[Vec<DocumentTableCellLayout>]) -> gtk::Grid {
    let table = document_table_widget();
    for (row, cells) in rows.iter().enumerate() {
        for (column, cell) in cells.iter().enumerate() {
            let label = gtk::Label::new(None);
            label.add_css_class("preview-document-table-cell");
            label.set_use_markup(true);
            if cell.text.len() > TABLE_CELL_DISPLAY_BYTES {
                label.set_text(&format!(
                    "{}…",
                    bounded_text_prefix(&cell.text, TABLE_CELL_DISPLAY_BYTES)
                ));
                label.set_tooltip_text(Some(
                    "Cell shortened for responsive preview; copying the table keeps the complete text.",
                ));
            } else {
                label.set_markup(&styled_markup(&cell.text, &cell.spans));
            }
            label.set_wrap(true);
            label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            label.set_xalign(0.0);
            label.set_yalign(0.0);
            label.set_hexpand(true);
            label.connect_activate_link(|label, uri| {
                if has_web_scheme(uri) {
                    super::browser::open_location(&Location::uri(uri), label);
                }
                glib::Propagation::Stop
            });
            if cell.header {
                label.add_css_class("header");
                label.set_accessible_role(gtk::AccessibleRole::ColumnHeader);
            } else {
                label.set_accessible_role(gtk::AccessibleRole::Cell);
            }
            table.attach(&label, column as i32, row as i32, 1, 1);
        }
    }
    table
}

fn styled_markup(text: &str, spans: &[DocumentSpan]) -> String {
    let mut events = Vec::with_capacity(spans.len() * 2);
    for (index, span) in spans.iter().enumerate() {
        events.push((span.range.start, false, index));
        events.push((span.range.end, true, index));
    }
    events.sort_unstable_by_key(|(position, ending, index)| (*position, !*ending, *index));
    let boundaries = text
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(text.len()))
        .collect::<Vec<_>>();
    let mut output = String::with_capacity(text.len());
    let mut active = BTreeSet::<usize>::new();
    let mut cursor = 0;
    let mut event = 0;
    while event < events.len() {
        let position = events[event].0.min(boundaries.len().saturating_sub(1));
        output.push_str(&glib::markup_escape_text(
            &text[boundaries[cursor]..boundaries[position]],
        ));
        for index in active.iter().rev() {
            output.push_str(span_close(&spans[*index].style));
        }
        while event < events.len() && events[event].0 == position && events[event].1 {
            active.remove(&events[event].2);
            event += 1;
        }
        while event < events.len() && events[event].0 == position && !events[event].1 {
            active.insert(events[event].2);
            event += 1;
        }
        for index in &active {
            output.push_str(&span_open(&spans[*index].style));
        }
        cursor = position;
    }
    output.push_str(&glib::markup_escape_text(&text[boundaries[cursor]..]));
    for index in active.iter().rev() {
        output.push_str(span_close(&spans[*index].style));
    }
    output
}

fn span_open(style: &DocumentSpanStyle) -> String {
    match style {
        DocumentSpanStyle::Accent => String::new(),
        DocumentSpanStyle::Bold => "<b>".to_owned(),
        DocumentSpanStyle::Italic => "<i>".to_owned(),
        DocumentSpanStyle::Strikethrough => "<s>".to_owned(),
        DocumentSpanStyle::Monospace => "<tt>".to_owned(),
        DocumentSpanStyle::Underline => "<u>".to_owned(),
        DocumentSpanStyle::Link(uri) => {
            format!("<a href=\"{}\">", glib::markup_escape_text(uri))
        }
    }
}

fn span_close(style: &DocumentSpanStyle) -> &'static str {
    match style {
        DocumentSpanStyle::Accent => "",
        DocumentSpanStyle::Bold => "</b>",
        DocumentSpanStyle::Italic => "</i>",
        DocumentSpanStyle::Strikethrough => "</s>",
        DocumentSpanStyle::Monospace => "</tt>",
        DocumentSpanStyle::Underline => "</u>",
        DocumentSpanStyle::Link(_) => "</a>",
    }
}

fn install_pointer_selection(
    list: &gtk::ListView,
    scroll: &gtk::ScrolledWindow,
    state: Rc<VirtualPreviewState>,
) {
    let drag_claimed = Rc::new(Cell::new(false));
    let drag_allowed = Rc::new(Cell::new(false));
    let click = gtk::GestureClick::new();
    click.set_button(1);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak_list = list.downgrade();
    let state_for_press = state.clone();
    let drag_claimed_for_press = drag_claimed.clone();
    click.connect_pressed(move |_, _, x, y| {
        let Some(list) = weak_list.upgrade() else {
            return;
        };
        state_for_press.pressed_link.borrow_mut().take();
        if point_hits_class(&list, x, y, "preview-code-copy") {
            return;
        }
        state_for_press
            .pressed_link
            .replace(link_info_at(&list, &state_for_press, x, y).map(|(_, _, uri)| uri));
        drag_claimed_for_press.set(false);
        list.grab_focus();
        let point = point_at(&list, &state_for_press, x, y)
            .unwrap_or(SelectionPoint { unit: 0, offset: 0 });
        state_for_press.selection.set(Some(DocumentSelection {
            anchor: point,
            focus: point,
        }));
        update_all_bound_selection(&state_for_press);
    });

    let weak_list = list.downgrade();
    let state_for_release = state.clone();
    let drag_claimed_for_release = drag_claimed.clone();
    click.connect_released(move |_, _, x, y| {
        let pressed = state_for_release.pressed_link.borrow_mut().take();
        if drag_claimed_for_release.replace(false) {
            return;
        }
        let Some(list) = weak_list.upgrade() else {
            return;
        };
        let released = link_info_at(&list, &state_for_release, x, y).map(|(_, _, uri)| uri);
        if let Some(uri) = matching_link(pressed.as_deref(), released.as_deref())
            && has_web_scheme(uri)
        {
            super::browser::open_location(&Location::uri(uri), &list);
        }
    });
    list.add_controller(click);

    let drag = gtk::GestureDrag::new();
    drag.set_button(1);
    drag.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak_list = list.downgrade();
    let state_for_begin = state.clone();
    let drag_allowed_for_begin = drag_allowed.clone();
    drag.connect_drag_begin(move |_, x, y| {
        let Some(list) = weak_list.upgrade() else {
            return;
        };
        let allowed = !point_hits_class(&list, x, y, "preview-code-copy");
        drag_allowed_for_begin.set(allowed);
        if !allowed {
            return;
        }
        state_for_begin.press.set((x, y));
        state_for_begin.pointer.set((x, y));
    });

    let weak_list = list.downgrade();
    let weak_scroll = scroll.downgrade();
    let state_for_update = state.clone();
    let drag_allowed_for_update = drag_allowed.clone();
    let drag_claimed_for_update = drag_claimed.clone();
    drag.connect_drag_update(move |gesture, offset_x, offset_y| {
        let Some(list) = weak_list.upgrade() else {
            return;
        };
        let (start_x, start_y) = state_for_update.press.get();
        let (x, y) = (start_x + offset_x, start_y + offset_y);
        if !state_for_update.dragging.get() {
            if !drag_allowed_for_update.get()
                || !drag_threshold_crossed(&list, start_x, start_y, x, y)
            {
                return;
            }
            drag_claimed_for_update.set(true);
            gesture.set_state(gtk::EventSequenceState::Claimed);
            state_for_update.pressed_link.borrow_mut().take();
            list.grab_focus();
            let point = point_at(&list, &state_for_update, start_x, start_y)
                .unwrap_or(SelectionPoint { unit: 0, offset: 0 });
            state_for_update.selection.set(Some(DocumentSelection {
                anchor: point,
                focus: point,
            }));
            state_for_update.dragging.set(true);
            state_for_update
                .drag_generation
                .set(state_for_update.drag_generation.get().wrapping_add(1));
            update_all_bound_selection(&state_for_update);
            if let Some(scroll) = weak_scroll.upgrade() {
                start_autoscroll(&list, &scroll, &state_for_update);
            }
        }
        state_for_update.pointer.set((x, y));
        update_drag_focus(&list, &state_for_update, x, y);
    });

    let weak_list = list.downgrade();
    let state_for_end = state.clone();
    let drag_allowed_for_end = drag_allowed.clone();
    drag.connect_drag_end(move |_, offset_x, offset_y| {
        drag_allowed_for_end.set(false);
        if !state_for_end.dragging.get() {
            return;
        }
        let Some(list) = weak_list.upgrade() else {
            return;
        };
        let (start_x, start_y) = state_for_end.press.get();
        let (x, y) = (start_x + offset_x, start_y + offset_y);
        update_drag_focus(&list, &state_for_end, x, y);
        if let Some(text) = selection_text(&state_for_end) {
            list.primary_clipboard().set_text(&text);
        }
        state_for_end.dragging.set(false);
    });

    let state_for_cancel = state.clone();
    let drag_allowed_for_cancel = drag_allowed;
    drag.connect_cancel(move |_, _| {
        drag_allowed_for_cancel.set(false);
        state_for_cancel.pressed_link.borrow_mut().take();
        state_for_cancel.dragging.set(false);
        state_for_cancel
            .drag_generation
            .set(state_for_cancel.drag_generation.get().wrapping_add(1));
    });
    list.add_controller(drag);

    let motion = gtk::EventControllerMotion::new();
    motion.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak_list = list.downgrade();
    let state_for_motion = state.clone();
    motion.connect_motion(move |_, x, y| {
        if state_for_motion.dragging.get() {
            return;
        }
        let Some(list) = weak_list.upgrade() else {
            return;
        };
        let next = link_info_at(&list, &state_for_motion, x, y).map(|(unit, span, _)| (unit, span));
        update_link_hover(&state_for_motion, next);
        list.set_cursor_from_name(Some(if next.is_some() { "pointer" } else { "text" }));
    });
    let weak_list = list.downgrade();
    motion.connect_leave(move |_| {
        update_link_hover(&state, None);
        if let Some(list) = weak_list.upgrade() {
            list.set_cursor_from_name(Some("text"));
        }
    });
    list.add_controller(motion);
}

fn drag_threshold_crossed(
    widget: &impl IsA<gtk::Widget>,
    start_x: f64,
    start_y: f64,
    current_x: f64,
    current_y: f64,
) -> bool {
    widget.drag_check_threshold(
        start_x.round() as i32,
        start_y.round() as i32,
        current_x.round() as i32,
        current_y.round() as i32,
    )
}

fn point_hits_class(list: &gtk::ListView, x: f64, y: f64, class: &str) -> bool {
    let mut target = list.pick(x, y, gtk::PickFlags::DEFAULT);
    while let Some(widget) = target {
        if widget.has_css_class(class) {
            return true;
        }
        target = widget.parent();
    }
    false
}

fn install_keyboard_selection(list: &gtk::ListView, state: Rc<VirtualPreviewState>) {
    let keys = gtk::EventControllerKey::new();
    let weak_list = list.downgrade();
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        if !modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK) {
            return glib::Propagation::Proceed;
        }
        let Some(list) = weak_list.upgrade() else {
            return glib::Propagation::Proceed;
        };
        if key == gtk::gdk::Key::a {
            let Some(last) = state.units.len().checked_sub(1) else {
                return glib::Propagation::Stop;
            };
            state.selection.set(Some(DocumentSelection {
                anchor: SelectionPoint { unit: 0, offset: 0 },
                focus: SelectionPoint {
                    unit: last,
                    offset: state.units[last].selection_len(),
                },
            }));
            update_all_bound_selection(&state);
            return glib::Propagation::Stop;
        }
        if key == gtk::gdk::Key::c {
            if let Some(text) = selection_text(&state) {
                list.clipboard().set_text(&text);
            }
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    list.add_controller(keys);
}

fn start_autoscroll(
    list: &gtk::ListView,
    scroll: &gtk::ScrolledWindow,
    state: &Rc<VirtualPreviewState>,
) {
    let expected = state.drag_generation.get();
    let weak_state = Rc::downgrade(state);
    let weak_list = list.downgrade();
    let weak_scroll = scroll.downgrade();
    glib::timeout_add_local(Duration::from_millis(16), move || {
        let Some(state) = weak_state
            .upgrade()
            .filter(|state| state.dragging.get() && state.drag_generation.get() == expected)
        else {
            return glib::ControlFlow::Break;
        };
        let Some(list) = weak_list.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let Some(scroll) = weak_scroll.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let (_, y) = state.pointer.get();
        let height = f64::from(list.height());
        let delta = if y < AUTOSCROLL_EDGE {
            -((AUTOSCROLL_EDGE - y) * 0.35).min(24.0)
        } else if y > height - AUTOSCROLL_EDGE {
            ((y - (height - AUTOSCROLL_EDGE)) * 0.35).min(24.0)
        } else {
            0.0
        };
        if delta != 0.0 {
            let adjustment = scroll.vadjustment();
            let upper = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
            adjustment.set_value((adjustment.value() + delta).clamp(adjustment.lower(), upper));
            let (x, y) = state.pointer.get();
            update_drag_focus(&list, &state, x, y);
        }
        glib::ControlFlow::Continue
    });
}

fn update_drag_focus(list: &gtk::ListView, state: &VirtualPreviewState, x: f64, y: f64) {
    let Some(point) = point_at(list, state, x, y) else {
        return;
    };
    if let Some(mut selection) = state.selection.get() {
        selection.focus = point;
        state.selection.set(Some(selection));
        update_all_bound_selection(state);
    }
}

fn point_at(
    list: &gtk::ListView,
    state: &VirtualPreviewState,
    x: f64,
    y: f64,
) -> Option<SelectionPoint> {
    let point = gtk::graphene::Point::new(x as f32, y as f32);
    let bound = state.bound.borrow();
    let mut rows = bound
        .iter()
        .filter_map(|(index, bound)| {
            bound.root.upgrade().and_then(|root| {
                root.compute_bounds(list)
                    .map(|bounds| (*index, bound, bounds))
            })
        })
        .collect::<Vec<_>>();
    rows.sort_by_key(|(index, _, _)| *index);
    let y = y as f32;
    let (index, row, bounds) = rows
        .iter()
        .find(|(_, _, bounds)| bounds.y() <= y && y <= bounds.y() + bounds.height())
        .or_else(|| {
            rows.iter().min_by(|(_, _, left), (_, _, right)| {
                vertical_distance(left, y).total_cmp(&vertical_distance(right, y))
            })
        })?;
    let unit = state.units.get(*index)?;
    if y < bounds.y() || y > bounds.y() + bounds.height() {
        return Some(SelectionPoint {
            unit: *index,
            offset: if y < bounds.y() {
                0
            } else {
                unit.selection_len()
            },
        });
    }
    if row.table.upgrade().is_some() {
        return Some(SelectionPoint {
            unit: *index,
            offset: usize::from(x as f32 >= bounds.x() + bounds.width() / 2.0),
        });
    }
    let Some(view) = row.view.upgrade() else {
        return Some(SelectionPoint {
            unit: *index,
            offset: if y >= bounds.y() + bounds.height() / 2.0 {
                unit.selection_len()
            } else {
                0
            },
        });
    };
    let local = list.compute_point(&view, &point).unwrap_or_else(|| {
        gtk::graphene::Point::new(
            if x as f32 <= bounds.x() {
                0.0
            } else {
                view.width() as f32
            },
            (y - bounds.y()).clamp(0.0, view.height().saturating_sub(1) as f32),
        )
    });
    let (buffer_x, buffer_y) = view.window_to_buffer_coords(
        gtk::TextWindowType::Widget,
        local.x().round() as i32,
        local.y().round() as i32,
    );
    let offset = view
        .iter_at_position(buffer_x, buffer_y)
        .map_or_else(
            || {
                if y >= bounds.y() + bounds.height() / 2.0 {
                    unit.selection_len()
                } else {
                    0
                }
            },
            |(iter, trailing)| {
                usize::try_from(iter.offset().saturating_add(trailing)).unwrap_or(usize::MAX)
            },
        )
        .min(unit.selection_len());
    Some(SelectionPoint {
        unit: *index,
        offset,
    })
}

fn vertical_distance(bounds: &gtk::graphene::Rect, y: f32) -> f32 {
    if y < bounds.y() {
        bounds.y() - y
    } else {
        (y - (bounds.y() + bounds.height())).max(0.0)
    }
}

fn link_info_at(
    list: &gtk::ListView,
    state: &VirtualPreviewState,
    x: f64,
    y: f64,
) -> Option<(usize, usize, String)> {
    let point = link_point_at(list, state, x, y)?;
    let PreviewUnit::Document(unit) = state.units.get(point.unit)? else {
        return None;
    };
    unit.spans.iter().enumerate().find_map(|(index, span)| {
        (span.range.start <= point.offset && point.offset < span.range.end)
            .then(|| match &span.style {
                DocumentSpanStyle::Link(uri) => Some((point.unit, index, uri.to_string())),
                _ => None,
            })
            .flatten()
    })
}

fn link_point_at(
    list: &gtk::ListView,
    state: &VirtualPreviewState,
    x: f64,
    y: f64,
) -> Option<SelectionPoint> {
    let point = gtk::graphene::Point::new(x as f32, y as f32);
    let bound = state.bound.borrow();
    let (index, row, _) = bound.iter().find_map(|(index, row)| {
        let root = row.root.upgrade()?;
        let bounds = root.compute_bounds(list)?;
        (bounds.x() <= point.x()
            && point.x() < bounds.x() + bounds.width()
            && bounds.y() <= point.y()
            && point.y() < bounds.y() + bounds.height())
        .then_some((*index, row, bounds))
    })?;
    let view = row.view.upgrade()?;
    let local = list.compute_point(&view, &point)?;
    if local.x() < 0.0
        || local.x() >= view.width() as f32
        || local.y() < 0.0
        || local.y() >= view.height() as f32
    {
        return None;
    }
    let (buffer_x, buffer_y) = view.window_to_buffer_coords(
        gtk::TextWindowType::Widget,
        local.x().round() as i32,
        local.y().round() as i32,
    );
    let (iter, _) = view.iter_at_position(buffer_x, buffer_y)?;
    let glyph = view.iter_location(&iter);
    if buffer_x < glyph.x()
        || buffer_x >= glyph.x() + glyph.width().max(1)
        || buffer_y < glyph.y()
        || buffer_y >= glyph.y() + glyph.height()
    {
        return None;
    }
    Some(SelectionPoint {
        unit: index,
        offset: usize::try_from(iter.offset()).ok()?,
    })
}

fn matching_link<'a>(pressed: Option<&str>, released: Option<&'a str>) -> Option<&'a str> {
    released.filter(|released| pressed == Some(*released))
}

fn update_link_hover(state: &VirtualPreviewState, next: Option<(usize, usize)>) {
    let previous = state.hovered.replace(next);
    if previous == next {
        return;
    }
    if let Some(previous) = previous {
        set_link_hover(state, previous, false);
    }
    if let Some(next) = next {
        set_link_hover(state, next, true);
    }
}

fn set_link_hover(
    state: &VirtualPreviewState,
    (unit_index, span_index): (usize, usize),
    hovered: bool,
) {
    let Some(PreviewUnit::Document(unit)) = state.units.get(unit_index) else {
        return;
    };
    let Some(span) = unit.spans.get(span_index) else {
        return;
    };
    let bound = state.bound.borrow();
    let Some(view) = bound
        .get(&unit_index)
        .and_then(|bound| bound.view.upgrade())
    else {
        return;
    };
    let buffer = view.buffer();
    let Some(tag) = buffer.tag_table().lookup("document-link-hover") else {
        return;
    };
    let start = buffer.iter_at_offset(i32::try_from(span.range.start).unwrap_or(i32::MAX));
    let end = buffer.iter_at_offset(i32::try_from(span.range.end).unwrap_or(i32::MAX));
    if hovered {
        buffer.apply_tag(&tag, &start, &end);
    } else {
        buffer.remove_tag(&tag, &start, &end);
    }
}

fn update_all_bound_selection(state: &VirtualPreviewState) {
    let indices = state.bound.borrow().keys().copied().collect::<Vec<_>>();
    for index in indices {
        update_bound_selection(state, index);
    }
}

fn update_bound_selection(state: &VirtualPreviewState, index: usize) {
    let Some(unit) = state.units.get(index) else {
        return;
    };
    let range = local_selection(state.selection.get(), index, unit.selection_len());
    let bound = state.bound.borrow();
    let Some(bound) = bound.get(&index) else {
        return;
    };
    if let Some(view) = bound.view.upgrade() {
        let buffer = view.buffer();
        let buffer_start = buffer.start_iter();
        let buffer_end = buffer.end_iter();
        let selection_tag = buffer.tag_table().lookup("document-selection");
        if let Some(tag) = selection_tag.as_ref() {
            buffer.remove_tag(tag, &buffer_start, &buffer_end);
        }
        if let Some((start, end)) = range.filter(|(start, end)| start < end) {
            let start = i32::try_from(start).unwrap_or(i32::MAX);
            let end = i32::try_from(end).unwrap_or(i32::MAX);
            view.set_selection_range(Some((start, end)));
            if let Some(tag) = selection_tag.as_ref() {
                buffer.apply_tag(
                    tag,
                    &buffer.iter_at_offset(start),
                    &buffer.iter_at_offset(end),
                );
            }
        } else {
            view.set_selection_range(None);
        }
    }
    if let Some(table) = bound.table.upgrade() {
        if range.is_some_and(|(start, end)| start < end) {
            table.add_css_class("selected");
        } else {
            table.remove_css_class("selected");
        }
    }
}

fn local_selection(
    selection: Option<DocumentSelection>,
    unit: usize,
    len: usize,
) -> Option<(usize, usize)> {
    let (start, end) = selection?.normalized();
    if start == end || unit < start.unit || unit > end.unit {
        return None;
    }
    let local_start = if unit == start.unit { start.offset } else { 0 };
    let local_end = if unit == end.unit { end.offset } else { len };
    (local_start < local_end).then_some((local_start.min(len), local_end.min(len)))
}

fn selection_text(state: &VirtualPreviewState) -> Option<String> {
    let selection = state.selection.get()?;
    if selection.is_empty() {
        return None;
    }
    let (start, end) = selection.normalized();
    let mut output = String::new();
    for index in start.unit..=end.unit {
        let unit = state.units.get(index)?;
        let len = unit.selection_len();
        let from = if index == start.unit {
            start.offset.min(len)
        } else {
            0
        };
        let to = if index == end.unit {
            end.offset.min(len)
        } else {
            len
        };
        if from >= to {
            continue;
        }
        if from == 0 && to == len {
            output.push_str(unit.copy_text());
        } else {
            output.push_str(&char_slice(unit.display_text(), from, to));
            if to == len && index < end.unit && unit.copy_text().ends_with('\n') {
                output.push('\n');
            }
        }
    }
    Some(output)
}

fn char_slice(text: &str, start: usize, end: usize) -> String {
    text.chars().skip(start).take(end - start).collect()
}

fn source_units(content: &str) -> (Vec<SourceUnit>, bool) {
    let mut units = Vec::new();
    let mut group = String::new();
    let mut group_line = 1;
    let mut group_lines = 0;
    let mut split_lines = false;

    for (line_index, line) in content.split_inclusive('\n').enumerate() {
        let next_line = line_index + 1;
        let content_end = line.len() - usize::from(line.ends_with('\n'));
        if content_end > PATHOLOGICAL_TEXT_UNIT_BYTES {
            push_source_group(&mut units, &mut group, group_line, group_lines);
            group_lines = 0;
            push_source_line_chunks(&mut units, line, next_line);
            split_lines = true;
        } else {
            if !group.is_empty()
                && (group.len() + line.len() > SOURCE_UNIT_BYTES
                    || group_lines >= SOURCE_UNIT_LINES)
            {
                push_source_group(&mut units, &mut group, group_line, group_lines);
                group_line = next_line;
                group_lines = 0;
            }
            if group.is_empty() {
                group_line = next_line;
            }
            group.push_str(line);
            group_lines += 1;
        }
    }
    push_source_group(&mut units, &mut group, group_line, group_lines);
    if units.is_empty() {
        units.push(SourceUnit {
            display: String::new(),
            source: String::new(),
            first_line: 1,
            line_count: 1,
            continuation: false,
        });
    }
    (units, split_lines)
}

fn push_source_group(
    units: &mut Vec<SourceUnit>,
    group: &mut String,
    first_line: usize,
    line_count: usize,
) {
    if group.is_empty() {
        return;
    }
    let source = std::mem::take(group);
    let display = source.strip_suffix('\n').unwrap_or(&source).to_owned();
    units.push(SourceUnit {
        display,
        source,
        first_line,
        line_count,
        continuation: false,
    });
}

fn push_source_line_chunks(units: &mut Vec<SourceUnit>, line: &str, first_line: usize) {
    let content_end = line.len() - usize::from(line.ends_with('\n'));
    let mut start = 0;
    let mut continuation = false;
    while content_end - start > PATHOLOGICAL_TEXT_UNIT_BYTES {
        let remaining = content_end - start;
        let chunk_bytes = if remaining < PATHOLOGICAL_TEXT_UNIT_BYTES * 2 {
            remaining.div_ceil(2)
        } else {
            PATHOLOGICAL_TEXT_UNIT_BYTES
        };
        let mut end = start + chunk_bytes;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        units.push(SourceUnit {
            display: line[start..end].to_owned(),
            source: line[start..end].to_owned(),
            first_line,
            line_count: usize::from(!continuation),
            continuation,
        });
        start = end;
        continuation = true;
    }
    units.push(SourceUnit {
        display: line[start..]
            .strip_suffix('\n')
            .unwrap_or(&line[start..])
            .to_owned(),
        source: line[start..].to_owned(),
        first_line,
        line_count: usize::from(!continuation),
        continuation,
    });
}

fn source_line_numbers(unit: &SourceUnit) -> String {
    if unit.continuation {
        return format!("{}↳", unit.first_line);
    }
    (unit.first_line..unit.first_line + unit.line_count.max(1))
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn bounded_text_prefix(text: &str, bytes: usize) -> &str {
    let mut end = text.len().min(bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests;
