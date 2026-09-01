// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    ffi::OsStr,
    path::Path,
    time::{Duration, Instant},
};

use html5gum::{DefaultEmitter, Token, Tokenizer};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::sandbox::Cancellation;

pub const DOCUMENT_INPUT_LIMIT: usize = 1024 * 1024;
pub const DOCUMENT_EVENT_LIMIT: usize = 20_000;
pub const DOCUMENT_DEPTH_LIMIT: usize = 32;
pub const DOCUMENT_WIDGET_LIMIT: usize = 512;
pub const DOCUMENT_MARKUP_LIMIT: usize = 4 * 1024 * 1024;
pub const DOCUMENT_TIME_LIMIT: Duration = Duration::from_millis(500);

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedDocument {
    pub document: Document,
    pub warnings: Vec<String>,
}

#[derive(Clone, Copy)]
struct ParseLimits {
    events: usize,
    depth: usize,
    widgets: usize,
    markup: usize,
    time: Duration,
}

impl Default for ParseLimits {
    fn default() -> Self {
        Self {
            events: DOCUMENT_EVENT_LIMIT,
            depth: DOCUMENT_DEPTH_LIMIT,
            widgets: DOCUMENT_WIDGET_LIMIT,
            markup: DOCUMENT_MARKUP_LIMIT,
            time: DOCUMENT_TIME_LIMIT,
        }
    }
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
    Quote(String),
    Code(String),
}

impl ActiveBlock {
    fn markup(&self) -> &str {
        match self {
            Self::Heading { markup, .. }
            | Self::Paragraph(markup)
            | Self::ListItem { markup, .. }
            | Self::Quote(markup)
            | Self::Code(markup) => markup,
        }
    }

    fn markup_mut(&mut self) -> &mut String {
        match self {
            Self::Heading { markup, .. }
            | Self::Paragraph(markup)
            | Self::ListItem { markup, .. }
            | Self::Quote(markup)
            | Self::Code(markup) => markup,
        }
    }
}

struct ParseBudget<'a> {
    cancellation: &'a Cancellation,
    limits: ParseLimits,
    started: Instant,
    events: usize,
    depth: usize,
}

impl<'a> ParseBudget<'a> {
    fn new(cancellation: &'a Cancellation, limits: ParseLimits) -> Self {
        Self {
            cancellation,
            limits,
            started: Instant::now(),
            events: 0,
            depth: 0,
        }
    }

    fn event(&mut self) -> Result<(), String> {
        if self.cancellation.is_cancelled() {
            return Err("Rendered preview was cancelled".to_owned());
        }
        if self.started.elapsed() >= self.limits.time {
            return Err("Rendered preview exceeded the 500 ms parsing limit".to_owned());
        }
        self.events = self.events.saturating_add(1);
        if self.events > self.limits.events {
            return Err("Rendered preview exceeded the 20,000 parser-event limit".to_owned());
        }
        Ok(())
    }

    fn enter(&mut self) -> Result<(), String> {
        self.depth = self.depth.saturating_add(1);
        if self.depth > self.limits.depth {
            return Err("Rendered preview exceeded the nesting-depth limit of 32".to_owned());
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn nesting(&mut self, depth: usize) -> Result<(), String> {
        self.depth = depth;
        if self.depth > self.limits.depth {
            return Err("Rendered preview exceeded the nesting-depth limit of 32".to_owned());
        }
        Ok(())
    }
}

pub fn document_kind(content_type: &str, name: &OsStr, is_native: bool) -> Option<DocumentKind> {
    if !is_native {
        return None;
    }
    match content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "text/markdown" | "text/x-markdown" => return Some(DocumentKind::Markdown),
        "text/html" | "application/xhtml+xml" => return Some(DocumentKind::Html),
        _ => {}
    }
    match Path::new(name)
        .extension()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("md" | "markdown" | "mdown" | "mkd" | "mkdn" | "mdwn") => Some(DocumentKind::Markdown),
        Some("html" | "htm" | "xhtml") => Some(DocumentKind::Html),
        _ => None,
    }
}

pub fn parse_document(
    kind: DocumentKind,
    source: &str,
    cancellation: &Cancellation,
) -> Result<ParsedDocument, String> {
    if source.len() > DOCUMENT_INPUT_LIMIT {
        return Err("Rendered preview is limited to documents of 1 MB or less".to_owned());
    }
    parse_document_with_limits(kind, source, cancellation, ParseLimits::default())
}

fn parse_document_with_limits(
    kind: DocumentKind,
    source: &str,
    cancellation: &Cancellation,
    limits: ParseLimits,
) -> Result<ParsedDocument, String> {
    let parsed = match kind {
        DocumentKind::Markdown => parse_markdown_bounded(source, cancellation, limits, true),
        DocumentKind::Html => parse_html_bounded(source, cancellation, limits),
    }?;
    validate_document(parsed, limits)
}

/// Parses release notes through the same Markdown model without changing their legacy limits.
pub fn parse_markdown(markdown: &str) -> Document {
    let cancellation = Cancellation::default();
    let limits = ParseLimits {
        events: usize::MAX,
        depth: usize::MAX,
        widgets: usize::MAX,
        markup: usize::MAX,
        time: Duration::MAX,
    };
    parse_markdown_bounded(markdown, &cancellation, limits, false)
        .map(|parsed| parsed.document)
        .unwrap_or(Document { blocks: Vec::new() })
}

fn parse_markdown_bounded(
    markdown: &str,
    cancellation: &Cancellation,
    limits: ParseLimits,
    document_features: bool,
) -> Result<ParsedDocument, String> {
    let mut budget = ParseBudget::new(cancellation, limits);
    let mut blocks = Vec::new();
    let mut active = None;
    let mut links = Vec::new();
    let mut lists = Vec::<Option<u64>>::new();
    let mut quote_depth = 0usize;
    let mut table_header = false;
    let mut table_row: Option<Vec<String>> = None;
    let mut table_cell: Option<String> = None;
    let mut raw_html = false;
    let mut options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    if document_features {
        options |= Options::ENABLE_TABLES;
    }

    for event in Parser::new_ext(markdown, options) {
        budget.event()?;
        if matches!(event, Event::Start(_)) {
            budget.enter()?;
        }
        match &event {
            Event::Start(Tag::Heading { level, .. }) => {
                finish_block(&mut active, &mut blocks);
                active = Some(ActiveBlock::Heading {
                    level: heading_level(*level),
                    markup: String::new(),
                });
            }
            Event::End(TagEnd::Heading(_)) => finish_block(&mut active, &mut blocks),
            Event::Start(Tag::Paragraph) => {
                if active.is_none() {
                    active = Some(if quote_depth > 0 {
                        ActiveBlock::Quote(String::new())
                    } else {
                        ActiveBlock::Paragraph(String::new())
                    });
                }
            }
            Event::End(TagEnd::Paragraph) => {
                if matches!(
                    active,
                    Some(ActiveBlock::Paragraph(_) | ActiveBlock::Quote(_))
                ) {
                    finish_block(&mut active, &mut blocks);
                }
            }
            Event::Start(Tag::BlockQuote(_)) => {
                if document_features {
                    finish_block(&mut active, &mut blocks);
                    quote_depth = quote_depth.saturating_add(1);
                    active = Some(ActiveBlock::Quote(String::new()));
                }
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                if document_features {
                    finish_block(&mut active, &mut blocks);
                    quote_depth = quote_depth.saturating_sub(1);
                }
            }
            Event::Start(Tag::List(start)) => {
                if matches!(active, Some(ActiveBlock::ListItem { .. })) {
                    finish_block(&mut active, &mut blocks);
                }
                lists.push(*start);
            }
            Event::End(TagEnd::List(_)) => {
                if matches!(active, Some(ActiveBlock::ListItem { .. })) {
                    finish_block(&mut active, &mut blocks);
                }
                lists.pop();
            }
            Event::Start(Tag::Item) => {
                finish_block(&mut active, &mut blocks);
                active = Some(ActiveBlock::ListItem {
                    marker: next_list_marker(&mut lists),
                    depth: lists.len().saturating_sub(1),
                    markup: String::new(),
                });
            }
            Event::End(TagEnd::Item) => {
                if matches!(active, Some(ActiveBlock::ListItem { .. })) {
                    finish_block(&mut active, &mut blocks);
                }
            }
            Event::Start(Tag::Table(_)) | Event::End(TagEnd::Table) => {
                finish_block(&mut active, &mut blocks);
            }
            Event::Start(Tag::TableHead) => {
                table_header = true;
                table_row = Some(Vec::new());
            }
            Event::End(TagEnd::TableHead) => {
                if let Some(cells) = table_row.take() {
                    blocks.push(DocumentBlock::TableRow {
                        header: true,
                        cells,
                    });
                }
                table_header = false;
            }
            Event::Start(Tag::TableRow) => table_row = Some(Vec::new()),
            Event::End(TagEnd::TableRow) => {
                if let Some(cells) = table_row.take() {
                    blocks.push(DocumentBlock::TableRow {
                        header: table_header,
                        cells,
                    });
                }
            }
            Event::Start(Tag::TableCell) => table_cell = Some(String::new()),
            Event::End(TagEnd::TableCell) => {
                if let Some(cell) = table_cell.take() {
                    table_row.get_or_insert_with(Vec::new).push(cell);
                }
            }
            Event::Start(Tag::Emphasis) => append_markup(&mut active, &mut table_cell, "<i>"),
            Event::End(TagEnd::Emphasis) => append_markup(&mut active, &mut table_cell, "</i>"),
            Event::Start(Tag::Strong) => append_markup(&mut active, &mut table_cell, "<b>"),
            Event::End(TagEnd::Strong) => append_markup(&mut active, &mut table_cell, "</b>"),
            Event::Start(Tag::Strikethrough) => {
                append_markup(&mut active, &mut table_cell, "<s>");
            }
            Event::End(TagEnd::Strikethrough) => {
                append_markup(&mut active, &mut table_cell, "</s>");
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                let destination = dest_url.as_ref();
                let external = has_web_scheme(destination);
                links.push(external);
                if external {
                    append_markup(&mut active, &mut table_cell, "<a href=\"");
                    append_escaped(&mut active, &mut table_cell, destination);
                    append_markup(&mut active, &mut table_cell, "\">");
                } else {
                    append_markup(&mut active, &mut table_cell, "<u>");
                }
            }
            Event::End(TagEnd::Link) => append_markup(
                &mut active,
                &mut table_cell,
                if links.pop().unwrap_or(false) {
                    "</a>"
                } else {
                    "</u>"
                },
            ),
            Event::Start(Tag::Image { .. }) => {
                append_markup(&mut active, &mut table_cell, "[Image: ");
            }
            Event::End(TagEnd::Image) => append_markup(&mut active, &mut table_cell, "]"),
            Event::Start(Tag::CodeBlock(_)) => {
                if matches!(
                    active,
                    Some(ActiveBlock::ListItem { .. } | ActiveBlock::Quote(_))
                ) {
                    append_markup(&mut active, &mut table_cell, "<tt>");
                } else {
                    finish_block(&mut active, &mut blocks);
                    active = Some(ActiveBlock::Code(String::new()));
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if matches!(active, Some(ActiveBlock::Code(_))) {
                    finish_block(&mut active, &mut blocks);
                } else {
                    append_markup(&mut active, &mut table_cell, "</tt>");
                }
            }
            Event::Code(text) => {
                append_markup(&mut active, &mut table_cell, "<tt>");
                append_escaped(&mut active, &mut table_cell, text);
                append_markup(&mut active, &mut table_cell, "</tt>");
            }
            Event::Text(text) => append_escaped(&mut active, &mut table_cell, text),
            Event::SoftBreak | Event::HardBreak => {
                append_markup(&mut active, &mut table_cell, "\n");
            }
            Event::Rule => {
                finish_block(&mut active, &mut blocks);
                blocks.push(DocumentBlock::Rule);
            }
            Event::Html(text) | Event::InlineHtml(text) => {
                raw_html = true;
                append_escaped(&mut active, &mut table_cell, text);
            }
            Event::TaskListMarker(checked) => append_markup(
                &mut active,
                &mut table_cell,
                if *checked { "☑ " } else { "☐ " },
            ),
            Event::Start(_)
            | Event::End(_)
            | Event::FootnoteReference(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_) => {}
        }
        if matches!(event, Event::End(_)) {
            budget.leave();
        }
    }
    finish_block(&mut active, &mut blocks);
    Ok(ParsedDocument {
        document: Document { blocks },
        warnings: raw_html
            .then(|| "Raw HTML is shown as inert text in Markdown previews.".to_owned())
            .into_iter()
            .collect(),
    })
}

struct HtmlState {
    blocks: Vec<DocumentBlock>,
    active: Option<ActiveBlock>,
    lists: Vec<Option<u64>>,
    links: Vec<Option<String>>,
    stack: Vec<String>,
    skipped_depth: usize,
    quote_depth: usize,
    table_header: bool,
    table_row_header: bool,
    table_row: Option<Vec<String>>,
    table_cell: Option<String>,
    markup_remaining: usize,
    markup_exceeded: bool,
    preformatted: bool,
    warned: bool,
    malformed: bool,
}

impl HtmlState {
    fn new(markup_limit: usize) -> Self {
        Self {
            blocks: Vec::new(),
            active: None,
            lists: Vec::new(),
            links: Vec::new(),
            stack: Vec::new(),
            skipped_depth: 0,
            quote_depth: 0,
            table_header: false,
            table_row_header: false,
            table_row: None,
            table_cell: None,
            markup_remaining: markup_limit,
            markup_exceeded: false,
            preformatted: false,
            warned: false,
            malformed: false,
        }
    }

    fn start(&mut self, name: &str, href: Option<&str>, self_closing: bool) {
        let void = is_void_html_tag(name) || self_closing;
        if self.skipped_depth > 0 {
            if !void {
                self.stack.push(name.to_owned());
                self.skipped_depth = self.skipped_depth.saturating_add(1);
            }
            return;
        }

        self.close_implied_before_start(name);
        if is_omitted_html_tag(name) {
            self.warned = true;
            if !void {
                self.stack.push(name.to_owned());
                self.skipped_depth = 1;
            }
            return;
        }

        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.finish_active();
                self.start_active(ActiveBlock::Heading {
                    level: name[1..].parse().unwrap_or(6),
                    markup: String::new(),
                });
            }
            "p" => {
                self.finish_active();
                self.start_active(if self.quote_depth > 0 {
                    ActiveBlock::Quote(String::new())
                } else {
                    ActiveBlock::Paragraph(String::new())
                });
            }
            "blockquote" => {
                self.finish_active();
                self.quote_depth = self.quote_depth.saturating_add(1);
                self.start_active(ActiveBlock::Quote(String::new()));
            }
            "ul" => {
                self.finish_active();
                self.lists.push(None);
            }
            "ol" => {
                self.finish_active();
                self.lists.push(Some(1));
            }
            "li" => {
                self.finish_active();
                let marker = next_list_marker(&mut self.lists);
                let depth = self.lists.len().saturating_sub(1);
                self.start_active(ActiveBlock::ListItem {
                    marker,
                    depth,
                    markup: String::new(),
                });
            }
            "em" | "i" => self.append_markup("<i>"),
            "strong" | "b" => self.append_markup("<b>"),
            "s" | "del" => self.append_markup("<s>"),
            "a" => {
                self.ensure_text_target();
                let destination = href.filter(|href| has_web_scheme(href)).map(str::to_owned);
                if href.is_some() && destination.is_none() {
                    self.warned = true;
                }
                self.append_link_open(destination.as_deref());
                self.links.push(destination);
            }
            "pre" => {
                self.finish_active();
                self.start_active(ActiveBlock::Code(String::new()));
                self.preformatted = true;
            }
            "code" if !self.preformatted => {
                self.append_markup("<tt>");
            }
            "hr" => {
                self.finish_active();
                self.blocks.push(DocumentBlock::Rule);
            }
            "br" => {
                self.ensure_text_target();
                self.append_markup("\n");
            }
            "table" => self.finish_active(),
            "thead" => self.table_header = true,
            "tr" => {
                self.table_row_header = false;
                self.table_row = Some(Vec::new());
            }
            "th" | "td" => {
                self.table_row_header |= name == "th";
                self.table_cell = Some(String::new());
                self.append_link_openings();
            }
            "main" | "article" | "section" | "header" | "footer" | "nav" | "aside" | "div" => {
                self.finish_active();
            }
            "html" | "body" | "span" | "tbody" | "tfoot" => {}
            _ => self.warned = true,
        }

        if !void {
            self.stack.push(name.to_owned());
        } else if self_closing {
            self.end_supported(name);
        }
    }

    fn end(&mut self, name: &str) {
        let Some(position) = self.stack.iter().rposition(|open| open == name) else {
            self.malformed = true;
            return;
        };
        while self.stack.len() > position {
            self.close_top();
        }
    }

    fn end_supported(&mut self, name: &str) {
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "p" | "li" => {
                self.finish_active();
            }
            "blockquote" => {
                self.finish_active();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            "ul" | "ol" => {
                if matches!(self.active, Some(ActiveBlock::ListItem { .. })) {
                    self.finish_active();
                }
                self.lists.pop();
            }
            "em" | "i" => self.append_markup("</i>"),
            "strong" | "b" => self.append_markup("</b>"),
            "s" | "del" => self.append_markup("</s>"),
            "a" => {
                if let Some(link) = self.links.pop() {
                    self.append_link_close(link.is_some());
                }
            }
            "pre" => {
                self.finish_active();
                self.preformatted = false;
            }
            "code" if !self.preformatted => {
                self.append_markup("</tt>");
            }
            "thead" => self.table_header = false,
            "tr" => {
                if let Some(cells) = self.table_row.take() {
                    self.blocks.push(DocumentBlock::TableRow {
                        header: self.table_header || self.table_row_header,
                        cells,
                    });
                }
            }
            "th" | "td" => {
                self.append_link_closings();
                if let Some(cell) = self.table_cell.take() {
                    self.table_row.get_or_insert_with(Vec::new).push(cell);
                }
            }
            "main" | "article" | "section" | "header" | "footer" | "nav" | "aside" | "div" => {
                self.finish_active();
            }
            _ => {}
        }
    }

    fn text(&mut self, text: &str) {
        if self.skipped_depth > 0 || (self.active.is_none() && text.trim().is_empty()) {
            return;
        }
        self.ensure_text_target();
        self.append_escaped(text);
    }

    fn start_active(&mut self, active: ActiveBlock) {
        self.active = Some(active);
        self.append_link_openings();
    }

    fn ensure_text_target(&mut self) {
        if self.active.is_none() && self.table_cell.is_none() {
            self.start_active(if self.quote_depth > 0 {
                ActiveBlock::Quote(String::new())
            } else {
                ActiveBlock::Paragraph(String::new())
            });
        }
    }

    fn finish_active(&mut self) {
        if self.active.is_some() {
            self.append_link_closings();
            finish_block(&mut self.active, &mut self.blocks);
        }
    }

    fn append_link_openings(&mut self) {
        for link in self.links.clone() {
            self.append_link_open(link.as_deref());
            if self.markup_exceeded {
                break;
            }
        }
    }

    fn append_link_closings(&mut self) {
        for index in (0..self.links.len()).rev() {
            self.append_link_close(self.links[index].is_some());
            if self.markup_exceeded {
                break;
            }
        }
    }

    fn append_markup(&mut self, markup: &str) {
        if self.markup_exceeded || markup.len() > self.markup_remaining {
            self.markup_exceeded = true;
            return;
        }
        self.markup_remaining -= markup.len();
        append_markup(&mut self.active, &mut self.table_cell, markup);
    }

    fn append_escaped(&mut self, text: &str) {
        self.append_markup(&glib::markup_escape_text(text));
    }

    fn append_link_open(&mut self, destination: Option<&str>) {
        if let Some(destination) = destination {
            self.append_markup("<a href=\"");
            self.append_escaped(destination);
            self.append_markup("\">");
        } else {
            self.append_markup("<u>");
        }
    }

    fn append_link_close(&mut self, external: bool) {
        if self.active.is_some() || self.table_cell.is_some() {
            self.append_markup(if external { "</a>" } else { "</u>" });
        }
    }

    fn check_markup(&self) -> Result<(), String> {
        if self.markup_exceeded {
            Err("Rendered preview exceeded the 4 MB markup limit".to_owned())
        } else {
            Ok(())
        }
    }

    fn close_top(&mut self) {
        let Some(open) = self.stack.pop() else {
            return;
        };
        if self.skipped_depth > 0 {
            self.skipped_depth -= 1;
        } else {
            self.end_supported(&open);
        }
    }

    fn close_implied_before_start(&mut self, incoming: &str) {
        while self
            .stack
            .last()
            .is_some_and(|open| html_start_implies_end(open, incoming))
        {
            self.close_top();
        }
    }

    fn finish(mut self) -> Self {
        while !self.stack.is_empty() {
            self.close_top();
        }
        self.finish_active();
        self
    }
}

fn parse_html_bounded(
    html: &str,
    cancellation: &Cancellation,
    limits: ParseLimits,
) -> Result<ParsedDocument, String> {
    let mut budget = ParseBudget::new(cancellation, limits);
    let mut state = HtmlState::new(limits.markup);
    let mut emitter = DefaultEmitter::default();
    emitter.naively_switch_states(true);
    for token in Tokenizer::new_with_emitter(html, emitter) {
        budget.event()?;
        let token = token.map_err(|_| "HTML tokenization failed".to_owned())?;
        match token {
            Token::StartTag(tag) => {
                let name = String::from_utf8_lossy(&tag.name).to_ascii_lowercase();
                let mut href = None;
                for (attribute, value) in &tag.attributes {
                    let attribute = String::from_utf8_lossy(attribute).to_ascii_lowercase();
                    if name == "a" && attribute == "href" {
                        href = Some(String::from_utf8_lossy(&value.value).into_owned());
                    } else {
                        state.warned = true;
                    }
                }
                state.start(&name, href.as_deref(), tag.self_closing);
                state.check_markup()?;
                budget.nesting(state.stack.len())?;
            }
            Token::EndTag(tag) => {
                let name = String::from_utf8_lossy(&tag.name).to_ascii_lowercase();
                state.end(&name);
                state.check_markup()?;
                budget.nesting(state.stack.len())?;
            }
            Token::String(text) => {
                state.text(&String::from_utf8_lossy(&text.value));
                state.check_markup()?;
            }
            Token::Error(_) => state.malformed = true,
            Token::Comment(_) | Token::Doctype(_) => {}
        }
    }
    let state = state.finish();
    state.check_markup()?;
    if state.malformed {
        return Err("Rendered preview is unavailable because the HTML is malformed".to_owned());
    }
    Ok(ParsedDocument {
        document: Document {
            blocks: state.blocks,
        },
        warnings: state
            .warned
            .then(|| "Unsupported or active HTML content was omitted.".to_owned())
            .into_iter()
            .collect(),
    })
}

fn validate_document(
    parsed: ParsedDocument,
    limits: ParseLimits,
) -> Result<ParsedDocument, String> {
    if parsed.document.blocks.is_empty() {
        return Err("Rendered preview found no supported document content".to_owned());
    }
    let widgets = parsed
        .document
        .blocks
        .iter()
        .map(|block| match block {
            DocumentBlock::TableRow { cells, .. } => cells.len(),
            _ => 1,
        })
        .sum::<usize>();
    if widgets > limits.widgets {
        return Err("Rendered preview exceeded the 512-widget limit".to_owned());
    }
    let markup = parsed
        .document
        .blocks
        .iter()
        .map(block_markup_bytes)
        .sum::<usize>();
    if markup > limits.markup {
        return Err("Rendered preview exceeded the 4 MB markup limit".to_owned());
    }
    if parsed
        .document
        .blocks
        .iter()
        .any(|block| !block_has_balanced_markup(block))
    {
        return Err("Rendered preview contains unsupported document structure".to_owned());
    }
    Ok(parsed)
}

fn block_has_balanced_markup(block: &DocumentBlock) -> bool {
    match block {
        DocumentBlock::Heading { markup, .. }
        | DocumentBlock::Paragraph(markup)
        | DocumentBlock::ListItem { markup, .. }
        | DocumentBlock::Quote(markup)
        | DocumentBlock::Code(markup) => has_balanced_markup(markup),
        DocumentBlock::TableRow { cells, .. } => cells.iter().all(|cell| has_balanced_markup(cell)),
        DocumentBlock::Rule => true,
    }
}

fn has_balanced_markup(markup: &str) -> bool {
    let mut tags = Vec::new();
    let mut remaining = markup;
    while let Some(start) = remaining.find('<') {
        let Some(end) = remaining[start + 1..].find('>') else {
            return false;
        };
        let tag = &remaining[start + 1..start + 1 + end];
        if let Some(closing) = tag.strip_prefix('/') {
            if tags.pop() != Some(closing) {
                return false;
            }
        } else {
            let name = if tag.starts_with("a href=\"") && tag.ends_with('"') {
                "a"
            } else if matches!(tag, "i" | "b" | "s" | "tt" | "u") {
                tag
            } else {
                return false;
            };
            tags.push(name);
        }
        remaining = &remaining[start + end + 2..];
    }
    tags.is_empty()
}

fn block_markup_bytes(block: &DocumentBlock) -> usize {
    match block {
        DocumentBlock::Heading { markup, .. }
        | DocumentBlock::Paragraph(markup)
        | DocumentBlock::ListItem { markup, .. }
        | DocumentBlock::Quote(markup)
        | DocumentBlock::Code(markup) => markup.len(),
        DocumentBlock::TableRow { cells, .. } => cells.iter().map(String::len).sum(),
        DocumentBlock::Rule => 0,
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    level as u8
}

fn next_list_marker(lists: &mut [Option<u64>]) -> String {
    match lists.last_mut() {
        Some(Some(next)) => {
            let marker = format!("{next}.");
            *next = next.saturating_add(1);
            marker
        }
        _ => "•".to_owned(),
    }
}

fn finish_block(active: &mut Option<ActiveBlock>, blocks: &mut Vec<DocumentBlock>) {
    let Some(active) = active.take() else {
        return;
    };
    if !has_visible_markup(active.markup()) {
        return;
    }
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
        ActiveBlock::Quote(markup) => DocumentBlock::Quote(markup),
        ActiveBlock::Code(markup) => DocumentBlock::Code(markup),
    });
}

fn has_visible_markup(markup: &str) -> bool {
    let mut in_tag = false;
    markup.chars().any(|character| match character {
        '<' => {
            in_tag = true;
            false
        }
        '>' if in_tag => {
            in_tag = false;
            false
        }
        _ => !in_tag && !character.is_whitespace(),
    })
}

fn append_markup(active: &mut Option<ActiveBlock>, cell: &mut Option<String>, markup: &str) {
    if let Some(cell) = cell {
        cell.push_str(markup);
    } else {
        active
            .get_or_insert_with(|| ActiveBlock::Paragraph(String::new()))
            .markup_mut()
            .push_str(markup);
    }
}

fn append_escaped(active: &mut Option<ActiveBlock>, cell: &mut Option<String>, text: &str) {
    append_markup(active, cell, &glib::markup_escape_text(text));
}

fn html_start_implies_end(open: &str, incoming: &str) -> bool {
    (open == "li" && incoming == "li")
        || (matches!(open, "th" | "td")
            && matches!(incoming, "th" | "td" | "tr" | "thead" | "tbody" | "tfoot"))
        || (open == "tr" && matches!(incoming, "tr" | "thead" | "tbody" | "tfoot"))
        || (matches!(open, "thead" | "tbody") && matches!(incoming, "thead" | "tbody" | "tfoot"))
        || (open == "p"
            && matches!(
                incoming,
                "address"
                    | "article"
                    | "aside"
                    | "blockquote"
                    | "div"
                    | "dl"
                    | "fieldset"
                    | "footer"
                    | "form"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "header"
                    | "hr"
                    | "menu"
                    | "nav"
                    | "ol"
                    | "p"
                    | "pre"
                    | "section"
                    | "table"
                    | "ul"
            ))
}

fn is_void_html_tag(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn is_omitted_html_tag(name: &str) -> bool {
    matches!(
        name,
        "head"
            | "title"
            | "script"
            | "style"
            | "form"
            | "input"
            | "button"
            | "select"
            | "option"
            | "textarea"
            | "iframe"
            | "frame"
            | "frameset"
            | "object"
            | "embed"
            | "img"
            | "picture"
            | "audio"
            | "video"
            | "source"
            | "track"
            | "canvas"
            | "svg"
            | "math"
            | "link"
            | "meta"
            | "base"
    )
}

pub fn has_web_scheme(uri: &str) -> bool {
    uri.split_once(':').is_some_and(|(scheme, _)| {
        scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
    })
}

#[cfg(test)]
mod tests;
