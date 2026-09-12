//! 按终端**显示列数**处理文本：折叠换行、计算宽度、安全截断。
//!
//! 这些是从被替换掉的 ratatui 版本里原样搬过来的——它们与 UI 框架无关，
//! 而且踩过的坑都还在（见下面注释），所以连同测试一起保留。

use unicode_width::UnicodeWidthChar;

/// 字符串在终端里占用的列数（CJK 全角算 2 列，控制字符算 0 列）。
pub fn display_width(text: &str) -> usize {
    text.chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

/// 把换行/制表符折叠成单行可见字符。
///
/// 剪贴板里多行文本极常见，不折叠的话列表项会被撑成多行、把布局顶乱。
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
/// 逐字符累加 `UnicodeWidthChar::width`，因此：
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

/// 列表项的文案：`预览 (N 字)`。
///
/// `max_width` 是给整条文案（含字数后缀）的预算；空间不足时优先保住正文，
/// 实在放不下才丢掉字数后缀——正文才是这个列表的意义所在。
pub fn entry_label(text: &str, max_width: usize) -> String {
    let count = format!("({} 字)", text.chars().count());
    let gap = 2;

    let reserved = display_width(&count) + gap;
    if max_width > reserved + 3 {
        let preview = truncate_to_width(text, max_width - reserved);
        return format!("{preview}{}{count}", " ".repeat(gap));
    }

    // 窄终端：放弃字数后缀，保住正文
    truncate_to_width(text, max_width)
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
    fn label_appends_character_count() {
        let label = entry_label("hello", 40);
        assert_eq!(label, "hello  (5 字)");
        assert_eq!(display_width(&label), 13);
    }

    #[test]
    fn label_keeps_text_when_too_narrow_for_the_count() {
        // 回归（沿袭 ratatui 版）：窄宽度下不能只剩序号和 (N 字)、正文消失
        let label = entry_label("Review this pull request", 12);
        assert!(label.contains('R'), "正文被字数统计挤没了: {label:?}");
        assert!(!label.contains("字)"), "空间不足时应先放弃字数统计: {label:?}");
        assert!(display_width(&label) <= 12);
    }

    #[test]
    fn label_never_exceeds_budget() {
        let samples = [
            "Review this pull request",
            "今天下午的会议纪要：讨论了三个方案，最终决定先做交互原型，下周再评估数据层",
            "https://example.com/a/very/long/path?q=1&x=2",
            "短",
            "",
        ];
        for width in 0..80usize {
            for text in samples {
                let label = entry_label(text, width);
                assert!(
                    display_width(&label) <= width,
                    "width={width} text={text:?} 得到 {:?}（{} 列）",
                    label,
                    display_width(&label)
                );
            }
        }
    }
}
