// SPDX-License-Identifier: GPL-3.0-or-later

use std::time::Duration;

use super::{
    DocumentBlock, DocumentKind, DocumentTableCell, ParseLimits, document_kind, parse_document,
    parse_document_with_limits, parse_markdown,
};
use crate::sandbox::Cancellation;

#[test]
fn markdown_renders_supported_formatting_as_blocks() {
    assert_eq!(
        parse_markdown("## Changes\n\n- **Fast** and `safe`\n- [Details](https://example.test)")
            .blocks,
        vec![
            DocumentBlock::Heading {
                level: 2,
                markup: "Changes".to_owned(),
            },
            DocumentBlock::ListItem {
                marker: "•".to_owned(),
                depth: 0,
                markup: "<b>Fast</b> and <tt>safe</tt>".to_owned(),
            },
            DocumentBlock::ListItem {
                marker: "•".to_owned(),
                depth: 0,
                markup: "<a href=\"https://example.test\">Details</a>".to_owned(),
            },
        ]
    );
}

#[test]
fn multiline_formatting_and_code_stay_in_balanced_blocks() {
    assert_eq!(
        parse_markdown("**first\nsecond**\n\n```text\none < two\n```").blocks,
        vec![
            DocumentBlock::Paragraph("<b>first\nsecond</b>".to_owned()),
            DocumentBlock::Code("one &lt; two\n".to_owned()),
        ]
    );
}

#[test]
fn nested_and_ordered_lists_keep_markers_and_depth() {
    assert_eq!(
        parse_markdown("3. outer\n   - inner\n4. next").blocks,
        vec![
            DocumentBlock::ListItem {
                marker: "3.".to_owned(),
                depth: 0,
                markup: "outer".to_owned(),
            },
            DocumentBlock::ListItem {
                marker: "•".to_owned(),
                depth: 1,
                markup: "inner".to_owned(),
            },
            DocumentBlock::ListItem {
                marker: "4.".to_owned(),
                depth: 0,
                markup: "next".to_owned(),
            },
        ]
    );
}

#[test]
fn markdown_keeps_html_inert_and_does_not_retain_image_urls() {
    let blocks = parse_markdown(
        "<script>alert('no')</script>\n\n![tracking](https://example.test/pixel.png)",
    )
    .blocks;
    let debug = format!("{blocks:?}");
    assert!(!debug.contains("<script>"));
    assert!(debug.contains("&lt;script&gt;"));
    assert!(!debug.contains("pixel.png"));
    assert!(debug.contains("[Image: tracking]"));
}

#[test]
fn markdown_does_not_activate_non_web_links() {
    assert_eq!(
        parse_markdown("[Run](javascript:alert('no'))").blocks,
        vec![DocumentBlock::Paragraph("<u>Run</u>".to_owned())]
    );
}

#[test]
fn malformed_markdown_and_entities_remain_inert() {
    let blocks = parse_markdown("<broken & **unfinished").blocks;
    let debug = format!("{blocks:?}");
    assert!(debug.contains("&lt;broken &amp;"));
    assert!(!debug.contains("<broken"));
}

#[test]
fn empty_markdown_has_no_blocks() {
    assert!(parse_markdown("  \n").blocks.is_empty());
}

#[test]
fn classifies_supported_local_document_mimes_and_extensions_case_insensitively() {
    for mime in [
        "text/markdown",
        "text/x-markdown",
        "TEXT/HTML",
        "application/xhtml+xml",
    ] {
        assert!(document_kind(mime, std::ffi::OsStr::new("file.txt"), true).is_some());
    }
    for name in [
        "README.md",
        "README.MARKDOWN",
        "a.mdown",
        "a.mkd",
        "a.mkdn",
        "a.mdwn",
        "a.HTML",
        "a.htm",
        "a.xhtml",
    ] {
        assert!(document_kind("text/plain", std::ffi::OsStr::new(name), true).is_some());
    }
    assert_eq!(
        document_kind("text/plain", std::ffi::OsStr::new("notes.md"), false),
        None
    );
    assert_eq!(
        document_kind("text/plain", std::ffi::OsStr::new("notes.txt"), true),
        None
    );
}

#[test]
fn markdown_supports_quotes_tables_tasks_strikethrough_and_safe_images() {
    let parsed = parse_document(
        DocumentKind::Markdown,
        "> quoted\n\n- [x] ~~done~~\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n![alt](file:///secret)",
        &Cancellation::default(),
    )
    .expect("supported Markdown should render");
    assert!(matches!(parsed.document.blocks[0], DocumentBlock::Quote(_)));
    assert!(format!("{:?}", parsed.document.blocks).contains("☑ <s>done</s>"));
    assert!(parsed.document.blocks.iter().any(|block| matches!(
        block,
        DocumentBlock::TableRow { cells }
            if cells == &[
                DocumentTableCell { header: true, markup: "A".to_owned() },
                DocumentTableCell { header: true, markup: "B".to_owned() },
            ]
    )));
    let debug = format!("{:?}", parsed.document.blocks);
    assert!(debug.contains("[Image: alt]"));
    assert!(!debug.contains("file:///secret"));
}

#[test]
fn markdown_raw_html_is_inert_and_warned() {
    let parsed = parse_document(
        DocumentKind::Markdown,
        "before <b onclick=\"run()\">after</b>",
        &Cancellation::default(),
    )
    .expect("raw HTML should remain inert text");
    let debug = format!("{:?}", parsed.document.blocks);
    assert!(debug.contains("&lt;b onclick=&quot;run()&quot;&gt;"));
    assert_eq!(parsed.warnings.len(), 1);
}

#[test]
fn html_supports_semantic_blocks_formatting_entities_lists_tables_and_breaks() {
    let parsed = parse_document(
        DocumentKind::Html,
        "<!doctype html><html><body><h2>Title &amp; more</h2><p><strong>Bold</strong><br><em>line</em></p><ol><li>one</li><li>two</li></ol><blockquote>quote</blockquote><pre>x &lt; y</pre><hr><table><thead><tr><th>A</th></tr></thead><tbody><tr><td>B</td></tr></tbody></table></body></html>",
        &Cancellation::default(),
    )
    .expect("supported HTML should render");
    let debug = format!("{:?}", parsed.document.blocks);
    assert!(debug.contains("Title &amp; more"));
    assert!(debug.contains("<b>Bold</b>\\n<i>line</i>"));
    assert!(debug.contains("marker: \"1.\""));
    assert!(debug.contains("Quote(\"quote\")"));
    assert!(debug.contains("Code(\"x &lt; y\")"));
    assert!(debug.contains("DocumentTableCell { header: true"));
    assert!(debug.contains("DocumentTableCell { header: false"));
    assert!(parsed.warnings.is_empty());
}

#[test]
fn html_omits_active_embedded_and_resource_content_without_retaining_urls() {
    let parsed = parse_document(
        DocumentKind::Html,
        "<html><body><p onclick=\"run()\"><a href=\"https://example.test\">safe</a> <a href=\"javascript:run()\">unsafe</a></p><script><img src=\"https://tracker.test/a\"></script><form action=\"https://submit.test\"><input></form><img src=\"file:///secret\"><video src=\"https://media.test/a\"></video><custom>kept</custom></body></html>",
        &Cancellation::default(),
    )
    .expect("safe mixed content should render");
    let debug = format!("{:?}", parsed.document.blocks);
    assert!(debug.contains("href=\\\"https://example.test\\\""));
    assert!(debug.contains("<u>unsafe</u>"));
    assert!(debug.contains("kept"));
    for omitted in [
        "javascript:",
        "tracker.test",
        "submit.test",
        "file:///",
        "media.test",
        "onclick",
    ] {
        assert!(!debug.contains(omitted), "must not retain {omitted}");
    }
    assert_eq!(parsed.warnings.len(), 1);
}

#[test]
fn html_contentless_and_malformed_documents_fall_back_to_source() {
    let contentless = parse_document(
        DocumentKind::Html,
        "<html><body><script>alert(1)</script></body></html>",
        &Cancellation::default(),
    )
    .expect_err("active-only HTML has no trustworthy rendering");
    assert!(contentless.contains("no supported document content"));

    let malformed = parse_document(
        DocumentKind::Html,
        "<p>text</span>",
        &Cancellation::default(),
    )
    .expect_err("an unmatched end tag should fall back");
    assert!(malformed.contains("malformed"));
}

#[test]
fn html_links_remain_balanced_when_they_contain_blocks() {
    let blocks = parse_document(
        DocumentKind::Html,
        "<a href=\"https://e\">lead<h2>Title</h2>tail</a>",
        &Cancellation::default(),
    )
    .expect("anchors may contain flow content")
    .document
    .blocks;
    assert_eq!(
        blocks,
        vec![
            DocumentBlock::Paragraph("<a href=\"https://e\">lead</a>".to_owned()),
            DocumentBlock::Heading {
                level: 2,
                markup: "<a href=\"https://e\">Title</a>".to_owned(),
            },
            DocumentBlock::Paragraph("<a href=\"https://e\">tail</a>".to_owned()),
        ]
    );
}

#[test]
fn html_never_publishes_unbalanced_pango_markup() {
    assert!(
        parse_document(
            DocumentKind::Html,
            "<em>lead<h2>Title</h2>tail</em>",
            &Cancellation::default(),
        )
        .expect_err("invalid phrasing-content nesting must fall back safely")
        .contains("unsupported document structure")
    );
}

#[test]
fn html_accepts_optional_end_tags_and_omitted_document_closures() {
    let parsed = parse_document(
        DocumentKind::Html,
        "<p>Hello, <ul><li>one<li>two</ul>",
        &Cancellation::default(),
    )
    .expect("optional paragraph and list-item end tags are valid");
    assert_eq!(
        parsed.document.blocks,
        vec![
            DocumentBlock::Paragraph("Hello, ".to_owned()),
            DocumentBlock::ListItem {
                marker: "•".to_owned(),
                depth: 0,
                markup: "one".to_owned(),
            },
            DocumentBlock::ListItem {
                marker: "•".to_owned(),
                depth: 0,
                markup: "two".to_owned(),
            },
        ]
    );

    assert!(
        parse_document(
            DocumentKind::Html,
            "<html><body><p>open document",
            &Cancellation::default(),
        )
        .is_ok()
    );
}

#[test]
fn html_paragraphs_inside_blockquotes_keep_quote_semantics() {
    assert_eq!(
        parse_document(
            DocumentKind::Html,
            "<blockquote><p>quote</p></blockquote>",
            &Cancellation::default(),
        )
        .expect("normal blockquote markup should render")
        .document
        .blocks,
        vec![DocumentBlock::Quote("quote".to_owned())]
    );
}

#[test]
fn html_paragraphs_inside_list_items_keep_list_semantics() {
    assert_eq!(
        parse_document(
            DocumentKind::Html,
            "<ul><li><p>one</p></li></ul>",
            &Cancellation::default(),
        )
        .expect("paragraphs are valid list-item content")
        .document
        .blocks,
        vec![DocumentBlock::ListItem {
            marker: "•".to_owned(),
            depth: 0,
            markup: "one".to_owned(),
        }]
    );
}

#[test]
fn separate_lists_and_tables_keep_container_boundaries() {
    let markdown = parse_document(
        DocumentKind::Markdown,
        "- one\n\n1. two",
        &Cancellation::default(),
    )
    .expect("separate Markdown lists should render");
    assert!(matches!(
        markdown.document.blocks.as_slice(),
        [
            DocumentBlock::ListItem { .. },
            DocumentBlock::ContainerBoundary,
            DocumentBlock::ListItem { .. }
        ]
    ));

    let html_lists = parse_document(
        DocumentKind::Html,
        "<ul><li>one</ul><ol><li>two</ol>",
        &Cancellation::default(),
    )
    .expect("separate HTML lists should render");
    assert!(matches!(
        html_lists.document.blocks.as_slice(),
        [
            DocumentBlock::ListItem { .. },
            DocumentBlock::ContainerBoundary,
            DocumentBlock::ListItem { .. }
        ]
    ));

    let markdown_tables = parse_document(
        DocumentKind::Markdown,
        "| A |\n| - |\n| 1 |\n\n| B |\n| - |\n| 2 |",
        &Cancellation::default(),
    )
    .expect("adjacent Markdown tables should render");
    assert!(matches!(
        markdown_tables.document.blocks.as_slice(),
        [
            DocumentBlock::TableRow { .. },
            DocumentBlock::TableRow { .. },
            DocumentBlock::ContainerBoundary,
            DocumentBlock::TableRow { .. },
            DocumentBlock::TableRow { .. }
        ]
    ));

    let html = parse_document(
        DocumentKind::Html,
        "<table><tr><td>A</table><table><tr><td>B</table>",
        &Cancellation::default(),
    )
    .expect("adjacent HTML tables should render");
    assert!(matches!(
        html.document.blocks.as_slice(),
        [
            DocumentBlock::TableRow { .. },
            DocumentBlock::ContainerBoundary,
            DocumentBlock::TableRow { .. }
        ]
    ));
}

#[test]
fn html_markup_limit_applies_while_links_are_reemitted() {
    let limits = ParseLimits {
        events: 2,
        markup: 128,
        ..ParseLimits::default()
    };
    let html = format!(
        "<a href=\"https://example.test/{}\">{}</a>",
        "x".repeat(64),
        "<p>x</p>".repeat(100)
    );
    assert!(
        parse_document_with_limits(DocumentKind::Html, &html, &Cancellation::default(), limits,)
            .expect_err("repeated link markup must stop at the output limit")
            .contains("markup limit")
    );
}

#[test]
fn html_closes_compact_table_rows_and_sections_without_losing_cells() {
    assert_eq!(
        parse_document(
            DocumentKind::Html,
            "<table><thead><tr><th>A<tbody><tr><td>B</table>",
            &Cancellation::default(),
        )
        .expect("optional table end tags are valid")
        .document
        .blocks,
        vec![
            DocumentBlock::TableRow {
                cells: vec![DocumentTableCell {
                    header: true,
                    markup: "A".to_owned(),
                }],
            },
            DocumentBlock::TableRow {
                cells: vec![DocumentTableCell {
                    header: false,
                    markup: "B".to_owned(),
                }],
            },
        ]
    );
}

#[test]
fn html_preserves_header_semantics_per_table_cell() {
    assert_eq!(
        parse_document(
            DocumentKind::Html,
            "<table><tr><th>Name</th><td>Alice</td></tr></table>",
            &Cancellation::default(),
        )
        .expect("th and td should retain distinct semantics")
        .document
        .blocks,
        vec![DocumentBlock::TableRow {
            cells: vec![
                DocumentTableCell {
                    header: true,
                    markup: "Name".to_owned(),
                },
                DocumentTableCell {
                    header: false,
                    markup: "Alice".to_owned(),
                },
            ],
        }]
    );
}

#[test]
fn html_closes_nested_content_before_an_optional_table_cell_end() {
    assert_eq!(
        parse_document(
            DocumentKind::Html,
            "<table><tr><td><p>A<td>B</table>",
            &Cancellation::default(),
        )
        .expect("a new cell should close nested content in the previous cell")
        .document
        .blocks,
        vec![DocumentBlock::TableRow {
            cells: vec![
                DocumentTableCell {
                    header: false,
                    markup: "A".to_owned(),
                },
                DocumentTableCell {
                    header: false,
                    markup: "B".to_owned(),
                },
            ],
        }]
    );
}

#[test]
fn html_pre_code_is_supported_without_an_omission_warning() {
    let parsed = parse_document(
        DocumentKind::Html,
        "<pre><code>x &lt; y</code></pre>",
        &Cancellation::default(),
    )
    .expect("canonical pre/code should render");
    assert_eq!(
        parsed.document.blocks,
        vec![DocumentBlock::Code("x &lt; y".to_owned())]
    );
    assert!(parsed.warnings.is_empty());
}

#[test]
fn parser_enforces_input_event_depth_widget_markup_time_and_cancellation_limits() {
    let cancellation = Cancellation::default();
    assert!(
        parse_document(
            DocumentKind::Markdown,
            &"x".repeat(1024 * 1024 + 1),
            &cancellation
        )
        .expect_err("oversized input")
        .contains("1 MB")
    );

    let many_events = "x\n\n".repeat(7_000);
    assert!(
        parse_document(DocumentKind::Markdown, &many_events, &cancellation)
            .expect_err("event-heavy input")
            .contains("parser-event")
    );

    let deep = format!("{}x{}", "<div>".repeat(33), "</div>".repeat(33));
    assert!(
        parse_document(DocumentKind::Html, &deep, &cancellation)
            .expect_err("deep input")
            .contains("nesting-depth")
    );

    let widgets = "x\n\n".repeat(513);
    assert!(
        parse_document(DocumentKind::Markdown, &widgets, &cancellation)
            .expect_err("widget-heavy input")
            .contains("widget")
    );

    let markup = "&".repeat(1024 * 1024);
    assert!(
        parse_document(DocumentKind::Markdown, &markup, &cancellation)
            .expect_err("escaped markup expansion")
            .contains("markup")
    );

    let zero_time = ParseLimits {
        time: Duration::ZERO,
        ..ParseLimits::default()
    };
    assert!(
        parse_document_with_limits(DocumentKind::Markdown, "text", &cancellation, zero_time)
            .expect_err("zero time budget")
            .contains("500 ms")
    );

    cancellation.cancel();
    assert!(
        parse_document(DocumentKind::Markdown, "text", &cancellation)
            .expect_err("cancelled parse")
            .contains("cancelled")
    );
}
