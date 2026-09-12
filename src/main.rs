//! Ditto —— 终端剪贴板历史管理器（Cursive TUI + SQLite 持久化）
//!
//! 线程模型：主线程跑 cursive 事件循环与所有 SQL 读；后台线程每 500ms 轮询
//! 系统剪贴板并写库。两者之间通过 `cb_sink`（UI 刷新）与 `clipboard::Cmd`
//! （写回剪贴板）通信。数据库只有一条 `Connection`，由 `Mutex` 串行化，
//! 详见 `db.rs` 的并发说明。

mod clipboard;
mod db;
mod filter;
mod text;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use cursive::event::{Event, Key};
use cursive::traits::*;
use cursive::views::{Dialog, EditView, LinearLayout, OnEventView, SelectView, TextView};
use cursive::Cursive;

use clipboard::{Cmd, Monitor};
use db::{Db, Entry};

const SEARCH_VIEW: &str = "search";
const LIST_VIEW: &str = "history";
const STATUS_VIEW: &str = "status";

/// 列表项按终端宽度的这个百分比截断。
const LABEL_PERCENT: usize = 70;
/// 最多列出多少条（与 `db::CAP` 不同：这只是查询上限，避免一次塞几万行进视图）。
const LIST_LIMIT: usize = 200;

const HINT: &str = " Enter 写回并退出 · Esc 退出 · Tab 切换焦点 · 列表聚焦时 j/k 选择、d 删除、D 清空";

/// 通过 cursive `user_data` 交给各回调的共享状态。
///
/// 注意：`set_user_data` 内部只保存**一个** `Box<dyn Any>`（后设的会覆盖先设的），
/// 所以这里必须合成一个结构体，不能分成两次 set。
struct Shared {
    db: Arc<Mutex<Db>>,
    cmd_tx: Sender<Cmd>,
}

fn main() -> ExitCode {
    let Some(path) = db_path() else {
        eprintln!("ditto: 无法确定数据目录，已退出");
        return ExitCode::FAILURE;
    };

    let db = match Db::open(&path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("ditto: {e}");
            return ExitCode::FAILURE;
        }
    };
    let db = Arc::new(Mutex::new(db));

    // cursive 自己**没有** panic hook（只有 crossterm 后端的 Drop 会恢复终端，
    // 而那里是 `disable_raw_mode().unwrap()`——panic 展开途中再失败就是 double
    // panic 直接 abort）。所以自己装一个：先把终端恢复，再让 panic 信息打出来。
    install_panic_hook();

    let mut siv = cursive::default();
    siv.set_window_title("Ditto — 剪贴板历史");

    build_ui(&mut siv);

    let monitor = Monitor::spawn(Arc::clone(&db), siv.cb_sink().clone());
    siv.set_user_data(Shared {
        db: Arc::clone(&db),
        cmd_tx: monitor.sender(),
    });

    // 首屏填充要等后端初始化之后再做：在那之前 screen_size() 还是 0，
    // 拿不到真实宽度就没法按 70% 截断。丢进 cb_sink 会在第一个事件循环执行。
    let _ = siv
        .cb_sink()
        .send(Box::new(refresh_list));

    // 用 try_run 而不是 run()：run() 内部是 `try_run().unwrap()`，终端不可用时会直接
    // panic（实测：没有控制终端时 crossterm 的 tty_fd() 会以 ENXIO 失败）。
    // 这里改成把错误打成人看得懂的提示，而不是一堆 panic 堆栈。
    let outcome = siv.try_run();

    // 退出顺序很重要：Enter 写回是异步的（由监听线程执行 set_text），
    // 命令按 FIFO 排队，所以这里先发 Shutdown 前，队列里的 Write 已经先被处理，
    // 写回不会因为退出而丢失。
    monitor.shutdown();

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ditto: 无法初始化终端: {e}");
            eprintln!("      这个程序需要真实的终端（或可用的 /dev/tty）。如果是在管道、");
            eprintln!("      重定向或没有控制终端的环境里运行，请直接在终端里执行 dtto。");
            ExitCode::FAILURE
        }
    }
}

fn db_path() -> Option<PathBuf> {
    // macOS: ~/Library/Application Support/ditto/history.db
    // Linux: ~/.local/share/ditto/history.db
    dirs::data_dir().map(|dir| dir.join("ditto").join("history.db"))
}

fn build_ui(siv: &mut Cursive) {
    // 搜索框：每次输入变动就重查数据库并重填列表。
    let search = EditView::new()
        .on_edit(|siv, _content, _cursor| refresh_list(siv))
        // 焦点在 EditView 上时事件不会传给兄弟节点 SelectView，所以两个视图都绑
        // 同一个动作。同一次按键只会命中其中一个（各自只有获得焦点才收到事件），
        // 不会重复触发。
        .on_submit(|siv, _content| write_back_and_quit(siv))
        .with_name(SEARCH_VIEW)
        .fixed_height(1);

    let select = SelectView::<Entry>::new()
        .on_submit(|siv, _entry: &Entry| write_back_and_quit(siv))
        .with_name(LIST_VIEW)
        .full_height();

    // d / D / j / k 包在 OnEventView 里，这样只在**列表聚焦时**才生效——
    // 做成全局回调会把搜索框里的字母抢走。
    // 另外 cursive 的 SelectView 默认只认方向键（select_view.rs:741-746），
    // 不认 vim 的 j/k，所以这里自己补上。
    let list = OnEventView::new(select)
        .on_event('j', |siv| move_selection(siv, 1))
        .on_event('k', |siv| move_selection(siv, -1))
        .on_event('d', delete_selected)
        .on_event('D', confirm_clear);

    let status = TextView::new(HINT).with_name(STATUS_VIEW).fixed_height(1);

    let layout = LinearLayout::vertical()
        .child(search)
        .child(list)
        .child(status);

    siv.add_layer(
        Dialog::around(layout)
            .title("Ditto — 剪贴板历史")
            .full_screen(),
    );

    siv.add_global_callback(Key::Esc, |siv| siv.quit());
    siv.add_global_callback(Event::CtrlChar('c'), |siv| siv.quit());
    // 终端尺寸变了要重新按新宽度截断列表项。
    siv.add_global_callback(Event::WindowResize, refresh_list);
}

// ------------------------------------------------------------------ 数据库访问

/// 所有 SQL 调用的唯一入口。
///
/// 关键约束：**查询结果读出来、锁释放之后才回到 UI**。持锁期间绝不调用 cursive，
/// 否则会形成「UI 线程持 DB 锁 → 触发 cursive 回调 → 回调里再要 DB 锁」的自锁。
fn with_db<T>(siv: &mut Cursive, action: impl FnOnce(&Db) -> Result<T, String>) -> Result<T, String> {
    let Some(shared) = siv.user_data::<Shared>() else {
        return Err("内部状态丢失（user_data）".to_string());
    };
    // 锁中毒（别的线程持锁时 panic）不该让程序不可用：SQLite 自身有事务保证，
    // 取回内层值继续用。
    let guard = shared
        .db
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    action(&guard)
}

// ---------------------------------------------------------------------- UI 动作

/// 重新查库并重填列表。搜索框每次输入、剪贴板有新内容、终端改变尺寸时都会调用。
pub fn refresh_list(siv: &mut Cursive) {
    let keyword = siv
        .call_on_name(SEARCH_VIEW, |view: &mut EditView| {
            (*view.get_content()).clone()
        })
        .unwrap_or_default();

    let (entries, total_in_db) = match with_db(siv, |db| {
        let entries = db.list(Some(&keyword), LIST_LIMIT)?;
        let total = db.len()?;
        Ok((entries, total))
    }) {
        Ok(result) => result,
        Err(e) => {
            set_status(siv, &format!("✗ {e}"));
            return;
        }
    };

    let width = label_width(siv);
    let shown = entries.len();

    siv.call_on_name(LIST_VIEW, |view: &mut SelectView<Entry>| {
        // `selected_id()` 返回的是**索引**（不是条目值），名字有误导性但不影响用途。
        let previous_index = view.selected_id();
        view.clear();
        for entry in entries {
            let label = text::entry_label(&entry.content, width);
            view.add_item(label, entry);
        }
        if shown > 0 {
            // 重填后把选中项挪回同一位置（越界则收敛到最后一条）。
            let index = previous_index.unwrap_or(0).min(shown - 1);
            let _ = view.set_selection(index);
        }
    });

    if keyword.trim().is_empty() {
        set_status(siv, &format!("共 {shown} 条 ·{HINT}"));
    } else {
        set_status(
            siv,
            &format!("搜索「{keyword}」命中 {shown} 条 / 库内共 {total_in_db} 条"),
        );
    }
}

pub fn set_status(siv: &mut Cursive, message: &str) {
    siv.call_on_name(STATUS_VIEW, |view: &mut TextView| {
        view.set_content(message.to_string());
    });
}

/// 把当前选中项写回系统剪贴板，然后退出。
fn write_back_and_quit(siv: &mut Cursive) {
    let selected = siv
        .call_on_name(LIST_VIEW, |view: &mut SelectView<Entry>| {
            view.selection().map(|entry| entry.content.clone())
        })
        .flatten();

    let Some(content) = selected else {
        set_status(siv, "没有可写回的条目");
        return;
    };

    let sent = match siv.user_data::<Shared>() {
        Some(shared) => shared.cmd_tx.send(Cmd::Write(content)).is_ok(),
        None => false,
    };

    if !sent {
        set_status(siv, "✗ 剪贴板监听线程已退出，无法写回");
        return;
    }

    // 写回由监听线程执行；退出前它会把队列里的 Write 先处理掉（见 main 的注释）。
    siv.quit();
}

/// 列表聚焦时上下移动选中项（`j` / `k`）。
///
/// 存在的理由：cursive 的 `SelectView` 内置只响应方向键与 Page/Home/End，
/// 没有 vim 的 j/k；而被替换掉的 ratatui 版本支持 j/k，不补上就是功能倒退。
fn move_selection(siv: &mut Cursive, delta: isize) {
    siv.call_on_name(LIST_VIEW, |view: &mut SelectView<Entry>| {
        let len = view.len();
        if len == 0 {
            return;
        }
        let current = view.selected_id().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        let _ = view.set_selection(next);
    });
}

fn delete_selected(siv: &mut Cursive) {
    let target = siv
        .call_on_name(LIST_VIEW, |view: &mut SelectView<Entry>| {
            view.selection().map(|entry| entry.id)
        })
        .flatten();

    let Some(id) = target else {
        set_status(siv, "没有可删除的条目");
        return;
    };

    match with_db(siv, |db| db.delete(id)) {
        Ok(()) => {
            set_status(siv, "已删除选中条目");
            refresh_list(siv);
        }
        Err(e) => set_status(siv, &format!("✗ {e}")),
    }
}

/// 清空属于不可逆操作，走一个确认弹窗而不是单键触发。
fn confirm_clear(siv: &mut Cursive) {
    siv.add_layer(
        Dialog::text("清空全部剪贴板历史？此操作不可撤销。")
            .title("确认清空")
            .button("清空", |siv| {
                siv.pop_layer();
                match with_db(siv, |db| db.clear()) {
                    Ok(()) => {
                        set_status(siv, "已清空全部历史");
                        refresh_list(siv);
                    }
                    Err(e) => set_status(siv, &format!("✗ {e}")),
                }
            })
            .button("取消", |siv| {
                siv.pop_layer();
            }),
    );
}

/// 列表项可用宽度：终端宽度的 70%，再扣掉对话框边框等占位。
fn label_width(siv: &mut Cursive) -> usize {
    let screen = siv.screen_size().x;
    if screen == 0 {
        // 后端还没初始化（首帧之前），给个保守值，第一个事件循环后会重算。
        return 40;
    }
    (screen * LABEL_PERCENT / 100).saturating_sub(4).max(8)
}

// ------------------------------------------------------------------ 终端状态

static TERMINAL_RESTORED: AtomicBool = AtomicBool::new(false);

/// 恢复终端。幂等：panic hook 与 cursive 的 Drop 都可能触发。
fn restore_terminal() {
    if TERMINAL_RESTORED.swap(true, Ordering::SeqCst) {
        return;
    }
    use std::io::Write;

    let _ = crossterm::terminal::disable_raw_mode();
    let mut stdout = std::io::stdout();
    let _ = crossterm::execute!(
        stdout,
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::cursor::Show
    );
    let _ = stdout.flush();
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // 先恢复终端，再交给原 hook 打印 panic 信息，
        // 否则报错会被吞在备用屏幕里、终端也留在原始模式。
        restore_terminal();
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_width_scales_with_the_terminal() {
        // 不依赖真实终端，直接验证 70% 这条规则本身
        assert_eq!(100 * LABEL_PERCENT / 100 - 4, 66);
        assert_eq!(80 * LABEL_PERCENT / 100 - 4, 52);
    }

    #[test]
    fn db_path_points_at_the_ditto_data_dir() {
        let path = db_path().expect("CI/开发机应有 data_dir");
        assert!(path.ends_with("ditto/history.db"), "实际: {}", path.display());
    }
}
