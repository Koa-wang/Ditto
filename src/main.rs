//! Ditto —— 终端剪贴板历史管理器。
//!
//! 结构：一个后台监听线程负责剪贴板轮询与写回，主线程只做 UI 与状态机，
//! 两者用 `std::sync::mpsc` 通道通信（见 `clipboard.rs`）。

mod app;
mod clipboard;
mod filter;
mod history;
mod storage;
mod ui;

use std::io::{self, BufWriter, IsTerminal, Stdout};
use std::panic;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::{Action, App};
use clipboard::Monitor;
use history::History;
use storage::LoadError;

/// UI 心跳：50ms 一次。够快以保证按键跟手，又不会烧 CPU。
const TICK: Duration = Duration::from_millis(50);

type Backend = CrosstermBackend<BufWriter<Stdout>>;

/// 终端恢复只允许生效一次：panic hook 与 `Drop` 可能都会触发，
/// 重复执行虽然无害，但统一走一个幂等入口更不容易出岔子。
static TERMINAL_RESTORED: AtomicBool = AtomicBool::new(false);

fn restore_terminal() {
    if TERMINAL_RESTORED.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = execute!(stdout, LeaveAlternateScreen, Show);
}

/// RAII 守卫：无论正常返回还是 `?` 提前退出，都走同一条恢复路径。
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, Hide)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// panic 时先恢复终端，再交给原 hook 打印 panic 信息，
/// 否则报错信息会被吞进备用屏幕，终端也会留在原始模式里。
fn install_panic_hook() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
}

fn run(terminal: &mut Terminal<Backend>, app: &mut App, monitor: &Monitor) -> io::Result<()> {
    while !app.should_quit {
        terminal.draw(|frame| ui::draw(frame, app))?;

        // 50ms 心跳：避免空转，同时让按键响应保持在人感觉不到延迟的量级。
        if event::poll(TICK)? {
            if let Event::Key(key) = event::read()? {
                // 只处理按下事件，否则 Windows/Crossterm 会同时送来 Release 造成重复触发。
                if key.kind == KeyEventKind::Press {
                    match app.on_key(key) {
                        Action::Quit => app.should_quit = true,
                        Action::Write(text) => {
                            if !monitor.write(text) {
                                app.alert = Some("剪贴板监听线程已退出，无法写回".to_string());
                            }
                        }
                        Action::None => {}
                    }
                }
            }
        }

        // 非阻塞排空后台事件。这是「UI 不卡」与「剪贴板实时」能并存的关键：
        // 键盘读取带超时，剪贴板事件永远只是 try_recv，谁都不会等谁。
        while let Ok(event) = monitor.events.try_recv() {
            app.on_event(event);
        }

        app.tick();
    }
    Ok(())
}

fn main() -> ExitCode {
    if !io::stdout().is_terminal() {
        eprintln!("ditto: 需要在真实终端中运行（标准输出不是 TTY）");
        return ExitCode::FAILURE;
    }

    let storage_path = storage::default_path();
    let (history, startup_issue) = match storage_path.as_deref() {
        Some(path) => match storage::load(path) {
            Ok(items) => (History::from_texts(items), None),
            // 首次运行没有历史文件是正常情况，不该打扰用户。
            Err(LoadError::NotFound) => (History::new(), None),
            Err(e) => (History::new(), Some(e.message())),
        },
        None => (
            History::new(),
            Some("无法确定数据目录，本次运行不会保存历史".to_string()),
        ),
    };

    let mut app = App::new(history, storage_path);
    app.alert = startup_issue;

    // hook 必须在进入原始模式之前装好，才能在后续任何一步失败时兜住终端。
    install_panic_hook();

    let guard = match TerminalGuard::enter() {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("ditto: 无法进入原始模式: {e}");
            return ExitCode::FAILURE;
        }
    };

    let monitor = Monitor::spawn();
    let mut terminal = match Terminal::new(Backend::new(BufWriter::new(io::stdout()))) {
        Ok(terminal) => terminal,
        Err(e) => {
            eprintln!("ditto: 初始化终端失败: {e}");
            return ExitCode::FAILURE;
        }
    };

    let result = run(&mut terminal, &mut app, &monitor);

    // 退出顺序很讲究：先停后台线程，再 flush 终端，最后恢复屏幕，
    // 这样任何在恢复之后打印的信息都落在正常屏幕上。
    monitor.shutdown();
    drop(terminal);
    drop(guard);

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ditto: 运行出错: {e}");
            ExitCode::FAILURE
        }
    }
}
