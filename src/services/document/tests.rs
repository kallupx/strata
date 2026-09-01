// SPDX-License-Identifier: GPL-3.0-or-later

use super::{DocumentBlock, parse_markdown};

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
