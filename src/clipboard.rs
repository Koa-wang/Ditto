//! 后台剪贴板轮询线程：读剪贴板 → 写 SQLite → 用 `cb_sink` 通知 UI 刷新。
//!
//! # `cb_sink` 是怎么被唤醒的（读源码实测，cursive 文档那句话极具误导性）
//!
//! cursive 文档只说 *"Callbacks will be executed in the order of arrival on the
//! next event cycle."* —— 看起来像是"得等下一次用户输入才会执行"。实测 cursive
//! 0.21.1 / cursive_core 0.4.7 的真实机制是：
//!
//! 1. `CbSink` 就是 `crossbeam_channel::unbounded()` 的 Sender，`send` 永不阻塞
//!    （`cursive_root.rs:75,90`）。
//! 2. 事件循环每轮调用 `process_callback()`，它用 `try_recv()` 在 `while` 里
//!    一次性排空所有待处理回调（`cursive_root.rs:831-839`）。
//! 3. 循环**不是**阻塞在 read 上：crossterm 后端的 `poll_event()` 是
//!    `poll(1ms)`，空闲时返回 `None`（`backends/crossterm.rs:335`）；
//!    空闲时 `post_events` 只 `sleep(30ms)` 就进入下一轮（`cursive_run.rs:176`）。
//! 4. 处理过回调的那一帧 `boring = false`，于是 `post_events` 直接走
//!    `refresh()` 分支（`cursive_run.rs:151-168`）。
//!
//! **结论：即便 `fps = 0`（默认值就是 0），循环本身也在 ~33Hz 空转，后台线程
//! 发出的刷新请求约 30ms 内就会被执行并重绘，不需要 `set_fps`。**
//! `fps` 只决定"没有事件时要不要定时重绘"（时钟/动画那类需求）。
//!
//! # 写回剪贴板为什么不在 UI 线程直接做
//!
//! 保持单一所有权：`arboard::Clipboard` **只由本线程持有**。UI 线程按 Enter 时
//! 发 `Cmd::Write` 过来，由本线程写回并同步 `last_seen`。这样"自己写回的内容"
//! 不会被下一轮轮询当成"外部新复制"；也避免 Linux/X11 下双实例争抢剪贴板
//! 所有权（X11 需要进程存活才能应答粘贴请求，实例必须活到程序结束）。

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use cursive::CbSink;

use crate::db::Db;
use crate::filter;

/// 剪贴板轮询间隔。
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 主线程 → 监听线程。
#[derive(Debug)]
pub enum Cmd {
    Write(String),
    /// 立即退出，避免 join 时干等一个 500ms 超时。
    Shutdown,
}

pub struct Monitor {
    cmd_tx: Sender<Cmd>,
    handle: JoinHandle<()>,
}

impl Monitor {
    pub fn spawn(db: Arc<Mutex<Db>>, sink: CbSink) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let handle = thread::spawn(move || run(db, sink, cmd_rx));
        Self { cmd_tx, handle }
    }

    /// 供 UI 回调投递命令用的发送端副本。
    ///
    /// 只暴露 `self.cmd_tx` 而不另写一个 `write` 包装：UI 回调拿到的是从
    /// `user_data` 里借出来的共享状态，直接 `send` 比再绕一层更清楚。
    pub fn sender(&self) -> Sender<Cmd> {
        self.cmd_tx.clone()
    }

    pub fn shutdown(self) {
        let _ = self.cmd_tx.send(Cmd::Shutdown);
        let _ = self.handle.join();
    }
}

/// 一次轮询的处置结果。
#[derive(Debug, PartialEq, Eq)]
enum Reaction {
    Skip,
    Changed,
    Filtered(&'static str),
}

/// 决定这次读到的内容怎么处理，并同步 `last_seen`。
///
/// 抽成纯函数是为了能测住最关键的顺序问题：**刷新 `last_seen` 必须发生在过滤
/// 判断之前**，否则同一段敏感文本会在剪贴板里躺多久、状态栏就每 500ms 弹一次。
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

    // 先刷新 last_seen，再决定做什么。
    *last_seen = Some(text.to_owned());

    match (is_blank, rule) {
        (true, _) => Reaction::Skip,
        (false, Some(rule)) => Reaction::Filtered(rule),
        (false, None) => Reaction::Changed,
    }
}

fn run(db: Arc<Mutex<Db>>, sink: CbSink, cmd_rx: Receiver<Cmd>) {
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(clipboard) => clipboard,
        Err(e) => {
            notify_status(&sink, format!("剪贴板不可用: {e}"));
            // 仍然保持响应，直到主线程要求退出。
            while let Ok(cmd) = cmd_rx.recv() {
                if matches!(cmd, Cmd::Shutdown) {
                    return;
                }
            }
            return;
        }
    };

    let mut last_seen: Option<String> = None;

    loop {
        // recv_timeout 兼作「命令处理」与「轮询节拍」：等待期间到达的写回请求
        // 会立即执行，不必等满 500ms。
        match cmd_rx.recv_timeout(POLL_INTERVAL) {
            Ok(Cmd::Write(text)) => {
                if clipboard.set_text(text.clone()).is_ok() {
                    last_seen = Some(text);
                } else {
                    notify_status(&sink, "✗ 写回剪贴板失败".to_string());
                }
            }
            Ok(Cmd::Shutdown) => return,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }

        // 读取失败（例如 macOS 上其他应用正持有剪贴板）视为「本轮无内容」，
        // 不刷新 last_seen，也不算错误。
        if let Ok(text) = clipboard.get_text() {
            match react(&mut last_seen, &text) {
                Reaction::Skip => {}
                Reaction::Filtered(rule) => {
                    notify_status(&sink, format!("⚠ 已过滤疑似 {rule}，未写入历史"));
                }
                Reaction::Changed => match store(&db, &text) {
                    Ok(()) => {
                        // 通知 UI 重新查库并重绘。见文件头：不需要 set_fps。
                        let _ = sink.send(Box::new(crate::refresh_list));
                    }
                    Err(e) => notify_status(&sink, format!("✗ {e}")),
                },
            }
        }
    }
}

/// 写库 + 裁剪。返回前一定释放锁。
///
/// 锁的作用域被刻意限制在这次调用内：**持锁期间绝不调用 cursive**，
/// 否则会形成「UI 线程持 DB 锁 → 触发回调 → 回调里再要 DB 锁」的自锁路径。
fn store(db: &Arc<Mutex<Db>>, text: &str) -> Result<(), String> {
    // 锁中毒（别的线程持锁时 panic）不该让轮询线程直接死掉：
    // SQLite 自身有事务保证，锁里的数据仍然是可用的，取回内层值继续用。
    let guard = db.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.upsert(text)?;
    guard.prune()?;
    Ok(())
}

fn notify_status(sink: &CbSink, message: String) {
    let _ = sink.send(Box::new(move |siv| crate::set_status(siv, &message)));
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

    /// 回归：敏感文本只提示一次，而不是每 500ms 弹一次。
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
    fn write_back_marks_content_as_seen() {
        // 模拟写回成功后 last_seen 被刷新：紧接着的轮询不会把自己写回的内容
        // 当成外部新复制。
        let written = "from-the-app".to_string();
        let mut last_seen = Some(written.clone());
        assert_eq!(react(&mut last_seen, &written), Reaction::Skip);
    }

    #[test]
    fn store_writes_and_prunes() {
        let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
        store(&db, "from-the-poller").unwrap();

        let guard = db.lock().unwrap();
        assert_eq!(guard.len().unwrap(), 1);
    }
}
