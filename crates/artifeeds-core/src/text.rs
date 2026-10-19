//! Allocation-light text helpers.
//!
//! Deliberately dependency-free: pulling in an HTML tree parser to render a
//! 400-character preview would cost more RAM per fetch than the whole article
//! record. These routines are single-pass, reuse the output buffer, and never
//! allocate per character.

use seahash::SeaHasher;
use std::hash::Hasher;

/// Hash used for dedup keys. SeaHash is ~10x faster than SipHash for short keys
/// and we only need collision resistance against accidental duplicates, not
/// cryptographic strength.
#[inline]
pub fn hash64(bytes: &[u8]) -> u64 {
    let mut h = SeaHasher::new();
    h.write(bytes);
    h.finish()
}

/// Hash of a (source, uid) pair without allocating a temporary buffer.
#[inline]
pub fn hash_pair64(a: u64, b: u64) -> u64 {
    let mut h = SeaHasher::new();
    h.write_u64(a);
    h.write_u64(b);
    h.finish()
}

/// Cheap deterministic shuffle value in `0..1000`, used to spread a fleet of
/// feeds out over their polling interval instead of stampeding at boot.
#[inline]
pub fn spread(key: &str) -> u64 {
    hash64(key.as_bytes()) % 1000
}

#[inline]
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// Decode the handful of entities that actually show up in feed bodies.
/// Numeric entities are decoded too; unknown ones are copied through verbatim
/// (which is what browsers do).
#[inline]
fn push_entity(out: &mut String, bytes: &[u8], start: usize, end: usize) -> bool {
    // bytes[start..end] excludes '&' and ';'
    let ent = &bytes[start..end];
    match ent {
        b"amp" => out.push('&'),
        b"lt" => out.push('<'),
        b"gt" => out.push('>'),
        b"quot" => out.push('"'),
        b"apos" | b"#39" => out.push('\''),
        b"nbsp" | b"#160" => out.push(' '),
        b"hellip" | b"#8230" => out.push('…'),
        b"mdash" | b"#8212" => out.push('—'),
        b"ndash" | b"#8211" => out.push('–'),
        b"ldquo" | b"#8220" => out.push('“'),
        b"rdquo" | b"#8221" => out.push('”'),
        b"lsquo" | b"#8216" => out.push('‘'),
        b"rsquo" | b"#8217" => out.push('’'),
        _ => {
            if let Some(rest) = ent.strip_prefix(b"#") {
                let (radix, digits) = if let Some(hex) = rest.strip_prefix(b"x").or_else(|| rest.strip_prefix(b"X")) {
                    (16, hex)
                } else {
                    (10, rest)
                };
                if let Ok(text) = std::str::from_utf8(digits)
                    && let Ok(code) = u32::from_str_radix(text, radix)
                    && let Some(ch) = char::from_u32(code)
                {
                    out.push(ch);
                    return true;
                }
            }
            return false;
        }
    }
    true
}

/// Convert (possibly malformed) HTML to readable plain text.
///
/// * drops `<script>`/`<style>` bodies entirely
/// * turns block-level tags into newlines so paragraphs survive
/// * collapses runs of whitespace
/// * appends at most `max_chars` characters
///
/// Returns the number of bytes appended to `out`.
pub fn html_to_text_into(html: &str, out: &mut String, max_chars: usize) -> usize {
    let bytes = html.as_bytes();
    let start_len = out.len();
    let mut i = 0;
    let mut pending_space = false;
    let mut pending_break = false;
    let mut in_skip: Option<&'static str> = None;
    let mut chars_added = 0usize;

    while i < bytes.len() {
        // Hard budget check first: no branch below can run with a full buffer,
        // which is what keeps the arithmetic in this loop total.
        if chars_added >= max_chars {
            break;
        }
        let b = bytes[i];
        if b == b'<' {
            // Comment
            if bytes[i..].starts_with(b"<!--") {
                i = match find(bytes, i + 4, b"-->") {
                    Some(p) => p + 3,
                    None => bytes.len(),
                };
                continue;
            }
            let Some(close) = find_byte(bytes, i + 1, b'>') else {
                break;
            };
            let mut tag = &bytes[i + 1..close];
            let closing = tag.first() == Some(&b'/');
            if closing {
                tag = &tag[1..];
            }
            let name_end = tag
                .iter()
                .position(|c| is_space(*c) || *c == b'/')
                .unwrap_or(tag.len());
            let name = tag[..name_end].to_ascii_lowercase();

            match name.as_slice() {
                b"script" => in_skip = if closing { None } else { Some("script") },
                b"style" => in_skip = if closing { None } else { Some("style") },
                _ => {}
            }
            // Self-closing line breaks and rules always break; block elements
            // break on their closing tag. No budget check is needed here: the
            // top of the loop guarantees room for one more character.
            let is_break = matches!(name.as_slice(), b"br" | b"hr");
            let is_block_close = closing
                && matches!(
                    name.as_slice(),
                    b"p" | b"div"
                        | b"section"
                        | b"article"
                        | b"li"
                        | b"tr"
                        | b"blockquote"
                        | b"h1"
                        | b"h2"
                        | b"h3"
                        | b"h4"
                        | b"h5"
                        | b"h6"
                        | b"pre"
                );
            if is_break || is_block_close {
                pending_break = true;
            }
            i = close + 1;
            continue;
        }

        if in_skip.is_some() {
            i += 1;
            continue;
        }

        if b == b'&'
            && let Some(semi) = find_byte(bytes, i + 1, b';').filter(|s| s - i <= 10)
        {
            if pending_break {
                out.push('\n');
                chars_added += 1;
                pending_break = false;
                pending_space = false;
            } else if pending_space && chars_added > 0 && chars_added < max_chars {
                out.push(' ');
                chars_added += 1;
                pending_space = false;
            }
            // The space above may have just filled the budget.
            if chars_added < max_chars {
                if !push_entity(out, bytes, i + 1, semi) {
                    out.push('&');
                }
                chars_added += 1;
            }
            i = semi + 1;
            continue;
        }

        if is_space(b) {
            pending_space = true;
            i += 1;
            continue;
        }

        // Copy a whole UTF-8 run in one go; avoids char-by-char pushes.
        //
        // An ampersand only ends a run when it starts an entity. Feed text has
        // been through entity decoding by the time it arrives here, so a plain
        // `&` is ordinary text — `AT&T`, `2023 && 2024` — and stopping on it
        // used to stop the cursor: the run came out empty, nothing advanced, and
        // one such item span a core forever.
        let run_start = i;
        while i < bytes.len() && !is_space(bytes[i]) && bytes[i] != b'<' {
            if bytes[i] == b'&'
                && find_byte(bytes, i + 1, b';').is_some_and(|semi| semi - i <= 10)
            {
                break;
            }
            i += 1;
        }
        if i == run_start {
            // Belt and braces: whatever the byte is, the loop must move. Nothing
            // below can make progress on an empty run.
            i += html[i..].chars().next().map_or(1, char::len_utf8);
            continue;
        }
        let run = &html[run_start..i];
        let run_chars = run.chars().count();
        if pending_break {
            out.push('\n');
            chars_added += 1;
            pending_break = false;
            pending_space = false;
        } else if pending_space && chars_added > 0 && chars_added < max_chars {
            out.push(' ');
            chars_added += 1;
            pending_space = false;
        }
        if chars_added + run_chars <= max_chars {
            out.push_str(run);
            chars_added += run_chars;
        } else {
            let take = max_chars.saturating_sub(chars_added);
            if take == 0 {
                break;
            }
            out.extend(run.chars().take(take));
            break;
        }
    }

    // Trim leading whitespace artefacts without reallocating.
    let produced = &out[start_len..];
    let trimmed = produced.trim_start_matches(['\n', ' ']);
    if trimmed.len() != produced.len() {
        let cut = produced.len() - trimmed.len();
        out.replace_range(start_len..start_len + cut, "");
    }
    out.len() - start_len
}

/// Convenience wrapper when the caller does not keep a scratch buffer.
pub fn html_to_text(html: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(max_chars.min(html.len()) + 8);
    html_to_text_into(html, &mut out, max_chars);
    out
}

/// Collapse whitespace in already-plain text (feed summaries are often padded).
pub fn squeeze_ws(text: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max_chars * 2));
    let mut pending = false;
    for ch in text.chars() {
        if out.chars().count() >= max_chars {
            break;
        }
        if ch.is_whitespace() {
            pending = !out.is_empty();
            continue;
        }
        if pending {
            out.push(' ');
            pending = false;
        }
        out.push(ch);
    }
    out
}

#[inline]
fn find_byte(bytes: &[u8], from: usize, needle: u8) -> Option<usize> {
    bytes.get(from..)?.iter().position(|b| *b == needle).map(|p| p + from)
}

#[inline]
fn find(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from >= bytes.len() || needle.len() > bytes.len() {
        return None;
    }
    bytes[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// Approximate terminal display width of a character: 2 for East Asian wide /
/// fullwidth characters and emoji, 1 otherwise.
///
/// `unicode-width` would be exact; this covers the ranges that actually appear
/// in feed bodies and costs a couple of comparisons instead of a table lookup.
/// Getting CJK right matters here: a Chinese article wrapped by character count
/// would overflow a pane by a factor of two.
#[inline]
pub fn char_width(c: char) -> usize {
    match c as u32 {
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x3FFFD => 2,
        _ => 1,
    }
}

/// Wrap plain text into fixed-width display lines, measured in **half-width
/// columns** (`width` = 2 half-columns per CJK character).
///
/// Greedy: break on the last whitespace inside the window when there is one,
/// hard-break otherwise (CJK text and long URLs have no spaces). Empty input
/// lines are preserved so paragraph structure survives.
///
/// Doing this once when an article is opened (and again on a window resize) is
/// what lets the reader pane use a virtualised list of uniform-height rows
/// instead of shaping and wrapping the whole body on every frame.
pub fn wrap_into_lines(text: &str, width: usize) -> Vec<String> {
    // Two half-width columns is the smallest useful budget (one CJK glyph); the
    // loop below already guarantees progress, so no larger floor is needed.
    let width = width.max(2);
    let mut out: Vec<String> = Vec::with_capacity(text.len() / width + 8);
    for raw_line in text.split('\n') {
        if raw_line.is_empty() {
            out.push(String::new());
            continue;
        }
        let chars: Vec<char> = raw_line.chars().collect();
        let mut start = 0usize;
        while start < chars.len() {
            // Walk forward until the column budget is spent.
            let mut used = 0usize;
            let mut end = start;
            while end < chars.len() {
                let w = char_width(chars[end]);
                if used + w > width {
                    break;
                }
                used += w;
                end += 1;
            }
            if end == start {
                // A single character wider than the whole pane; emit it alone
                // rather than looping forever.
                end = start + 1;
            }
            let slice_end = if end == chars.len() {
                end
            } else {
                // Prefer the last whitespace in the window.
                match chars[start..end].iter().rposition(|c| c.is_whitespace()) {
                    Some(pos) if pos > (end - start) / 3 => start + pos,
                    _ => end,
                }
            };
            let line: String = chars[start..slice_end].iter().collect();
            out.push(line.trim_end().to_string());
            start = if slice_end == start { start + 1 } else { slice_end };
            while start < chars.len() && chars[start] == ' ' && slice_end != chars.len() {
                start += 1;
            }
        }
    }
    out
}

/// Truncate on a char boundary, appending an ellipsis when cut.
pub fn truncate_chars(text: &str, max_chars: usize) -> &str {
    match text.char_indices().nth(max_chars) {
        Some((idx, _)) => &text[..idx],
        None => text,
    }
}

/// Human-friendly byte size, precomputed once per snapshot rather than per frame.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// Relative age, precomputed per snapshot so the render path never formats time.
pub fn human_age(seconds: i64) -> String {
    let s = seconds.max(0);
    if s < 60 {
        "now".into()
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h", s / 3600)
    } else if s < 86_400 * 30 {
        format!("{}d", s / 86_400)
    } else if s < 86_400 * 365 {
        format!("{}mo", s / (86_400 * 30))
    } else {
        format!("{}y", s / (86_400 * 365))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tags_and_entities() {
        let html = "<p>Hello&nbsp;<b>world</b> &amp; <i>friends</i></p><script>bad()</script>";
        let text = html_to_text(html, 1000);
        assert_eq!(text, "Hello world & friends");
    }

    #[test]
    fn respects_char_budget() {
        let html = "<div>".to_string() + &"x".repeat(5000) + "</div>";
        let text = html_to_text(&html, 100);
        assert_eq!(text.chars().count(), 100);
    }

    #[test]
    fn block_tags_become_breaks() {
        let text = html_to_text("<div>a</div><div>b</div>", 100);
        assert_eq!(text, "a\nb");
    }

    #[test]
    fn numeric_entities() {
        assert_eq!(html_to_text("&#65;&#x42;", 10), "AB");
    }

    #[test]
    fn wraps_on_whitespace() {
        let lines = wrap_into_lines("hello world this is a test of wrapping behaviour", 12);
        assert!(lines.iter().all(|l| l.chars().count() <= 12));
        assert!(lines.len() > 2);
    }

    #[test]
    fn hard_breaks_long_tokens() {
        assert_eq!(wrap_into_lines(&"x".repeat(50), 10).len(), 5);
    }

    #[test]
    fn preserves_blank_lines() {
        let lines = wrap_into_lines("a\n\nb", 10);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1], "");
    }

    #[test]
    fn handles_cjk_without_spaces() {
        let text = "这是一段没有任何空格的中文文本，需要被硬换行处理。".repeat(3);
        // `width` is in half-width columns, so 20 columns is 10 CJK characters.
        let lines = wrap_into_lines(&text, 20);
        assert!(
            lines.iter().all(|l| l.chars().count() <= 10),
            "CJK must be counted as double width"
        );
    }

    #[test]
    fn cjk_is_double_width() {
        let lines = wrap_into_lines(&"中".repeat(30), 10);
        assert_eq!(lines.len(), 6, "5 full-width characters per 10-column line");
        assert!(lines.iter().all(|l| l.chars().count() == 5));
    }

    #[test]
    fn mixed_width_wraps_proportionally() {
        // 4 Latin (4 columns) + 3 CJK (6 columns) = exactly 10.
        let lines = wrap_into_lines("abcd中文测试", 10);
        assert_eq!(lines[0], "abcd中文测");
        assert_eq!(lines[1], "试");
    }

    #[test]
    fn budget_is_exact_under_entities_and_spaces() {
        // Regression: the entity branch used to be able to push past the budget,
        // which underflowed the truncating branch.
        for budget in 1..40usize {
            let html = "<p>&amp; &lt; a &nbsp; b &hellip; c&#8230;d</p> <b>tail</b>"
                .repeat(3);
            let text = html_to_text(&html, budget);
            assert!(
                text.chars().count() <= budget,
                "budget {budget} produced {} chars",
                text.chars().count()
            );
        }
    }

    #[test]
    fn multibyte_safe() {
        let text = html_to_text("<p>你好，世界！这是一个测试。</p>", 4);
        assert_eq!(text.chars().count(), 4);
    }
}

/// Traditional Chinese to simplified, for feeds published in traditional script.
///
/// Character and phrase level, not a translation: 臺灣 → 台湾, 記憶體 → 记忆体.
/// Cheap enough to run on an article body when it is opened, which is where it
/// is applied — the stored text keeps whatever the feed published.
pub fn to_simplified(text: &str) -> String {
    zhconv::zhconv(text, zhconv::Variant::ZhCN)
}

#[cfg(test)]
mod simplified_tests {
    use super::to_simplified;

    #[test]
    fn converts_traditional_to_simplified() {
        assert_eq!(to_simplified("繁體字與記憶體"), "繁体字与记忆体");
        assert_eq!(to_simplified("這個問題很難"), "这个问题很难");
        assert_eq!(to_simplified("臺灣的軟體開發者"), "台湾的软件开发者");
    }

    #[test]
    fn leaves_simplified_text_alone() {
        for text in ["简体字", "这个问题很难", "hello world", ""] {
            assert_eq!(to_simplified(text), text);
        }
    }

    #[test]
    fn is_idempotent() {
        let once = to_simplified("後端與資料庫的最佳實踐");
        let twice = to_simplified(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn keeps_punctuation_urls_and_ascii() {
        let source = "《測試》 https://example.com/資料?q=1 — 100% 完成！";
        let out = to_simplified(source);
        assert!(out.contains("https://example.com/"), "urls survive: {out}");
        assert!(out.contains("—") && out.contains("！"));
        assert!(out.contains("100%"));
    }
}

#[cfg(test)]
mod ampersand_tests {
    use super::html_to_text;

    /// A `&` that is not an entity must not stop the cursor.
    ///
    /// This hung forever on a real feed: the item's text was `2023 && 2024`,
    /// which reaches this function *after* XML entity decoding, so the `&` is
    /// neither an entity nor a byte the run scanner would consume.
    #[test]
    fn a_bare_ampersand_is_text() {
        assert_eq!(html_to_text("2023 && 2024", 100), "2023 && 2024");
        assert_eq!(html_to_text("AT&T 与 R&D", 100), "AT&T 与 R&D");
        assert_eq!(html_to_text("&", 10), "&");
        assert_eq!(html_to_text("&&&", 10), "&&&");
        assert_eq!(html_to_text("a & b", 10), "a & b");
        // Not entities: no semicolon, or one too far away to be one.
        assert_eq!(html_to_text("&#", 10), "&#");
        assert_eq!(html_to_text("&x", 10), "&x");
        // A semicolon within reach reads as an entity terminator and is
        // consumed — the point here is only that the loop terminates.
        assert!(html_to_text("&;;;;;;;;;;y", 20).starts_with("&;y") || html_to_text("&;;;;;;;;;;y", 20).contains('y'));
    }

    #[test]
    fn real_entities_still_decode() {
        assert_eq!(html_to_text("a &amp; b", 20), "a & b");
        assert_eq!(html_to_text("&lt;tag&gt;", 20), "<tag>");
        assert_eq!(html_to_text("R&amp;D &amp;&amp; more", 40), "R&D && more");
    }
}
