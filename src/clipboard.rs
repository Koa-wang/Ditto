//! 后台剪贴板监听线程与线程间协议。
//!
//! 单一所有权原则：`arboard::Clipboard` **只由监听线程持有**，主线程从不直接
//! 调用剪贴板 API，写回也走 `MonitorCmd::Write` 命令。
//!
//! 这样做有两个好处：
//! 1. 写回与轮询共用同一个 `last_seen`，天然消除「自己写回的内容又被当成外部
//!    新复制」的竞态；
//! 2. 避免 Linux/X11 下双实例争抢剪贴板所有权（X11 需要进程存活以应答粘贴请求，
//!    所以实例必须活到程序结束，不能每次轮询临时创建一个）。

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::filter;

/// 剪贴板轮询间隔。
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 监听线程 → 主线程。
#[derive(Debug)]
pub enum AppEvent {
    /// 外部新复制的文本，已通过空文本与敏感内容过滤。
    ClipboardChanged(String),
    /// 命中了敏感规则，携带规则名，仅用于状态栏提示。
    Filtered(&'static str),
    /// 写回剪贴板的结果。携带原文，便于主线程在成功后把它提升到顶部。
    WriteDone { text: String, result: Result<(), String> },
    /// 剪贴板不可用（初始化失败），只发送一次。
    MonitorError(String),
}

/// 主线程 → 监听线程。
#[derive(Debug)]
pub enum MonitorCmd {
    Write(String),
    /// 立即退出，避免 join 时干等一个 500ms 超时。
    Shutdown,
}

pub struct Monitor {
    handle: JoinHandle<()>,
    cmd_tx: Sender<MonitorCmd>,
    /// 主线程用 `try_recv` 非阻塞排空，因此剪贴板更新永远不会卡住按键读取。
    pub events: Receiver<AppEvent>,
}

impl Monitor {
    pub fn spawn() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let handle = thread::spawn(move || run(cmd_rx, event_tx));

        Self {
            handle,
            cmd_tx,
            events: event_rx,
        }
    }

    /// 请求写回剪贴板。返回 false 表示监听线程已退出（剪贴板不可用）。
    pub fn write(&self, text: String) -> bool {
        self.cmd_tx.send(MonitorCmd::Write(text)).is_ok()
    }

    /// 通知退出并等待线程结束，保证没有悬挂线程。
    pub fn shutdown(self) {
        let _ = self.cmd_tx.send(MonitorCmd::Shutdown);
        let _ = self.handle.join();
    }
}

/// 一次轮询的处置结果。
#[derive(Debug, PartialEq, Eq)]
enum Reaction {
    /// 没变化或空文本：不上报。
    Skip,
    Changed,
    Filtered(&'static str),
}

/// 决定这次读到的剪贴板内容该怎么处理，并同步 `last_seen`。
///
/// 抽成纯函数是为了能测到最关键的顺序问题：**刷新 `last_seen` 必须发生在过滤
/// 判断之前**，否则同一段敏感文本会在剪贴板里躺多久、状态栏就每 500ms 弹一次。
///
/// 只借用 `text` 而不拿走所有权：上报时由调用方直接移动原字符串，避免为了
/// 同时用于比较和发送而复制一份大文本。
fn react(last_seen: &mut Option<String>, text: &str) -> Reaction {
    if last_seen.as_deref() == Some(text) {
        return Reaction::Skip;
    }

    let is_blank = text.trim().is_empty();
    let rule = if is_blank {
        None
    } else {
        filter::is_sensitive(text)
    };

    // 先刷新 last_seen，再决定上报什么。
    *last_seen = Some(text.to_owned());

    match (is_blank, rule) {
        (true, _) => Reaction::Skip,
        (false, Some(rule)) => Reaction::Filtered(rule),
        (false, None) => Reaction::Changed,
    }
}

fn run(cmd_rx: Receiver<MonitorCmd>, event_tx: Sender<AppEvent>) {
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(clipboard) => clipboard,
        Err(e) => {
            let _ = event_tx.send(AppEvent::MonitorError(format!("剪贴板不可用: {e}")));
            // 仍然保持响应，直到主线程要求退出。
            while let Ok(cmd) = cmd_rx.recv() {
                if matches!(cmd, MonitorCmd::Shutdown) {
                    return;
                }
            }
            return;
        }
    };

    let mut last_seen: Option<String> = None;

    loop {
        // recv_timeout 兼作「命令处理」与「轮询节拍」：等待期间到达的写回请求会
        // 立即执行，不必等满 500ms，也不会被打断睡眠。
        match cmd_rx.recv_timeout(POLL_INTERVAL) {
            Ok(MonitorCmd::Write(text)) => {
                let result = clipboard.set_text(text.clone());
                if result.is_ok() {
                    last_seen = Some(text.clone());
                }
                let _ = event_tx.send(AppEvent::WriteDone {
                    text,
                    result: result.map_err(|e| e.to_string()),
                });
            }
            Ok(MonitorCmd::Shutdown) => return,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }

        // 读取失败（例如 macOS 上其他应用正持有剪贴板）视为「本轮无内容」，
        // 不刷新 last_seen，也不算错误。
        if let Ok(text) = clipboard.get_text() {
            match react(&mut last_seen, &text) {
                Reaction::Skip => {}
                // 移动而不是 clone：text 在这里用完即弃。
                Reaction::Changed => {
                    let _ = event_tx.send(AppEvent::ClipboardChanged(text));
                }
                Reaction::Filtered(rule) => {
                    let _ = event_tx.send(AppEvent::Filtered(rule));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_read_is_reported() {
        let mut last_seen = None;
        assert_eq!(react(&mut last_seen, "hello"), Reaction::Changed);
        assert_eq!(last_seen.as_deref(), Some("hello"));
    }

    #[test]
    fn identical_content_is_skipped() {
        let mut last_seen = Some("hello".to_string());
        assert_eq!(react(&mut last_seen, "hello"), Reaction::Skip);
    }

    #[test]
    fn blank_content_is_never_reported() {
        let mut last_seen = None;
        assert_eq!(react(&mut last_seen, "   \n\t "), Reaction::Skip);
        assert_eq!(react(&mut last_seen, ""), Reaction::Skip);
    }

    #[test]
    fn blank_read_still_updates_last_seen() {
        let mut last_seen = Some("hello".to_string());
        assert_eq!(react(&mut last_seen, "   "), Reaction::Skip);
        assert_eq!(last_seen.as_deref(), Some("   "));
    }

    #[test]
    fn sensitive_content_reports_rule_name() {
        let mut last_seen = None;
        let token = format!("ghp_{}", "a1B2c3D4e5".repeat(4));
        assert_eq!(
            react(&mut last_seen, &token),
            Reaction::Filtered("github-token")
        );
        assert_eq!(last_seen.as_deref(), Some(token.as_str()));
    }

    /// 回归测试：敏感文本必须只提示一次，而不是每 500ms 弹一次。
    #[test]
    fn sensitive_content_only_notifies_once() {
        let mut last_seen = None;
        let token = format!("ghp_{}", "a1B2c3D4e5".repeat(4));

        assert_eq!(
            react(&mut last_seen, &token),
            Reaction::Filtered("github-token")
        );
        assert_eq!(react(&mut last_seen, &token), Reaction::Skip);
    }

    #[test]
    fn write_result_marks_content_as_seen() {
        // 模拟监听线程里 Write 成功后的处理：last_seen 被刷新，
        // 于是紧接着的轮询不会把自己写回的内容当成外部新复制。
        let written = "from-the-app".to_string();
        let mut last_seen = Some(written.clone());

        assert_eq!(react(&mut last_seen, &written), Reaction::Skip);
    }
}
