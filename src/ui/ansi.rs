//! Turning recipe output into styled segments.
//!
//! Recipes run without a TTY and with `NO_COLOR=1`, but several write escape codes
//! unconditionally — `just features` prints its ✓ and ✗ with a hardcoded `\033[32m` —
//! so the log view would otherwise show raw `[32m` noise. This handles the small
//! subset that actually appears in the justfile: colour, bold, dim, and reset.

/// A run of text sharing one style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    pub style: Style,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub color: Option<Color>,
    pub bold: bool,
    pub dim: bool,
}

impl Style {
    /// The GtkTextTag name for this style, or `None` when it needs no tag.
    pub fn tag_name(&self) -> Option<String> {
        let mut name = String::new();
        if let Some(color) = self.color {
            name.push_str(color.tag());
        }
        if self.bold {
            name.push_str("-bold");
        }
        if self.dim {
            name.push_str("-dim");
        }
        (!name.is_empty()).then_some(name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Grey,
}

impl Color {
    fn from_sgr(code: u8) -> Option<Color> {
        Some(match code {
            31 | 91 => Color::Red,
            32 | 92 => Color::Green,
            33 | 93 => Color::Yellow,
            34 | 94 => Color::Blue,
            35 | 95 => Color::Magenta,
            36 | 96 => Color::Cyan,
            30 | 90 | 37 | 97 => Color::Grey,
            _ => return None,
        })
    }

    fn tag(self) -> &'static str {
        match self {
            Color::Red => "red",
            Color::Green => "green",
            Color::Yellow => "yellow",
            Color::Blue => "blue",
            Color::Magenta => "magenta",
            Color::Cyan => "cyan",
            Color::Grey => "grey",
        }
    }

    /// Resolved against the theme's own palette rather than a fixed hex value, so the
    /// log stays readable in both light and dark.
    pub fn css(self) -> &'static str {
        match self {
            Color::Red => "#e01b24",
            Color::Green => "#2ec27e",
            Color::Yellow => "#e5a50a",
            Color::Blue => "#3584e4",
            Color::Magenta => "#c74ded",
            Color::Cyan => "#00b3c8",
            Color::Grey => "#9a9996",
        }
    }

    pub const ALL: [Color; 7] = [
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::Grey,
    ];
}

/// Split one line into styled segments, dropping every escape sequence.
pub fn parse(line: &str) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut style = Style::default();
    let mut text = String::new();
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '\x1b' {
            text.push(c);
            continue;
        }
        // Only CSI sequences appear in practice; anything else is dropped along with
        // the escape that introduced it.
        if chars.peek() != Some(&'[') {
            continue;
        }
        chars.next();

        let mut params = String::new();
        let mut final_byte = None;
        for c in chars.by_ref() {
            if c.is_ascii_digit() || c == ';' {
                params.push(c);
            } else {
                final_byte = Some(c);
                break;
            }
        }

        // Styling changes at a boundary, so flush what came before it.
        if final_byte == Some('m') {
            if !text.is_empty() {
                segments.push(Segment {
                    text: std::mem::take(&mut text),
                    style,
                });
            }
            style = apply(style, &params);
        }
    }

    if !text.is_empty() {
        segments.push(Segment { text, style });
    }
    segments
}

fn apply(mut style: Style, params: &str) -> Style {
    // A bare `ESC[m` means reset, same as `ESC[0m`.
    if params.is_empty() {
        return Style::default();
    }
    for param in params.split(';') {
        let Ok(code) = param.parse::<u8>() else {
            continue;
        };
        match code {
            0 => style = Style::default(),
            1 => style.bold = true,
            2 => style.dim = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            39 => style.color = None,
            other => {
                if let Some(color) = Color::from_sgr(other) {
                    style.color = Some(color);
                }
            }
        }
    }
    style
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The readable text, with every escape sequence dropped.
    fn strip(line: &str) -> String {
        parse(line).into_iter().map(|s| s.text).collect()
    }

    #[test]
    fn plain_text_is_one_unstyled_segment() {
        let segments = parse("rebuilding…");
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "rebuilding…");
        assert_eq!(segments[0].style, Style::default());
    }

    #[test]
    fn parses_the_feature_list_the_justfile_actually_prints() {
        // From `just features`: printf "    \033[32m✓\033[0m %s\n"
        let segments = parse("    \x1b[32m✓\x1b[0m gaming");
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0].text, "    ");
        assert_eq!(segments[1].text, "✓");
        assert_eq!(segments[1].style.color, Some(Color::Green));
        assert_eq!(segments[2].text, " gaming");
        assert_eq!(segments[2].style, Style::default());
    }

    #[test]
    fn handles_the_dim_variant() {
        let segments = parse("\x1b[90m✗\x1b[0m print3d");
        assert_eq!(segments[0].style.color, Some(Color::Grey));
    }

    #[test]
    fn combines_bold_with_colour() {
        let segments = parse("\x1b[1;31mfailed\x1b[0m");
        assert_eq!(segments[0].style.color, Some(Color::Red));
        assert!(segments[0].style.bold);
        assert_eq!(segments[0].style.tag_name().as_deref(), Some("red-bold"));
    }

    #[test]
    fn unknown_sequences_leave_no_debris() {
        // Cursor movement and erase-line, which progress output likes to emit.
        assert_eq!(strip("\x1b[2K\x1b[1Gbuilding"), "building");
        assert_eq!(strip("plain"), "plain");
    }

    #[test]
    fn a_bare_reset_clears_the_style() {
        let segments = parse("\x1b[32mgreen\x1b[mplain");
        assert_eq!(segments[1].style, Style::default());
    }

    #[test]
    fn stripping_gives_back_the_readable_text() {
        assert_eq!(strip("    \x1b[32m✓\x1b[0m gaming"), "    ✓ gaming");
    }
}
