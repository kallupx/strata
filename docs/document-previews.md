# Rendered document previews

Strata can render local Markdown and a deliberately small HTML subset with native GTK widgets. Each document preview has **Rendered** and **Source** views. **Render documents by default** under **Settings → General → Browsing** chooses the initial view for each newly opened document; switching the current preview does not change that preference.

Remote Markdown and HTML locations remain source-only. Rendered previews are selected from the local filename or content type:

- Markdown: `.md`, `.markdown`, `.mdown`, `.mkd`, `.mkdn`, `.mdwn`, `text/markdown`, and `text/x-markdown`;
- HTML: `.html`, `.htm`, `.xhtml`, `text/html`, and `application/xhtml+xml`.

## Supported content

Markdown uses `pulldown-cmark` with tables, strikethrough, and task lists enabled. Headings, paragraphs, emphasis, lists, block quotes, web links, fenced and inline code, rules, tables, and line breaks render natively. Raw HTML is displayed as inert text. Images become text placeholders, and their resource URLs are discarded.

HTML uses `html5gum` tokenization and supports semantic containers, headings, paragraphs, emphasis, lists, block quotes, `http` and `https` links, `pre`/`code`, rules, simple tables, and line breaks. Entities are decoded before their text is escaped for Pango.

CSS, classes, IDs, metadata, and other presentation attributes are ignored. Scripts, styles, forms, frames, objects, embedded content, images, media, event handlers, unsafe links, and resource-bearing tags or attributes are omitted. If a document still has useful safe content, Strata renders it with an omission warning. Link schemes are checked again when a link is activated before the existing external URI launcher is used.

## Limits and fallback

Rendered parsing has fixed limits:

- 1 MiB input;
- 20,000 parser events;
- nesting depth 32;
- 512 rendered widget units, with each table cell counting as one unit;
- 4 MiB of escaped Pango markup; and
- 500 ms parser time.

A truncated, malformed, contentless, timed-out, cancelled, or limit-exceeding document opens in **Source**. The **Rendered** control is disabled and the reason is shown above the source preview.

## Trust boundary

Document parsing runs off the GTK thread through `gio::spawn_blocking`. The parsers are in-process, pure Rust code that consumes only the already bounded source string and produces escaped Pango markup plus a small native document model. Parser cancellation and preview request identity are checked before results are published.

This path does not use WebKit, Chromium, JavaScript execution, CSS rendering, subresource loading, external converters, or parser-initiated filesystem or network access. Native GTK creates the final widgets; only a user-activated, revalidated `http` or `https` link can launch an external application.
