//! 应用状态与按键分发。
//!
//! 这里刻意不依赖终端和剪贴板：`on_key` 返回一个 `Action` 由 main 去路由，
//! 因此整套交互逻辑都可以用普通单测覆盖。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::clipboard::AppEvent;
use crate::history::{History, Outcome};
use crate::storage;

/// 状态栏消息存活时长。
pub const STATUS_TTL: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// 等待 `y` 确认清空，防误触。
    ConfirmClear,
}

/// 按键处理结果，由 main 负责副作用。
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    Write(String),
}

pub struct App {
    pub history: History,
    /// 选中项的 id（不是索引）：MRU 重排后选中项能跟着原条目走。
    pub selected: Option<u64>,
    pub mode: Mode,
    /// 瞬时消息：(文本, 产生时刻)，由 `tick` 负责过期。
    pub status: Option<(String, Instant)>,
    /// 常驻告警（剪贴板不可用、存储失败），不会被 3 秒 TTL 清掉。
    pub alert: Option<String>,
    pub storage_path: Option<PathBuf>,
    /// 被敏感规则拦下的条数，仅用于展示。
    pub filtered: u64,
    pub should_quit: bool,
}

impl App {
    pub fn new(history: History, storage_path: Option<PathBuf>) -> Self {
        let selected = history.items().first().map(|entry| entry.id);
        Self {
            history,
            selected,
            mode: Mode::Normal,
            status: None,
            alert: None,
            storage_path,
            filtered: 0,
            should_quit: false,
        }
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.selected.and_then(|id| self.history.index_of(id))
    }

    pub fn selected_text(&self) -> Option<String> {
        let id = self.selected?;
        self.history.get(id).map(|entry| entry.text.clone())
    }

    pub fn status_line(&self) -> Option<&str> {
        self.status.as_ref().map(|(message, _)| message.as_str())
    }

    fn set_status(&mut self, message: impl Into<String>) {
        self.status = Some((message.into(), Instant::now()));
    }

    /// 消息过期。由主循环的 50ms 心跳驱动。
    pub fn tick(&mut self) {
        if let Some((_, created_at)) = &self.status {
            if created_at.elapsed() >= STATUS_TTL {
                self.status = None;
            }
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        // 原始模式下 Ctrl+C 是普通按键事件，不会被转成 SIGINT，需要自己处理。
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Action::Quit;
        }

        match self.mode {
            Mode::ConfirmClear => {
                self.mode = Mode::Normal;
                if key.code == KeyCode::Char('y') {
                    self.clear_all();
                } else {
                    self.set_status("已取消清空");
                }
                Action::None
            }
            Mode::Normal => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
                KeyCode::Char('j') | KeyCode::Down => {
                    self.move_selection(1);
                    Action::None
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.move_selection(-1);
                    Action::None
                }
                KeyCode::Enter => match self.selected_text() {
                    Some(text) => Action::Write(text),
                    None => {
                        self.set_status("没有可写回的条目");
                        Action::None
                    }
                },
                KeyCode::Char('d') => {
                    self.delete_selected();
                    Action::None
                }
                KeyCode::Char('D') => {
                    self.mode = Mode::ConfirmClear;
                    Action::None
                }
                _ => Action::None,
            },
        }
    }

    pub fn on_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::ClipboardChanged(text) => {
                let outcome = self.history.push_or_promote(text);
                let id = match outcome {
                    Outcome::Inserted(id) | Outcome::Promoted(id) => id,
                };
                // 之前没有选中项（历史为空）时，跟随最新一条。
                if self.selected.is_none() {
                    self.selected = Some(id);
                }
                self.normalize_selection();
                self.persist();
            }
            AppEvent::Filtered(rule) => {
                self.filtered += 1;
                self.set_status(format!("⚠ 已过滤疑似 {rule}，未写入历史"));
            }
            AppEvent::WriteDone { text, result } => match result {
                Ok(()) => {
                    // 写回算一次「使用」：提升到顶部，但不产生重复条目。
                    self.history.push_or_promote(text);
                    self.normalize_selection();
                    self.persist();
                    self.set_status("✓ 已复制到剪贴板");
                }
                Err(e) => self.set_status(format!("✗ 写回剪贴板失败: {e}")),
            },
            AppEvent::MonitorError(message) => {
                self.alert = Some(message);
            }
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.history.is_empty() {
            self.selected = None;
            return;
        }
        let current = self.selected_index().unwrap_or(0) as isize;
        let last = self.history.len() as isize - 1;
        let next = (current + delta).clamp(0, last) as usize;
        self.selected = Some(self.history.items()[next].id);
    }

    fn delete_selected(&mut self) {
        let Some(id) = self.selected else {
            self.set_status("没有可删除的条目");
            return;
        };
        let position = self.history.index_of(id).unwrap_or(0);
        if !self.history.remove(id) {
            return;
        }
        // 删除后优先选中「顶上来占了同一位置」的那条，删的是最后一条则退到新的末尾。
        self.selected = self
            .history
            .items()
            .get(position)
            .or_else(|| self.history.items().last())
            .map(|entry| entry.id);
        self.persist();
        self.set_status(format!("已删除第 {} 条", position + 1));
    }

    fn clear_all(&mut self) {
        self.history.clear();
        self.selected = None;
        self.persist();
        self.set_status("已清空全部历史");
    }

    /// 选中项被容量淘汰挤掉时的兜底修正。
    fn normalize_selection(&mut self) {
        if self.history.is_empty() {
            self.selected = None;
            return;
        }
        let still_present = self
            .selected
            .and_then(|id| self.history.index_of(id))
            .is_some();
        if !still_present {
            // 淘汰只发生在尾部，所以退到当前最后一条最接近用户原来的位置。
            self.selected = self.history.items().last().map(|entry| entry.id);
        }
    }

    /// 每次历史变更后落盘。失败只进状态栏，不中断运行。
    fn persist(&mut self) {
        let Some(path) = self.storage_path.clone() else {
            return;
        };
        let items = self.history.texts();
        if let Err(e) = storage::save(&path, &items) {
            self.alert = Some(format!("保存历史失败: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::AppEvent;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn app_with(entries: &[&str]) -> App {
        // storage_path = None：单测不碰磁盘
        App::new(
            History::from_texts(entries.iter().map(|s| s.to_string())),
            None,
        )
    }

    fn texts(app: &App) -> Vec<&str> {
        app.history
            .items()
            .iter()
            .map(|e| e.text.as_str())
            .collect()
    }

    #[test]
    fn selects_first_entry_on_startup() {
        let app = app_with(&["newest", "older"]);
        assert_eq!(app.selected_index(), Some(0));
    }

    #[test]
    fn first_clipboard_event_selects_the_new_entry() {
        let mut app = app_with(&[]);
        app.on_event(AppEvent::ClipboardChanged("hello".into()));

        assert_eq!(texts(&app), vec!["hello"]);
        assert_eq!(app.selected_index(), Some(0));
    }

    /// 用户正在浏览第 3 条时，来了新剪贴板内容，选中项不能跟着列表跳走。
    #[test]
    fn browsing_selection_stays_on_same_entry_when_new_content_arrives() {
        let mut app = app_with(&["c", "b", "a"]);
        let target_id = app.history.items()[2].id;
        app.selected = Some(target_id);
        assert_eq!(app.selected_index(), Some(2));

        app.on_event(AppEvent::ClipboardChanged("brand-new".into()));

        assert_eq!(texts(&app), vec!["brand-new", "c", "b", "a"]);
        assert_eq!(
            app.selected_index(),
            Some(3),
            "选中项必须还是同一条目，只是索引被挤到后面"
        );
    }

    #[test]
    fn promotion_keeps_selection_on_promoted_entry() {
        let mut app = app_with(&["c", "b", "a"]);
        let id_of_a = app.history.items()[2].id;
        app.selected = Some(id_of_a);

        app.on_event(AppEvent::ClipboardChanged("a".into()));

        assert_eq!(texts(&app), vec!["a", "c", "b"]);
        assert_eq!(app.selected, Some(id_of_a));
        assert_eq!(app.selected_index(), Some(0), "选中项随条目一起升到顶部");
    }

    #[test]
    fn evicted_selection_falls_back_to_oldest_remaining() {
        let mut app = app_with(&["a", "b"]);
        let id_of_oldest = app.history.items()[1].id;
        app.selected = Some(id_of_oldest);

        // 把历史塞满，逼出容量淘汰
        for i in 0..crate::history::CAP {
            app.on_event(AppEvent::ClipboardChanged(format!("item-{i}")));
        }

        assert_eq!(app.history.len(), crate::history::CAP);
        assert!(
            app.history.index_of(id_of_oldest).is_none(),
            "该条目应已被淘汰"
        );
        assert_eq!(
            app.selected_index(),
            Some(crate::history::CAP - 1),
            "选中项应退到当前最后一条，而不是越界"
        );
    }

    #[test]
    fn down_and_up_move_selection_without_wrapping() {
        let mut app = app_with(&["c", "b", "a"]);

        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.selected_index(), Some(1));
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.selected_index(), Some(2));

        app.on_key(key(KeyCode::Down));
        assert_eq!(app.selected_index(), Some(2), "到底后保持不动");

        app.on_key(key(KeyCode::Char('k')));
        assert_eq!(app.selected_index(), Some(1));
        app.on_key(key(KeyCode::Up));
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.selected_index(), Some(0), "到顶后保持不动");
    }

    #[test]
    fn enter_requests_write_and_does_not_mutate_history() {
        let mut app = app_with(&["c", "b", "a"]);
        app.on_key(key(KeyCode::Char('j')));

        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::Write("b".into()));
        assert_eq!(texts(&app), vec!["c", "b", "a"], "回车本身不改历史");
    }

    #[test]
    fn successful_write_back_promotes_without_duplicating() {
        let mut app = app_with(&["c", "b", "a"]);
        app.on_event(AppEvent::WriteDone {
            text: "a".into(),
            result: Ok(()),
        });

        assert_eq!(texts(&app), vec!["a", "c", "b"]);
        assert_eq!(app.history.len(), 3, "写回不能产生重复条目");
        assert_eq!(app.status_line(), Some("✓ 已复制到剪贴板"));
    }

    #[test]
    fn failed_write_back_reports_error_and_keeps_history() {
        let mut app = app_with(&["c", "b", "a"]);
        app.on_event(AppEvent::WriteDone {
            text: "a".into(),
            result: Err("boom".into()),
        });

        assert_eq!(texts(&app), vec!["c", "b", "a"]);
        assert!(app.status_line().unwrap().contains("写回剪贴板失败"));
    }

    #[test]
    fn delete_middle_selects_the_entry_that_took_its_place() {
        let mut app = app_with(&["c", "b", "a"]);
        app.on_key(key(KeyCode::Char('j'))); // 选中 b
        let id_of_a = app.history.items()[2].id;

        app.on_key(key(KeyCode::Char('d')));

        assert_eq!(texts(&app), vec!["c", "a"]);
        assert_eq!(app.selected, Some(id_of_a), "选中应落到顶上来的 a");
    }

    #[test]
    fn deleting_last_entry_falls_back_to_new_last() {
        let mut app = app_with(&["c", "b", "a"]);
        app.on_key(key(KeyCode::Char('j')));
        app.on_key(key(KeyCode::Char('j'))); // 选中最后一条 a
        let id_of_b = app.history.items()[1].id;

        app.on_key(key(KeyCode::Char('d')));

        assert_eq!(texts(&app), vec!["c", "b"]);
        assert_eq!(app.selected, Some(id_of_b));
    }

    #[test]
    fn deleting_the_only_entry_clears_selection() {
        let mut app = app_with(&["only"]);
        app.on_key(key(KeyCode::Char('d')));

        assert!(app.history.is_empty());
        assert_eq!(app.selected, None);
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None);
    }

    #[test]
    fn clear_requires_explicit_confirmation() {
        let mut app = app_with(&["c", "b", "a"]);

        app.on_key(key(KeyCode::Char('D')));
        assert_eq!(app.mode, Mode::ConfirmClear);
        assert_eq!(app.history.len(), 3, "进入确认态不能立即清空");

        app.on_key(key(KeyCode::Char('n')));
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.history.len(), 3, "非 y 键应取消");

        app.on_key(key(KeyCode::Char('D')));
        app.on_key(key(KeyCode::Char('y')));
        assert!(app.history.is_empty());
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.selected, None);
    }

    #[test]
    fn esc_in_confirm_mode_only_cancels() {
        let mut app = app_with(&["a"]);
        app.on_key(key(KeyCode::Char('D')));

        assert_eq!(app.on_key(key(KeyCode::Esc)), Action::None);
        assert!(!app.should_quit);
        assert_eq!(app.history.len(), 1);
    }

    #[test]
    fn q_esc_and_ctrl_c_quit() {
        for code in [KeyCode::Char('q'), KeyCode::Esc] {
            let mut app = app_with(&["a"]);
            assert_eq!(app.on_key(key(code)), Action::Quit);
        }

        let mut app = app_with(&["a"]);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(app.on_key(ctrl_c), Action::Quit);
    }

    #[test]
    fn status_expires_after_ttl() {
        let mut app = app_with(&["a"]);
        app.on_event(AppEvent::Filtered("github-token"));

        assert_eq!(app.filtered, 1);
        assert!(app.status_line().is_some());

        app.tick();
        assert!(app.status_line().is_some(), "3 秒内不应过期");

        app.status = Some(("旧消息".into(), Instant::now() - STATUS_TTL));
        app.tick();
        assert_eq!(app.status_line(), None);
    }

    #[test]
    fn monitor_error_becomes_persistent_alert() {
        let mut app = app_with(&["a"]);
        app.on_event(AppEvent::MonitorError("剪贴板不可用: nope".into()));
        assert_eq!(app.alert.as_deref(), Some("剪贴板不可用: nope"));

        app.tick();
        assert!(app.alert.is_some(), "常驻告警不应被 TTL 清掉");
    }
}
