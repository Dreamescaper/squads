//! Shared, UI-free HTML ⇄ text handling (milestone **M5**).
//!
//! Teams delivers message bodies as **RichText/Html**: paragraphs, line breaks,
//! inline emphasis, `<at>` mentions, `<pre>` code blocks and `<img>` media all
//! live in the raw HTML. Two operations recur everywhere in the core:
//!
//! * [`strip_html`] — collapse raw HTML to the plain text a timeline shows. It
//!   is **spacing-aware**: a block-level tag boundary (`</p><p>`, `<br>`) becomes
//!   a single space so words never glue together, while inline tags (`<b>`,
//!   `<at>`, `<code>`) vanish silently. Common named entities are decoded.
//! * [`html_escape`] — escape text for embedding in outgoing HTML.
//!
//! ## Why this module exists
//!
//! The M4 realtime decoder carried a *local* `strip_html` because the vendored
//! protocol helpers are private. M5 needs the same behaviour for history reads
//! and optimistic sends, so the one implementation now lives here and
//! [`crate::realtime::event`] reuses it. Its rules mirror the vendored
//! `ost::api::chat::strip_html` closely enough to be indistinguishable on the
//! wire shapes we handle.
//!
//! ## What we do *not* do here
//!
//! Outgoing HTML is built by the vendored, **fenced-code-aware**
//! [`ost::api::build_message_html`] — we never reimplement it (a second
//! markdown-ish dialect would only drift). This module only *inspects* HTML and
//! escapes plain text.
//!
//! ## Known limits (documented, not fixed here)
//!
//! * [`strip_html`] decodes a small set of named entities (`&amp;`, `&lt;`,
//!   `&gt;`, `&quot;`, `&#39;`, `&nbsp;`); numeric character references other
//!   than those, and entities inside attribute values, are not decoded.
//! * It does not insert a space *inside* a `<pre>` block: code preserves its own
//!   whitespace, which is what we want.

/// Block-level tags: a boundary here separates words, so it yields one space
/// (pending, emitted only *between* two non-space characters). Inline tags
/// (`b`, `i`, `at`, `code`, …) vanish silently so `a<b>x</b>b` stays glued.
///
/// The list matches the vendored protocol crate's `BLOCK_TAGS`.
const BLOCK_TAGS: &[&str] = &[
    "p",
    "div",
    "br",
    "section",
    "article",
    "header",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "ul",
    "ol",
    "li",
    "dl",
    "dt",
    "dd",
    "table",
    "tr",
    "td",
    "th",
    "blockquote",
    "pre",
    "hr",
];

/// Strip HTML tags from `html`, leaving readable plain text.
///
/// Block-level tag boundaries become a single space (so `</p><p>` never glues
/// words); inline tags disappear; a handful of common entities are decoded. No
/// leading/trailing trimming is performed — callers that want a one-liner call
/// `.trim()`. Never panics and never allocates more than a copy of the input.
pub fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut tag = String::new();
    let mut in_tag = false;
    let mut pending_space = false;
    for ch in html.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
                let name = tag
                    .trim_start_matches('/')
                    .split(|c: char| c.is_whitespace() || c == '/')
                    .next()
                    .unwrap_or("");
                if BLOCK_TAGS.iter().any(|t| t.eq_ignore_ascii_case(name)) {
                    pending_space = true;
                }
                tag.clear();
            } else {
                tag.push(ch);
            }
        } else if ch == '<' {
            in_tag = true;
        } else {
            if pending_space {
                pending_space = false;
                if !out.is_empty() && !out.ends_with(char::is_whitespace) && !ch.is_whitespace() {
                    out.push(' ');
                }
            }
            out.push(ch);
        }
    }
    decode_entities(&out)
}

/// Escape plain text for embedding in Teams RichText/Html.
///
/// `&` is escaped first so already-escaped text is not double-decoded. The set
/// matches the vendored `build_message_html` escaper.
pub fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Decode the small set of named/numeric entities Teams emits for text content.
fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        // `&amp;` last so `&amp;lt;` decodes to the literal text `&lt;`.
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_tags_vanish_and_keep_words_glued() {
        assert_eq!(strip_html("a<b>x</b>b"), "axb");
        assert_eq!(strip_html("<p>hello <b>world</b></p>"), "hello world");
    }

    #[test]
    fn block_boundaries_become_one_space() {
        assert_eq!(strip_html("<p>a</p><p>b</p>"), "a b");
        // Whitespace between boundaries is preserved, not doubled.
        assert_eq!(strip_html("<div>a</div>\n<div>b</div>"), "a\nb");
        assert_eq!(strip_html("a<br>b"), "a b");
        // No leading space is inserted for a leading block tag.
        assert_eq!(strip_html("<p>hi</p>"), "hi");
    }

    #[test]
    fn at_mentions_render_as_their_text() {
        let html = r#"<p>Hi <at id="0">Bob Smith</at> and <at id="1">Carol</at></p>"#;
        assert_eq!(strip_html(html), "Hi Bob Smith and Carol");
    }

    #[test]
    fn pre_blocks_preserve_whitespace() {
        assert_eq!(
            strip_html("<pre>fn main() {\n    run();\n}</pre>"),
            "fn main() {\n    run();\n}"
        );
        // A <pre> next to prose gets exactly one separating space.
        assert_eq!(strip_html("<p>see</p><pre>x</pre>"), "see x");
    }

    #[test]
    fn entities_are_decoded() {
        assert_eq!(
            strip_html("<p>a &amp; b &lt; c &gt; d &quot;e&quot; &#39;f&#39;</p>"),
            "a & b < c > d \"e\" 'f'"
        );
        assert_eq!(strip_html("a&nbsp;b"), "a b");
        // `&amp;lt;` decodes to the literal text `&lt;`, not `<`.
        assert_eq!(strip_html("&amp;lt;"), "&lt;");
    }

    #[test]
    fn unicode_survives_intact() {
        assert_eq!(
            strip_html("<p>café ☕ — 日本語 😀</p>"),
            "café ☕ — 日本語 😀"
        );
    }

    #[test]
    fn malformed_html_does_not_panic() {
        // An unterminated tag swallows the rest, exactly like the vendored parser.
        assert_eq!(strip_html("a < b"), "a ");
        assert_eq!(strip_html("<p>unclosed"), "unclosed");
        assert_eq!(strip_html("<"), "");
        assert_eq!(strip_html("a<b"), "a");
    }

    #[test]
    fn escape_handles_every_special_char_and_amp_first() {
        assert_eq!(html_escape("<b>&\"'"), "&lt;b&gt;&amp;&quot;&#39;");
        // Already-escaped input is not double decoded.
        assert_eq!(html_escape("&amp;"), "&amp;amp;");
    }
}
