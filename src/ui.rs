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

/// 正文至少要留出的列数。空间不够时，它是最后被牺牲的东西。
const MIN_PREVIEW_WIDTH: usize = 4;

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
///
/// 逐字符累加 `UnicodeWidthChar::width`（CJK 全角算 2 列），因此：
/// - 不会把一个宽字符切成半截；
/// - 永远不会在多字节字符中间切片。直接写 `&s[..n]` 会在中文上 panic。
pub fn truncate_to_width(text: &str, max_width: usize) -> String {
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
/// 优先级是 **正文 > 序号 > 字数**，空间不足时逐级降级：
/// 1. `序号 + 正文 + 空隙 + 字数`（常规形态）；
/// 2. 放不下就先丢掉字数统计——三者里它最不重要，正文才是这个列表的意义；
/// 3. 再放不下就只留正文。
///
/// 这个阶梯是必需的：ratatui 对超宽行是从右侧**静默裁掉**的（不会自己补省略号），
/// 所以只要拼出来的行超过 `row_width`，行尾的字数统计就会连同省略号一起消失；
/// 而如果把字数当成不可让步的固定宽度，窄终端下正文会被挤成空字符串，
/// 只剩下一行「序号 + (N 字)」。
///
/// 返回值保证：前缀宽度 + 预览宽度 + pad + 字数宽度 <= row_width。
pub fn row_parts(index: usize, text: &str, row_width: usize) -> (String, String, usize, String) {
    let prefix = format!("{:>3} ", index + 1);
    let count = format!("({} 字)", text.chars().count());

    let prefix_width = display_width(&prefix);
    let count_width = display_width(&count);

    // 第 1 级：完整形态，正文、序号、字数都在
    if row_width >= prefix_width + MIN_PREVIEW_WIDTH + MIN_GAP + count_width {
        let budget = (row_width - prefix_width - MIN_GAP - count_width).min(MAX_PREVIEW_WIDTH);
        let preview = truncate_to_width(text, budget);
        let pad = row_width - prefix_width - display_width(&preview) - count_width;
        return (prefix, preview, pad, count);
    }

    // 第 2 级：放弃字数统计，保住正文
    if row_width >= prefix_width + MIN_PREVIEW_WIDTH {
        let budget = (row_width - prefix_width).min(MAX_PREVIEW_WIDTH);
        let preview = truncate_to_width(text, budget);
        let pad = row_width - prefix_width - display_width(&preview);
        return (prefix, preview, pad, String::new());
    }

    // 第 3 级：极端窄，只留正文
    let preview = truncate_to_width(text, row_width.min(MAX_PREVIEW_WIDTH));
    let pad = row_width - display_width(&preview);
    (String::new(), preview, pad, String::new())
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
        assert_eq!(truncate_to_width("hello", 40), "hello");
        assert_eq!(truncate_to_width("", 40), "");
    }

    #[test]
    fn exact_fit_is_not_treated_as_overflow() {
        let text = "a".repeat(40);
        let result = truncate_to_width(&text, 40);
        assert_eq!(result, text);
        assert!(!result.ends_with("..."), "恰好等宽不应补省略号");
    }

    #[test]
    fn long_ascii_text_is_truncated_with_ellipsis() {
        let result = truncate_to_width(&"x".repeat(100), 10);
        assert_eq!(result, "xxxxxxx...");
        assert_eq!(display_width(&result), 10);
    }

    #[test]
    fn long_cjk_text_is_truncated_on_character_boundary() {
        let text = "中文".repeat(30);
        let result = truncate_to_width(&text, 10);

        assert!(result.ends_with("..."));
        assert!(display_width(&result) <= 10, "不能超出预算宽度");
        assert_eq!(result, "中文中...");
    }

    #[test]
    fn truncation_never_splits_a_wide_character() {
        // 预算 7 列：2+2+2=6 可放 3 个汉字，第 4 个会超
        let result = truncate_to_width(&"漢".repeat(20), 10);
        assert_eq!(result, "漢漢漢...");
        assert_eq!(display_width(&result), 9);
    }

    #[test]
    fn newlines_and_tabs_are_folded() {
        assert_eq!(truncate_to_width("line1\nline2", 40), "line1⏎line2");
        assert_eq!(truncate_to_width("a\tb", 40), "a b");
        assert_eq!(truncate_to_width("a\r\nb", 40), "a⏎b");
    }

    #[test]
    fn tiny_budget_degrades_to_dots_without_panicking() {
        assert_eq!(truncate_to_width("hello", 0), "");
        assert_eq!(truncate_to_width("hello", 3), "...");
        assert_eq!(truncate_to_width("hello", 1), ".");
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

    // --- 回归：窄宽度下「正文被右侧字数统计挤掉」---

    #[test]
    fn narrow_row_keeps_text_instead_of_dropping_it_for_the_count() {
        let (_, preview, _, _) = row_parts(0, "Review this pull request", 12);
        assert!(
            !preview.is_empty(),
            "宽度不足时正文被字数统计挤空了，行里只剩下序号和 (N 字)"
        );
    }

    #[test]
    fn count_is_abandoned_before_the_text_when_space_is_tight() {
        let (_, preview, _, suffix) = row_parts(0, "Review this pull request", 12);
        assert_eq!(suffix, "", "空间不足时应先放弃字数统计，而不是放弃正文");
        assert!(!preview.is_empty());
    }

    #[test]
    fn count_is_kept_when_there_is_enough_room() {
        let long = "Review this pull request carefully and then merge it";
        let (prefix, preview, pad, suffix) = row_parts(0, long, 46);
        assert!(suffix.starts_with('(') && suffix.ends_with(" 字)"), "实际 {suffix:?}");
        assert!(preview.ends_with("..."));
        let total = display_width(&prefix) + display_width(&preview) + pad + display_width(&suffix);
        assert_eq!(total, 46);
    }

    /// ratatui 会直接从右侧裁掉超宽内容（不会自己补省略号），
    /// 所以拼出来的行一旦超过可用宽度，行尾的字数统计就会静默消失。
    #[test]
    fn composed_row_never_exceeds_available_width() {
        let samples = [
            "Review this pull request",
            "今天下午的会议纪要：讨论了三个方案，最终决定先做交互原型",
            "https://example.com/a/very/long/path?q=1&x=2",
            "短",
            "",
        ];
        for width in 0..80usize {
            for text in samples {
                let (prefix, preview, pad, suffix) = row_parts(3, text, width);
                let total =
                    display_width(&prefix) + display_width(&preview) + pad + display_width(&suffix);
                assert!(
                    total <= width,
                    "width={width} text={text:?} 拼出 {total} 列，超出部分会被 ratatui 裁掉"
                );
            }
        }
    }

    #[test]
    fn extremely_narrow_rows_degrade_gracefully() {
        for width in 0..=8usize {
            let (prefix, preview, pad, suffix) = row_parts(0, "Review", width);
            let total = display_width(&prefix) + display_width(&preview) + pad + display_width(&suffix);
            assert!(total <= width, "width={width} 溢出到 {total} 列");
        }
    }
}

/// 缓冲区级回归测试：直接量 ratatui 真正写进 `Buffer` 的内容。
///
/// 宽度算错、行被静默裁掉、边框被覆盖，都会在这里暴露，不需要靠肉眼盯终端。
#[cfg(test)]
mod render_tests {
    use super::*;
    use crate::app::App;
    use crate::history::History;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// 渲染一帧并按行取回可见文本。
    fn render_lines(app: &App, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();

        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| {
                        buffer
                            .cell((x, y))
                            .map(|cell| cell.symbol().to_string())
                            .unwrap_or_default()
                    })
                    .collect()
            })
            .collect()
    }

    /// 布局：0=标题，1..=5=带边框的列表，2/3/4=三条记录，6=快捷键，7=状态。
    const FIRST_ITEM_ROW: usize = 2;

    fn demo() -> App {
        App::new(
            History::from_texts(vec![
                "Review this pull request carefully".to_string(),
                "今天下午的会议纪要：讨论了三个方案，最终决定先做交互原型，下周再评估数据层".to_string(),
                "https://example.com/a/very/long/url/that/keeps/going?q=1".to_string(),
            ]),
            None,
        )
    }

    /// 调试入口：`cargo test dump_render_evidence -- --nocapture`
    #[test]
    fn dump_render_evidence() {
        for width in [70u16, 50, 34, 24, 16, 12] {
            eprintln!("======== width={width} ========");
            for row in render_lines(&demo(), width, 8) {
                eprintln!("|{row}|");
            }
        }
    }

    /// 回归：正文开头的字符必须完整显示，不能被任何东西盖掉。
    #[test]
    fn item_row_never_loses_its_leading_characters() {
        for width in [70u16, 50, 34, 24] {
            let rows = render_lines(&demo(), width, 8);
            let row = &rows[FIRST_ITEM_ROW];
            assert!(
                row.contains("Revi"),
                "width={width} 时首字符丢失或被覆盖，实际行: {row:?}"
            );
        }
    }

    /// 回归：窄终端下曾经只剩「▸   1  (34 字 )」，正文被字数统计挤成空字符串。
    #[test]
    fn narrow_terminal_still_shows_text_instead_of_only_the_count() {
        for width in [16u16, 18, 20, 22] {
            let rows = render_lines(&demo(), width, 8);
            let row = &rows[FIRST_ITEM_ROW];
            assert!(
                row.contains('R'),
                "width={width} 时正文消失了，实际行: {row:?}"
            );
        }
    }

    /// 空间充足时，字数统计要保留并贴齐右边框。
    ///
    /// 两处偷不得懒：
    /// - 不能用 `&row[1..len-1]` 按字节切片，`│` 占 3 字节，会直接 panic；
    /// - 宽字符在 buffer 里占两个格子，第二格是填充空格，
    ///   所以 `字` 和 `)` 在缓冲区里并不相邻，不能直接断言 `ends_with("字)")`。
    #[test]
    fn count_is_kept_and_right_aligned_when_there_is_room() {
        let rows = render_lines(&demo(), 60, 8);
        let row = &rows[FIRST_ITEM_ROW];

        assert!(row.starts_with('│') && row.ends_with('│'), "边框缺失: {row:?}");

        // 跳过左边框（按字符而不是字节），再去掉右边框
        let inner: String = row.chars().skip(1).collect();
        let inner = inner.trim_end_matches('│');

        assert!(inner.contains('字'), "字数统计丢失: {row:?}");
        assert!(
            inner.trim_end().ends_with(')'),
            "字数统计应贴齐右边框，实际: {row:?}"
        );
    }

    /// CJK 行：截断点必须落在字符边界上，边框不能被写穿。
    #[test]
    fn cjk_row_truncates_cleanly_without_touching_borders() {
        for width in [12u16, 16, 24, 34, 50] {
            let rows = render_lines(&demo(), width, 8);
            for row in &rows[FIRST_ITEM_ROW..=FIRST_ITEM_ROW + 2] {
                assert!(row.starts_with('│'), "width={width} 左边框被覆盖: {row:?}");
                assert!(row.ends_with('│'), "width={width} 右边框被覆盖: {row:?}");
            }
        }
    }
}
