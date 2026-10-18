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
//! ## Mention extraction (M8)
//!
//! [`mentions`] is the third operation here: it pulls the `@`-mentions out of a
//! raw body so the notification router (M8) can tell whether *we* were
//! mentioned. It recognises Teams' `<at …>Name</at>` tags (with or without an
//! MRI-carrying attribute) and plain-text `@name` tokens.
//!
//! ## Image extraction (M13)
//!
//! [`image_urls`] pulls the inline images out of a raw body — `<img src="…">`
//! tags (any quoting) plus obvious image links — so the conversation view can
//! fetch and render them via [`crate::MediaCache`]. It decodes HTML entities in
//! the `src` but leaves percent-encoding intact.
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
//! * [`mentions`] cannot recover the **display name** behind an index-only
//!   `<at id="0">` tag's MRI — that mapping lives in the message's `mentions`
//!   property array, which we do not parse here — and a plain `@name` token has
//!   no MRI at all. It also does not attempt names containing spaces (Teams
//!   encodes those as `<at>` tags).

use serde::{Deserialize, Serialize};

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

/// One `@`-mention found in a raw message body (milestone **M8**).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mention {
    /// The mentioned user's MRI (`8:orgid:<guid>`) when the body named one.
    ///
    /// Teams emits the MRI in a `<at mri="…">`/`<at id="8:orgid:…">` tag; an
    /// index-only `<at id="0">` tag carries **no** MRI here (the index maps into
    /// the message's `mentions` property, which we do not parse).
    pub mri: Option<String>,
    /// The mention's display text (the `<at>` inner text, or the `@name` token).
    pub name: String,
}

/// Extract `@`-mentions from raw Teams message HTML (milestone **M8**).
///
/// Two shapes are recognised, in document order:
///
/// * Teams **mention tags** — `<at id="0">Bob Smith</at>` (an index id that
///   yields no MRI) and `<at id="8:orgid:…">Bob</at>` / `<at mri="…">Bob</at>`
///   (which carry the MRI). The tag's inner text, HTML-stripped, is the name.
/// * plain-text **`@name`** tokens — a `@` at a word boundary followed by
///   letters/digits/`._-` (unicode-aware). These carry no MRI.
///
/// Pure and total: malformed or unterminated tags are skipped, an email-like
/// `a@b.c` is **not** a mention (the `@` is not at a word boundary), and the
/// function never panics.
pub fn mentions(raw: &str) -> Vec<Mention> {
    let mut out = Vec::new();
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            if let Some((mention, next)) = parse_at_tag(raw, i) {
                if !mention.name.is_empty() || mention.mri.is_some() {
                    out.push(mention);
                }
                i = next;
                continue;
            }
            // Skip a tag we do not model so an `@` inside an attribute value is
            // never mistaken for a mention. A `<` that does not open a tag is
            // advanced a single char so plain text after it still scans.
            if opens_tag_name(bytes, i) {
                match raw[i..].find('>') {
                    Some(gt) => {
                        i += gt + 1;
                        continue;
                    }
                    None => break,
                }
            }
            i += 1;
            continue;
        }
        let ch = raw[i..].chars().next().unwrap_or('\0');
        if ch == '@' && at_word_boundary(raw, i) {
            let (name, next) = scan_at_name(raw, i + 1);
            if !name.is_empty() {
                out.push(Mention { mri: None, name });
                i = next;
                continue;
            }
        }
        i += ch.len_utf8();
    }
    out
}

/// Parse a `<at …>Name</at>` tag starting at byte `start` (which must be `<`).
///
/// Returns the mention and the byte index just past the closing `</at>`, or
/// `None` when this is not a well-formed mention tag.
fn parse_at_tag(raw: &str, start: usize) -> Option<(Mention, usize)> {
    let rest = raw.get(start..)?;
    let mut chars = rest.chars();
    if chars.next()? != '<' {
        return None;
    }
    if !chars.next()?.eq_ignore_ascii_case(&'a') || !chars.next()?.eq_ignore_ascii_case(&'t') {
        return None;
    }
    // The tag name must be delimited (`<at>`, `<at …>`, `<at/>`).
    match chars.next() {
        Some(c) if c == '>' || c == '/' || c.is_whitespace() => {}
        _ => return None,
    }
    let open_end = start + rest.find('>')? + 1;
    // Attributes live between "<at" and the closing '>'.
    let attrs = raw.get(start + 3..open_end - 1)?;
    let close = open_end + find_ci(raw.get(open_end..)?, "</at>")?;
    let inner = raw.get(open_end..close)?;
    let name = strip_html(inner).trim().to_string();
    let mri = mention_mri(&parse_attrs(attrs));
    Some((Mention { mri, name }, close + "</at>".len()))
}

/// Whether `bytes[i] == b'<'` opens a tag name (letter, `/` or `!`).
fn opens_tag_name(bytes: &[u8], i: usize) -> bool {
    matches!(bytes.get(i + 1), Some(b) if b.is_ascii_alphanumeric() || *b == b'/' || *b == b'!')
}

/// Whether the `@` at byte `i` starts a word (start of text, or preceded by a
/// non-word character).
fn at_word_boundary(raw: &str, i: usize) -> bool {
    match raw.get(..i).and_then(|s| s.chars().next_back()) {
        None => true,
        Some(c) => !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '@')),
    }
}

/// Consume a `@name` run starting at byte `start` (just past the `@`).
///
/// Returns the trimmed name and the byte index past the consumed run.
fn scan_at_name(raw: &str, start: usize) -> (String, usize) {
    let mut name = String::new();
    let mut end = start;
    for (off, ch) in raw.get(start..).unwrap_or("").char_indices() {
        if ch.is_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            name.push(ch);
            end = start + off + ch.len_utf8();
        } else {
            break;
        }
    }
    let trimmed = name.trim_end_matches(['.', '_', '-']).to_string();
    (trimmed, end)
}

/// The `(key, value)` attribute pairs of a tag's attribute string.
fn parse_attrs(attrs: &str) -> Vec<(String, String)> {
    let bytes = attrs.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let name_start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' {
            i += 1;
        }
        if i == name_start {
            i += 1;
            continue;
        }
        let name = attrs[name_start..i].trim().to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let quote = bytes[i];
                i += 1;
                let v_start = i;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                value = attrs[v_start..i].to_string();
                if i < bytes.len() {
                    i += 1;
                }
            } else {
                let v_start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                value = attrs[v_start..i].to_string();
            }
        }
        if !name.is_empty() {
            out.push((name, value));
        }
    }
    out
}

/// The MRI an `<at>` tag carries, if any.
///
/// Prefers an explicit `mri`/`userid`/`user` attribute; falls back to the `id`
/// attribute **only when it is not a bare index** (a purely numeric id is the
/// position in the message's `mentions` array, not an MRI).
fn mention_mri(attrs: &[(String, String)]) -> Option<String> {
    let value_of = |key: &str| {
        attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.trim())
            .filter(|v| !v.is_empty())
    };
    for key in ["mri", "userid", "user"] {
        if let Some(v) = value_of(key) {
            return Some(v.to_string());
        }
    }
    if let Some(v) = value_of("id")
        && !v.chars().all(|c| c.is_ascii_digit())
    {
        return Some(v.to_string());
    }
    None
}

/// Case-insensitive substring search (ASCII folding preserves byte offsets).
fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    haystack.to_ascii_lowercase().find(needle)
}

// ----- image URL extraction (M13) -----------------------------------------

/// Extract image URLs from raw Teams message HTML (milestone **M13**).
///
/// Two shapes are recognised, in document order:
///
/// * `<img src="…">` tags — any attribute order, single/double/unquoted values,
///   case-insensitive tag/attribute names. The `src` is HTML-entity-decoded
///   (`&amp;` → `&`) but left percent-encoded (a URL must keep `%3A`). A
///   Microsoft object-store `src` (`…/views/imgo`, no file extension) is
///   surfaced here, which is why `<img>` is handled separately from links.
/// * plain `http(s)://…` links whose path ends in a known image extension
///   (`png`/`jpg`/`jpeg`/`gif`/`webp`/`bmp`/`svg`/`avif`/`ico`/`tif`), e.g. an
///   `<a href>` or a bare link.
///
/// Pure and total: malformed/unterminated tags are skipped, a non-image link is
/// not returned, results are de-duplicated preserving first-seen order, and the
/// function never panics.
pub fn image_urls(raw: &str) -> Vec<String> {
    let mut out = Vec::new();

    // 1. `<img src="…">` tags (case-insensitive tag name, any quoting).
    let lower = raw.to_ascii_lowercase();
    let mut i = 0;
    while let Some(pos) = lower[i..].find("<img") {
        let start = i + pos;
        let after = raw.get(start + 4..).unwrap_or("");
        let boundary = after
            .chars()
            .next()
            .is_some_and(|c| c.is_whitespace() || c == '>' || c == '/');
        if !boundary {
            i = start + 4;
            continue;
        }
        let Some(gt) = raw[start..].find('>') else {
            break;
        };
        let attrs = raw.get(start + 4..start + gt).unwrap_or("");
        if let Some(src) = attr_value(attrs, "src") {
            let decoded = decode_entities(src.trim());
            if !decoded.is_empty() {
                push_unique(&mut out, decoded);
            }
        }
        i = start + gt + 1;
    }

    // 2. Obvious image links (a `http(s)://…` ending in an image extension).
    for url in find_urls(raw) {
        if has_image_extension(&url) {
            push_unique(&mut out, url);
        }
    }

    out
}

/// The value of one attribute in a tag's attribute string (case-insensitive
/// name), or `None`.
fn attr_value(attrs: &str, key: &str) -> Option<String> {
    parse_attrs(attrs)
        .into_iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

/// Every `http(s)://…` token in `raw`, trimmed of trailing punctuation.
fn find_urls(raw: &str) -> Vec<String> {
    let lower = raw.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(pos) = lower[i..].find("http") {
        let start = i + pos;
        let rest = &lower[start..];
        if !rest.starts_with("https://") && !rest.starts_with("http://") {
            i = start + 4;
            continue;
        }
        let end = raw[start..]
            .find(|c: char| {
                c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ')' | ']' | '}' | ',')
            })
            .map(|n| start + n)
            .unwrap_or(raw.len());
        let url = trim_url(&decode_entities(&raw[start..end]));
        if !url.is_empty() {
            out.push(url);
        }
        i = end.max(start + 4);
    }
    out
}

/// Strip trailing sentence punctuation from a scanned URL.
fn trim_url(url: &str) -> String {
    url.trim_end_matches(['.', ',', ';', ':', '!', '?'])
        .to_string()
}

/// Whether a URL's path ends in a known image extension.
fn has_image_extension(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "avif" | "ico" | "tif" | "tiff"
    )
}

/// Push `url` unless an identical value is already present (first-seen order).
fn push_unique(out: &mut Vec<String>, url: String) {
    if !out.iter().any(|existing| existing == &url) {
        out.push(url);
    }
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

    // ----- M8: mention extraction -------------------------------------------

    fn named(name: &str, mri: Option<&str>) -> Mention {
        Mention {
            mri: mri.map(str::to_string),
            name: name.to_string(),
        }
    }

    #[test]
    fn at_tag_without_mri_yields_name_only() {
        let raw = r#"<p>Hi <at id="0">Bob Smith</at> and <at id="1">Carol</at></p>"#;
        assert_eq!(
            mentions(raw),
            vec![named("Bob Smith", None), named("Carol", None)]
        );
    }

    #[test]
    fn at_tag_carrying_an_mri_or_userid_keeps_it() {
        // The vendored parser's own shape puts the MRI in `id`.
        assert_eq!(
            mentions(r#"<at id="8:orgid:aaaa">Bob</at>"#),
            vec![named("Bob", Some("8:orgid:aaaa"))]
        );
        // `mri` and `userId` attributes are also honoured.
        assert_eq!(
            mentions(r#"<at mri="8:orgid:bbbb">Carol Jones</at>"#),
            vec![named("Carol Jones", Some("8:orgid:bbbb"))]
        );
        assert_eq!(
            mentions(r#"<at id="0" userId="8:orgid:cccc">Dan</at>"#),
            vec![named("Dan", Some("8:orgid:cccc"))]
        );
    }

    #[test]
    fn at_tag_decodes_entities_in_the_name() {
        assert_eq!(
            mentions(r#"<at id="0">A &amp; B</at>"#),
            vec![named("A & B", None)]
        );
    }

    #[test]
    fn plain_at_name_tokens_are_found_at_word_boundaries() {
        assert_eq!(
            mentions("<p>hey @Bob and @Carol_Jones, plus @a-b.c!</p>"),
            vec![
                named("Bob", None),
                named("Carol_Jones", None),
                named("a-b.c", None),
            ]
        );
        // A leading `@` at the very start of the body counts.
        assert_eq!(mentions("@Bob hello"), vec![named("Bob", None)]);
        // Trailing punctuation is not part of the name.
        assert_eq!(mentions("hi @Bob."), vec![named("Bob", None)]);
    }

    #[test]
    fn email_like_at_is_not_a_mention() {
        assert!(mentions("mail me at bob@contoso.com").is_empty());
        assert!(mentions("a@b").is_empty());
    }

    #[test]
    fn mention_tags_and_at_names_are_ordered_and_not_double_counted() {
        let raw = r#"<p><at id="0">Bob</at> sees @Carol and <at mri="8:x">Eve</at></p>"#;
        assert_eq!(
            mentions(raw),
            vec![
                named("Bob", None),
                named("Carol", None),
                named("Eve", Some("8:x")),
            ]
        );
    }

    #[test]
    fn mention_detection_never_panics_on_odd_input() {
        for raw in [
            "<at id=\"0\">Unclosed",
            "<at",
            "<<at id='0'>x</at>",
            "<attachment id=\"x\"></attachment>",
            "plain text with no mentions",
            "@",
            "@@",
            "<at mri=\"8:x\"></at>",
        ] {
            let _ = mentions(raw);
        }
        // `<attachment …>` is not a mention tag.
        assert!(mentions(r#"<attachment id="a"></attachment>"#).is_empty());
        // A tag with an MRI but no inner text still surfaces (the MRI is useful).
        assert_eq!(
            mentions(r#"<at mri="8:x"></at>"#),
            vec![named("", Some("8:x"))]
        );
    }

    // ----- M13: image URL extraction ----------------------------------------

    #[test]
    fn img_tags_are_extracted_with_every_quoting_style() {
        let html = r#"<p>look <img src="https://a.example/one.png"> and
            <img src='https://b.example/two.gif' alt="x"> plus
            <img src=https://c.example/three.jpg></p>"#;
        assert_eq!(
            image_urls(html),
            vec![
                "https://a.example/one.png",
                "https://b.example/two.gif",
                "https://c.example/three.jpg",
            ]
        );
    }

    #[test]
    fn multiple_images_keep_document_order_and_dedupe() {
        let html =
            r#"<img src="https://a/1.png"><img src="https://a/2.png"><img src="https://a/1.png">"#;
        assert_eq!(image_urls(html), vec!["https://a/1.png", "https://a/2.png"]);
    }

    #[test]
    fn object_store_srcs_without_an_extension_are_kept() {
        // AMS/object-store URLs carry no file extension; the <img> tag is the
        // only place they appear, so they must still be surfaced.
        let html = r#"<img src="https://us-api.asm.skype.com/v1/objects/0-abc/views/imgo">"#;
        assert_eq!(
            image_urls(html),
            vec!["https://us-api.asm.skype.com/v1/objects/0-abc/views/imgo"]
        );
    }

    #[test]
    fn obvious_image_links_are_found_but_other_links_are_not() {
        let html = r#"see <a href="https://giphy.com/cat.gif">a cat</a>
            and <a href="https://example.com/doc.pdf">a doc</a>
            or https://example.com/photo.webp here"#;
        assert_eq!(
            image_urls(html),
            vec![
                "https://giphy.com/cat.gif",
                "https://example.com/photo.webp"
            ]
        );
        // A bare non-image link is excluded.
        assert!(image_urls("https://example.com/page.html").is_empty());
    }

    #[test]
    fn entities_are_decoded_and_percent_encoding_is_preserved() {
        let html = r#"<img src="https://h.example/a%20b.png?x=1&amp;y=2">"#;
        assert_eq!(
            image_urls(html),
            vec!["https://h.example/a%20b.png?x=1&y=2"]
        );
    }

    #[test]
    fn image_extraction_never_panics_on_odd_input() {
        for raw in [
            "<img",
            "<img src=",
            "<img src=\"unclosed",
            "<IMG SRC=\"HTTPS://A/B.PNG\">",
            "<image src=\"x\">",
            "no images at all",
            "<img src=\"\">",
            "https://",
            "http://",
        ] {
            let _ = image_urls(raw);
        }
        // Tag names are case-insensitive.
        assert_eq!(
            image_urls("<IMG SRC=\"https://a/b.PNG\">"),
            vec!["https://a/b.PNG"]
        );
        // An empty src is dropped.
        assert!(image_urls("<img src=\"\">").is_empty());
        // `<image>` is not an `<img>` tag; the URL inside it is only surfaced
        // by the obvious-image-link scan (which is fine — it is an image).
        assert_eq!(
            image_urls("<image src=\"https://a/b.png\">"),
            vec!["https://a/b.png"]
        );
        // A relative `<img src>` is kept (it is a real `<img>`), but a relative
        // non-image link is not.
        assert_eq!(image_urls("<img src=\"/x/y.png\">"), vec!["/x/y.png"]);
    }
}
