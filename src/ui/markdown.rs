//! Markdown styling for the agent's prose in the transcript.
//!
//! Only the structure that matters at a glance is styled: headings, inline
//! code, fenced code blocks and bold. Everything else keeps the entry's own
//! color. The markdown markers stay in the text, styled like the content they
//! enclose, so the transcript still reads as markdown.

use crossterm::style::Color;

use crate::ui::tui::STATUS_COLOR;

/// Headings are light amber (the color inline code used to have), inline code
/// the status bar's amber (the same amber rush uses for the interactive
/// prompt), and fenced code blocks a light blue.
pub(super) const HEADING: Color = Color::AnsiValue(214);
pub(super) const INLINE_CODE: Color = STATUS_COLOR;
pub(super) const CODE_BLOCK: Color = Color::AnsiValue(110);

/// How a span is drawn: a color, and whether the terminal is asked for bold.
/// Bold is orthogonal to color, so `**text**` keeps the entry's own color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Style {
    pub color: Color,
    pub bold: bool,
}

impl Style {
    pub(super) const fn plain(color: Color) -> Self {
        Self { color, bold: false }
    }

    pub(super) const fn bold(color: Color) -> Self {
        Self { color, bold: true }
    }
}

impl From<Color> for Style {
    fn from(color: Color) -> Self {
        Self::plain(color)
    }
}

/// A logical line as styled spans, before wrapping.
pub(super) type Spans = Vec<(Style, String)>;

/// Style `text` line by line. Fenced code blocks keep their state across
/// lines, so the whole entry is styled at once.
pub(super) fn lines(text: &str, base: Color) -> Vec<Spans> {
    let base = Style::from(base);
    let mut result = Vec::new();
    let mut fenced = false;
    for line in text.split('\n') {
        let trimmed = line.trim_start();
        if fence(trimmed) {
            fenced = !fenced;
            result.push(vec![(Style::plain(CODE_BLOCK), line.to_string())]);
        } else if fenced {
            result.push(vec![(Style::plain(CODE_BLOCK), line.to_string())]);
        } else if heading(trimmed) {
            result.push(vec![(Style::plain(HEADING), line.to_string())]);
        } else {
            result.push(inline(line, base));
        }
    }
    result
}

/// Wrap styled spans to `width` columns, keeping each span's style. Rows break
/// at spaces so words stay whole, and a word longer than a row is still broken.
/// Tabs become spaces, as in the plain wrapper.
pub(super) fn wrap(spans: Spans, width: usize) -> Vec<Spans> {
    let width = width.max(1);
    let mut characters = Vec::new();
    for (style, text) in spans {
        for character in text.chars() {
            characters.push((style, if character == '\t' { ' ' } else { character }));
        }
    }
    let mut lines = Vec::new();
    let mut start = 0;
    loop {
        if characters.len() - start <= width {
            lines.push(row(&characters[start..]));
            break;
        }
        let window = start + width;
        let split = characters[start..window]
            .iter()
            .rposition(|(_, character)| *character == ' ')
            .filter(|index| *index > 0)
            .map_or(window, |index| start + index + 1);
        lines.push(row(&characters[start..split]));
        start = split;
    }
    lines
}

/// One wrapped row, with adjacent characters of the same style merged.
fn row(characters: &[(Style, char)]) -> Spans {
    let mut spans = Spans::new();
    for (style, character) in characters {
        push(&mut spans, *style, &character.to_string());
    }
    spans
}

/// An ATX heading: one to six `#` followed by a space or the end of the line.
fn heading(trimmed: &str) -> bool {
    let hashes = trimmed
        .chars()
        .take_while(|character| *character == '#')
        .count();
    (1..=6).contains(&hashes)
        && trimmed[hashes..]
            .chars()
            .next()
            .is_none_or(|character| character == ' ' || character == '\t')
}

fn fence(trimmed: &str) -> bool {
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

/// Split a line into plain, inline-code and bold spans. A backtick run opens
/// code that the next run of the same length closes, and `**` opens bold that
/// the next `**` closes; whichever marker comes first wins, so code inside a
/// bold run stays code. The markers stay in the text, styled like the content
/// they enclose, so the transcript still reads as markdown. An unclosed run
/// stays plain, markers and all.
fn inline(line: &str, base: Style) -> Spans {
    let mut spans = Spans::new();
    let mut rest = line;
    loop {
        let code = rest.find('`');
        let bold = rest.find("**");
        let Some(start) = code.into_iter().chain(bold).min() else {
            break;
        };
        if code == Some(start) {
            let ticks = rest[start..]
                .chars()
                .take_while(|character| *character == '`')
                .count();
            let marker = "`".repeat(ticks);
            let after = &rest[start + ticks..];
            let Some(end) = after.find(&marker) else {
                break;
            };
            push(&mut spans, base, &rest[..start]);
            // Code keeps the bold it was opened in, and takes its own color.
            let style = Style {
                color: INLINE_CODE,
                bold: base.bold,
            };
            push(&mut spans, style, &marker);
            push(&mut spans, style, &after[..end]);
            push(&mut spans, style, &marker);
            rest = &after[end + ticks..];
        } else {
            let after = &rest[start + 2..];
            let Some(end) = after.find("**") else {
                break;
            };
            push(&mut spans, base, &rest[..start]);
            let style = Style::bold(base.color);
            push(&mut spans, style, "**");
            // The run's own text may still hold code, so style it in turn.
            for (inner, text) in inline(&after[..end], style) {
                push(&mut spans, inner, &text);
            }
            push(&mut spans, style, "**");
            rest = &after[end + 2..];
        }
    }
    push(&mut spans, base, rest);
    spans
}

pub(super) fn push(spans: &mut Spans, style: impl Into<Style>, text: &str) {
    let style = style.into();
    if text.is_empty() {
        return;
    }
    match spans.last_mut() {
        Some((last, existing)) if *last == style => existing.push_str(text),
        _ => spans.push((style, text.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: Color = Color::White;

    fn plain(color: Color) -> Style {
        Style::plain(color)
    }

    fn bold(color: Color) -> Style {
        Style::bold(color)
    }

    fn text(spans: &Spans) -> String {
        spans.iter().map(|(_, text)| text.as_str()).collect()
    }

    #[test]
    fn headings_are_dark_amber() {
        let lines = lines("# Title\n## Section\nnot # a heading\n####### seven", BASE);
        assert_eq!(lines[0], [(plain(HEADING), "# Title".to_string())]);
        assert_eq!(lines[1], [(plain(HEADING), "## Section".to_string())]);
        assert_eq!(lines[2], [(plain(BASE), "not # a heading".to_string())]);
        assert_eq!(lines[3], [(plain(BASE), "####### seven".to_string())]);
    }

    #[test]
    fn inline_code_is_light_amber() {
        let lines = lines("run `lorry build` now", BASE);
        assert_eq!(
            lines[0],
            [
                (plain(BASE), "run ".to_string()),
                (plain(INLINE_CODE), "`lorry build`".to_string()),
                (plain(BASE), " now".to_string()),
            ]
        );
    }

    #[test]
    fn unclosed_backticks_stay_plain() {
        let lines = lines("a `b", BASE);
        assert_eq!(lines[0], [(plain(BASE), "a `b".to_string())]);
    }

    #[test]
    fn bold_is_bold_and_keeps_its_markers() {
        let lines = lines("a **b** c", BASE);
        assert_eq!(
            lines[0],
            [
                (plain(BASE), "a ".to_string()),
                (bold(BASE), "**b**".to_string()),
                (plain(BASE), " c".to_string()),
            ]
        );
    }

    #[test]
    fn unclosed_bold_stays_plain() {
        let lines = lines("a **b", BASE);
        assert_eq!(lines[0], [(plain(BASE), "a **b".to_string())]);
    }

    #[test]
    fn bold_inside_inline_code_is_code() {
        let lines = lines("`**x**`", BASE);
        assert_eq!(lines[0], [(plain(INLINE_CODE), "`**x**`".to_string())]);
    }

    #[test]
    fn code_inside_bold_keeps_the_bold() {
        let lines = lines("**a `b` c**", BASE);
        assert_eq!(
            lines[0],
            [
                (bold(BASE), "**a ".to_string()),
                (
                    Style {
                        color: INLINE_CODE,
                        bold: true,
                    },
                    "`b`".to_string(),
                ),
                (bold(BASE), " c**".to_string()),
            ]
        );
    }

    #[test]
    fn bold_inside_a_fence_is_plain_code() {
        let lines = lines("```\n**x**\n```", BASE);
        assert_eq!(lines[1], [(plain(CODE_BLOCK), "**x**".to_string())]);
    }

    #[test]
    fn fenced_blocks_are_blue_including_the_fences() {
        let lines = lines("before\n```rust\nlet x = 1;\n```\nafter", BASE);
        assert_eq!(lines[0], [(plain(BASE), "before".to_string())]);
        assert_eq!(lines[1], [(plain(CODE_BLOCK), "```rust".to_string())]);
        assert_eq!(lines[2], [(plain(CODE_BLOCK), "let x = 1;".to_string())]);
        assert_eq!(lines[3], [(plain(CODE_BLOCK), "```".to_string())]);
        assert_eq!(lines[4], [(plain(BASE), "after".to_string())]);
    }

    #[test]
    fn code_inside_a_fence_is_not_inline_code() {
        let lines = lines("```\n`x`\n```", BASE);
        assert_eq!(lines[1], [(plain(CODE_BLOCK), "`x`".to_string())]);
    }

    #[test]
    fn wrapping_keeps_each_span_style() {
        let wrapped = wrap(
            vec![
                (plain(BASE), "ab ".to_string()),
                (plain(INLINE_CODE), "cdef".to_string()),
            ],
            3,
        );
        assert_eq!(
            wrapped,
            vec![
                vec![(plain(BASE), "ab ".to_string())],
                vec![(plain(INLINE_CODE), "cde".to_string())],
                vec![(plain(INLINE_CODE), "f".to_string())],
            ]
        );
    }

    #[test]
    fn wrapping_keeps_bold_across_a_break() {
        let wrapped = wrap(vec![(bold(BASE), "hello world".to_string())], 8);
        assert_eq!(wrapped[0], [(bold(BASE), "hello ".to_string())]);
        assert_eq!(wrapped[1], [(bold(BASE), "world".to_string())]);
    }

    #[test]
    fn wrapping_breaks_at_spaces() {
        let wrapped = wrap(vec![(plain(BASE), "hello world".to_string())], 8);
        assert_eq!(text(&wrapped[0]), "hello ");
        assert_eq!(text(&wrapped[1]), "world");
    }

    #[test]
    fn wrapping_an_empty_line_yields_one_empty_line() {
        assert_eq!(wrap(Vec::new(), 10), [Vec::new()]);
    }

    #[test]
    fn wrapping_replaces_tabs_with_spaces() {
        let wrapped = wrap(vec![(plain(BASE), "a\tb".to_string())], 10);
        assert_eq!(text(&wrapped[0]), "a b");
    }
}
