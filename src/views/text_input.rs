use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(super) fn visible_input_window(
    input: &str,
    cursor: usize,
    available: usize,
) -> (String, usize) {
    let graphemes = input.graphemes(true).collect::<Vec<_>>();
    let cursor = cursor.min(graphemes.len());
    let mut start = cursor;
    let mut cursor_width: usize = 0;
    while start > 0 {
        let width = UnicodeWidthStr::width(display_grapheme(graphemes[start - 1]));
        if cursor_width.saturating_add(width) > available {
            break;
        }
        cursor_width = cursor_width.saturating_add(width);
        start -= 1;
    }

    let mut end = cursor;
    let mut total_width: usize = cursor_width;
    while end < graphemes.len() {
        let width = UnicodeWidthStr::width(display_grapheme(graphemes[end]));
        if total_width.saturating_add(width) > available {
            break;
        }
        total_width = total_width.saturating_add(width);
        end += 1;
    }

    (
        graphemes[start..end]
            .iter()
            .map(|grapheme| display_grapheme(grapheme))
            .collect(),
        cursor_width,
    )
}

pub(super) fn truncate_display_width(text: &str, max_width: usize) -> String {
    let mut width: usize = 0;
    text.graphemes(true)
        .map(display_grapheme)
        .take_while(|grapheme| {
            let grapheme_width = UnicodeWidthStr::width(*grapheme);
            if width.saturating_add(grapheme_width) > max_width {
                return false;
            }
            width = width.saturating_add(grapheme_width);
            true
        })
        .collect()
}

fn display_grapheme(grapheme: &str) -> &str {
    match grapheme {
        "\n" | "\r" => "↵",
        "\t" => "⇥",
        _ if grapheme.chars().any(char::is_control) => "�",
        _ => grapheme,
    }
}

#[cfg(test)]
mod tests {
    use super::{truncate_display_width, visible_input_window};

    #[test]
    fn input_windows_follow_the_cursor_without_splitting_unicode() {
        assert_eq!(
            visible_input_window("display-message", 15, 10),
            ("ay-message".to_owned(), 10)
        );
        assert_eq!(
            visible_input_window("e\u{301}界x", 1, 3),
            ("e\u{301}界".to_owned(), 1)
        );
    }

    #[test]
    fn displayed_text_replaces_controls_and_obeys_cell_width() {
        assert_eq!(truncate_display_width("a\tb\nc", 5), "a⇥b↵c");
        assert_eq!(truncate_display_width("a界b", 3), "a界");
    }
}
