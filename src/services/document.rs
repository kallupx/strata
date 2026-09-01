// SPDX-License-Identifier: GPL-3.0-or-later

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentKind {
    Markdown,
    Html,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DocumentBlock {
    Heading {
        level: u8,
        markup: String,
    },
    Paragraph(String),
    ListItem {
        marker: String,
        depth: usize,
        markup: String,
    },
    Quote(String),
    Code(String),
    Rule,
    TableRow {
        header: bool,
        cells: Vec<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Document {
    pub blocks: Vec<DocumentBlock>,
}

#[derive(Debug)]
enum ActiveBlock {
    Heading {
        level: u8,
        markup: String,
    },
    Paragraph(String),
    ListItem {
        marker: String,
        depth: usize,
        markup: String,
    },
    Code(String),
}

impl ActiveBlock {
    fn markup_mut(&mut self) -> &mut String {
        match self {
            Self::Heading { markup, .. }
            | Self::Paragraph(markup)
            | Self::ListItem { markup, .. }
            | Self::Code(markup) => markup,
        }
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn finish_block(active: &mut Option<ActiveBlock>, blocks: &mut Vec<DocumentBlock>) {
    let Some(active) = active.take() else {
        return;
    };
    blocks.push(match active {
        ActiveBlock::Heading { level, markup } => DocumentBlock::Heading { level, markup },
        ActiveBlock::Paragraph(markup) => DocumentBlock::Paragraph(markup),
        ActiveBlock::ListItem {
            marker,
            depth,
            markup,
        } => DocumentBlock::ListItem {
            marker,
            depth,
            markup,
        },
        ActiveBlock::Code(markup) => DocumentBlock::Code(markup),
    });
}

fn append_markup(active: &mut Option<ActiveBlock>, markup: &str) {
    active
        .get_or_insert_with(|| ActiveBlock::Paragraph(String::new()))
        .markup_mut()
        .push_str(markup);
}

fn append_escaped(active: &mut Option<ActiveBlock>, text: &str) {
    append_markup(active, &glib::markup_escape_text(text));
}

/// Parses the release-note Markdown subset into safe, balanced Pango markup.
pub fn parse_markdown(markdown: &str) -> Document {
    let mut blocks = Vec::new();
    let mut active = None;
    let mut links = Vec::new();
    let mut lists = Vec::<Option<u64>>::new();
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;

    for event in Parser::new_ext(markdown, options) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                finish_block(&mut active, &mut blocks);
                active = Some(ActiveBlock::Heading {
                    level: heading_level(level),
                    markup: String::new(),
                });
            }
            Event::End(TagEnd::Heading(_)) => finish_block(&mut active, &mut blocks),
            Event::Start(Tag::Paragraph) => {
                if active.is_none() {
                    active = Some(ActiveBlock::Paragraph(String::new()));
                }
            }
            Event::End(TagEnd::Paragraph) => {
                if matches!(active, Some(ActiveBlock::Paragraph(_))) {
                    finish_block(&mut active, &mut blocks);
                }
            }
            Event::Start(Tag::List(start)) => {
                if matches!(active, Some(ActiveBlock::ListItem { .. })) {
                    finish_block(&mut active, &mut blocks);
                }
                lists.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                if matches!(active, Some(ActiveBlock::ListItem { .. })) {
                    finish_block(&mut active, &mut blocks);
                }
                lists.pop();
            }
            Event::Start(Tag::Item) => {
                finish_block(&mut active, &mut blocks);
                let marker = match lists.last_mut() {
                    Some(Some(next)) => {
                        let marker = format!("{next}.");
                        *next = next.saturating_add(1);
                        marker
                    }
                    _ => "•".to_owned(),
                };
                active = Some(ActiveBlock::ListItem {
                    marker,
                    depth: lists.len().saturating_sub(1),
                    markup: String::new(),
                });
            }
            Event::End(TagEnd::Item) => {
                if matches!(active, Some(ActiveBlock::ListItem { .. })) {
                    finish_block(&mut active, &mut blocks);
                }
            }
            Event::Start(Tag::Emphasis) => append_markup(&mut active, "<i>"),
            Event::End(TagEnd::Emphasis) => append_markup(&mut active, "</i>"),
            Event::Start(Tag::Strong) => append_markup(&mut active, "<b>"),
            Event::End(TagEnd::Strong) => append_markup(&mut active, "</b>"),
            Event::Start(Tag::Strikethrough) => append_markup(&mut active, "<s>"),
            Event::End(TagEnd::Strikethrough) => append_markup(&mut active, "</s>"),
            Event::Start(Tag::Link { dest_url, .. }) => {
                let destination = dest_url.as_ref();
                let external = has_web_scheme(destination);
                links.push(external);
                if external {
                    append_markup(&mut active, "<a href=\"");
                    append_escaped(&mut active, destination);
                    append_markup(&mut active, "\">");
                } else {
                    append_markup(&mut active, "<u>");
                }
            }
            Event::End(TagEnd::Link) => append_markup(
                &mut active,
                if links.pop().unwrap_or(false) {
                    "</a>"
                } else {
                    "</u>"
                },
            ),
            Event::Start(Tag::Image { .. }) => append_markup(&mut active, "[Image: "),
            Event::End(TagEnd::Image) => append_markup(&mut active, "]"),
            Event::Start(Tag::CodeBlock(_)) => {
                if matches!(active, Some(ActiveBlock::ListItem { .. })) {
                    append_markup(&mut active, "<tt>");
                } else {
                    finish_block(&mut active, &mut blocks);
                    active = Some(ActiveBlock::Code(String::new()));
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if matches!(active, Some(ActiveBlock::Code(_))) {
                    finish_block(&mut active, &mut blocks);
                } else {
                    append_markup(&mut active, "</tt>");
                }
            }
            Event::Code(text) => {
                append_markup(&mut active, "<tt>");
                append_escaped(&mut active, &text);
                append_markup(&mut active, "</tt>");
            }
            Event::Text(text) => append_escaped(&mut active, &text),
            Event::SoftBreak | Event::HardBreak => append_markup(&mut active, "\n"),
            Event::Rule => {
                finish_block(&mut active, &mut blocks);
                blocks.push(DocumentBlock::Rule);
            }
            Event::Html(text) | Event::InlineHtml(text) => append_escaped(&mut active, &text),
            Event::TaskListMarker(checked) => {
                append_markup(&mut active, if checked { "☑ " } else { "☐ " });
            }
            _ => {}
        }
    }
    finish_block(&mut active, &mut blocks);
    Document { blocks }
}

pub fn has_web_scheme(uri: &str) -> bool {
    uri.split_once(':').is_some_and(|(scheme, _)| {
        scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
    })
}

#[cfg(test)]
mod tests;
