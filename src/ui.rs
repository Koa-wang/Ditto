//! Ratatui 渲染层。
//!
//! 截断与行布局都抽成纯函数，这样中文宽度、换行折叠这类容易出错的地方可以被
//! 单测覆盖，而不是靠肉眼看终端。

use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthChar;

use crate::app::{App, Mode};
use crate::history;

/// 单条预览的最大显示宽度（设计文档：超长截断）。
pub const MAX_PREVIEW_WIDTH: usize = 40;

/// 选中项前缀。它的宽度必须从可用行宽里扣掉，否则右侧数字会错位。
const HIGHLIGHT_SYMBOL: &str = "▸ ";

/// 预览与右侧字数之间保留的最小空隙。
const MIN_GAP: usize = 2;

pub fn draw(frame: &mut Frame, app: &App) {
    let [title_area, body_area, hint_area, status_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    // ---- 标题栏 ----
    let title = Line::from(vec![
        Span::styled(
            " 📋 Ditto ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" 剪贴板历史  {}/{}", app.history.len(), history::CAP)),
    ]);
    frame.render_widget(Paragraph::new(title), title_area);

    // ---- 列表 ----
    let block = Block::bordered().border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(body_area);
    frame.render_widget(block, body_area);

    if app.history.is_empty() {
        frame.render_widget(
            Paragraph::new("剪贴板历史为空 —— 去别处复制点文本，500ms 内会出现在这里")
                .style(Style::default().fg(Color::DarkGray)),
            inner,
        );
    } else {
        // 高亮符号会占用行首宽度，这里先扣掉再算每条记录的可用宽度。
        let row_width = (inner.width as usize).saturating_sub(display_width(HIGHLIGHT_SYMBOL));

        let items: Vec<ListItem> = app
            .history
            .items()
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let (prefix, preview, pad, suffix) = row_parts(index, &entry.text, row_width);
                ListItem::new(Line::from(vec![
                    Span::styled(prefix, Style::default().fg(Color::DarkGray)),
                    Span::raw(preview),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(suffix, Style::default().fg(Color::DarkGray)),
                ]))
            })
            .collect();

        let mut state = ListState::default();
        state.select(app.selected_index());

        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .bg(Color::Cyan)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(HIGHLIGHT_SYMBOL);
        frame.render_stateful_widget(list, inner, &mut state);
    }

    // ---- 快捷键提示 ----
    frame.render_widget(Paragraph::new(hint_line(app)), hint_area);

    // ---- 状态行 ----
    frame.render_widget(Paragraph::new(status_line(app)), status_area);
}

fn hint_line(app: &App) -> Line<'static> {
    if app.mode == Mode::ConfirmClear {
        return Line::from(vec![
            Span::styled(
                " 确认清空全部历史？",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" 按 "),
            Span::styled("y", Style::default().fg(Color::Yellow)),
            Span::raw(" 确认 · 其他任意键取消"),
        ]);
    }

    let keys = [
        ("j/k ↑/↓", "选择"),
        ("Enter", "写回剪贴板"),
        ("d", "删除"),
        ("D", "清空"),
        ("q/Esc", "退出"),
    ];
    let mut spans = vec![Span::raw(" ")];
    for (position, (key, description)) in keys.iter().enumerate() {
        if position > 0 {
            spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
        }
        spans.push(Span::styled(*key, Style::default().fg(Color::Cyan)));
        spans.push(Span::raw(format!(" {description}")));
    }
    Line::from(spans)
}

fn status_line(app: &App) -> Line<'static> {
    let mut spans = Vec::new();

    if let Some(message) = app.status_line() {
        let color = if message.starts_with('✓') {
            Color::Green
        } else if message.starts_with('⚠') || message.starts_with('✗') {
            Color::Yellow
        } else {
            Color::Gray
        };
        spans.push(Span::styled(format!(" {message}"), Style::default().fg(color)));
    }

    if let Some(alert) = &app.alert {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            format!("⚠ {alert}"),
            Style::default().fg(Color::Red),
        ));
    }

    Line::from(spans)
}

/// 字符串在终端里占用的列数（CJK 算 2 列，控制字符算 0 列）。
pub fn display_width(text: &str) -> usize {
    text.chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

/// 把换行/制表符折叠成单行可见字符，避免多行文本破坏列表布局。
pub fn fold_whitespace(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            '\n' => Some('⏎'),
            '\t' => Some(' '),
            '\r' => None,
            other => Some(other),
        })
        .collect()
}

/// 按**显示宽度**截断，超出时补 `...`。恰好等宽不截断、不补省略号。
pub fn truncate_preview(text: &str, max_width: usize) -> String {
    let folded = fold_whitespace(text);
    if display_width(&folded) <= max_width {
        return folded;
    }
    if max_width <= 3 {
        return ".".repeat(max_width);
    }

    let budget = max_width - 3;
    let mut kept = String::new();
    let mut used = 0;
    for c in folded.chars() {
        let width = UnicodeWidthChar::width(c).unwrap_or(0);
        // 用 > 而不是 >=：保证不会把一个宽字符切成半截
        if used + width > budget {
            break;
        }
        used += width;
        kept.push(c);
    }
    kept.push_str("...");
    kept
}

/// 拼出一行的片段：`(序号前缀, 预览, 填充空格数, 右侧字数)`。
///
/// 抽出来是为了能断言「整行宽度刚好等于可用宽度」——右侧字数靠对齐全靠这里的
/// 空格数算得准，靠肉眼在终端里很难发现差了一两列。
pub fn row_parts(index: usize, text: &str, row_width: usize) -> (String, String, usize, String) {
    let prefix = format!("{:>3} ", index + 1);
    let suffix = format!("({} 字)", text.chars().count());

    let reserved = display_width(&prefix) + display_width(&suffix) + MIN_GAP;
    let budget = row_width.saturating_sub(reserved).min(MAX_PREVIEW_WIDTH);
    let preview = truncate_preview(text, budget);

    let used = display_width(&prefix) + display_width(&preview) + display_width(&suffix);
    let pad = row_width.saturating_sub(used);

    (prefix, preview, pad, suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_width_counts_cjk_as_two_columns() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("中文"), 4);
        assert_eq!(display_width(""), 0);
    }

    #[test]
    fn short_text_is_left_untouched() {
        assert_eq!(truncate_preview("hello", 40), "hello");
        assert_eq!(truncate_preview("", 40), "");
    }

    #[test]
    fn exact_fit_is_not_treated_as_overflow() {
        let text = "a".repeat(40);
        let result = truncate_preview(&text, 40);
        assert_eq!(result, text);
        assert!(!result.ends_with("..."), "恰好等宽不应补省略号");
    }

    #[test]
    fn long_ascii_text_is_truncated_with_ellipsis() {
        let result = truncate_preview(&"x".repeat(100), 10);
        assert_eq!(result, "xxxxxxx...");
        assert_eq!(display_width(&result), 10);
    }

    #[test]
    fn long_cjk_text_is_truncated_on_character_boundary() {
        let text = "中文".repeat(30);
        let result = truncate_preview(&text, 10);

        assert!(result.ends_with("..."));
        assert!(display_width(&result) <= 10, "不能超出预算宽度");
        assert_eq!(result, "中文中...");
    }

    #[test]
    fn truncation_never_splits_a_wide_character() {
        // 预算 7 列：2+2+2=6 可放 3 个汉字，第 4 个会超
        let result = truncate_preview(&"漢".repeat(20), 10);
        assert_eq!(result, "漢漢漢...");
        assert_eq!(display_width(&result), 9);
    }

    #[test]
    fn newlines_and_tabs_are_folded() {
        assert_eq!(truncate_preview("line1\nline2", 40), "line1⏎line2");
        assert_eq!(truncate_preview("a\tb", 40), "a b");
        assert_eq!(truncate_preview("a\r\nb", 40), "a⏎b");
    }

    #[test]
    fn tiny_budget_degrades_to_dots_without_panicking() {
        assert_eq!(truncate_preview("hello", 0), "");
        assert_eq!(truncate_preview("hello", 3), "...");
        assert_eq!(truncate_preview("hello", 1), ".");
    }

    #[test]
    fn row_fills_available_width_exactly() {
        let (prefix, preview, pad, suffix) = row_parts(0, "hi", 40);
        assert_eq!(prefix, "  1 ");
        assert_eq!(preview, "hi");
        assert_eq!(suffix, "(2 字)");
        let total = display_width(&prefix) + display_width(&preview) + pad + display_width(&suffix);
        assert_eq!(total, 40, "右侧字数必须正好贴到行尾");
    }

    #[test]
    fn row_truncates_long_text_and_keeps_minimum_gap() {
        let (prefix, preview, pad, suffix) = row_parts(9, &"y".repeat(200), 40);
        assert_eq!(prefix, " 10 ");
        assert!(preview.ends_with("..."));
        assert!(pad >= MIN_GAP, "预览与字数之间至少留 {MIN_GAP} 空格，实际 {pad}");
        assert!(display_width(&preview) <= MAX_PREVIEW_WIDTH);
        assert!(suffix.ends_with(" 字)"));
    }

    #[test]
    fn row_survives_narrow_terminal() {
        // 极窄终端下不能 panic，也不能出现减法溢出
        for width in [0, 1, 5, 12] {
            let (prefix, preview, pad, suffix) = row_parts(0, "内容", width);
            let _ = (prefix, preview, pad, suffix);
        }
    }

    #[test]
    fn row_prefix_is_right_aligned_for_double_digits() {
        assert_eq!(row_parts(99, "a", 40).0, "100 ");
        assert_eq!(row_parts(0, "a", 40).0, "  1 ");
    }
}
