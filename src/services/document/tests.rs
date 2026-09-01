// SPDX-License-Identifier: GPL-3.0-or-later

use std::time::Duration;

use super::{
    DocumentBlock, DocumentKind, ParseLimits, document_kind, parse_document,
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
        DocumentBlock::TableRow { header: true, cells } if cells == &["A", "B"]
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
    assert!(debug.contains("TableRow { header: true"));
    assert!(debug.contains("TableRow { header: false"));
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
        "<html><body><p>unfinished</body></html>",
        &Cancellation::default(),
    )
    .expect_err("misnested HTML should fall back");
    assert!(malformed.contains("malformed"));
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
