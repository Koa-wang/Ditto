# Ditto（Cursive + SQLite 版）设计文档

- 日期：2026-09-12
- 状态：已实现并验证
- 取代：`2026-09-11-ditto-design.md`（ratatui + JSON 版，已归档到 tag `ratatui-final`）
- 目标：换用 `cursive` 组件化 UI 与 `rusqlite` 本地持久化，重写为 SQLite 版剪贴板管理器

## 1. 背景与取舍

原实现（ratatui + `serde_json`）功能完整、测试齐备，但持久化是「整个历史重写一个 JSON 文件」，
查询只能全量读进内存再过滤。本次按用户决定**整体替换**为 cursive + SQLite：

- 保留：敏感内容过滤、删除/清空、条数上限（这三项与 UI/存储实现无关，属产品能力）。
- 放弃：`history.json` 的数据迁移（本机数据目录为空，无实际数据可迁）。
- 归档：旧实现留在 git 历史与 tag `ratatui-final`，`git checkout ratatui-final -- src/` 可取回。

## 2. 技术选型（版本为实测解析结果）

| 关注点 | 选择 | 说明 |
|---|---|---|
| TUI | `cursive 0.21.1` | `crossterm-backend` 是默认 feature，`ncurses-backend` 默认关闭，无需 `default-features = false`，也不需要系统装 ncurses |
| 存储 | `rusqlite 0.40.2`（`bundled`） | `bundled` 把 sqlite3 C 源码编进去，用户端不需要 sqlite3 开发库；代价是编译变慢 |
| 剪贴板 | `arboard 3.6.1` | Linux 追加 `wayland-data-control`（此前 README 声称支持但依赖没接上，本次修正） |
| 终端恢复 | `crossterm 0.28.1`（直接依赖） | 只为 panic hook；版本必须与 cursive 用的一致，否则会编出两份 crossterm |
| 宽度 | `unicode-width 0.2.2` | CJK 占 2 列 |
| 目录 | `dirs 7.0.0` | |

## 3. 数据层

```sql
CREATE TABLE IF NOT EXISTS clipboard_history (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    content    TEXT    NOT NULL UNIQUE,
    created_at INTEGER NOT NULL          -- Unix 毫秒
);
CREATE INDEX IF NOT EXISTS idx_history_created_at ON clipboard_history (created_at DESC);
```

- `CAP = 500`，插入后裁剪。
- **不用 `INSERT OR REPLACE`**：SQLite 的 REPLACE 语义是「先 DELETE 旧行再 INSERT 新行」，
  会换掉 `id`、`rowid`，外键也会断。改用真 upsert：
  `INSERT ... ON CONFLICT(content) DO UPDATE SET created_at = excluded.created_at`。
- **时间戳单调递增**：取 `max(now_ms, MAX(created_at)+1)`。直接用 `now_ms` 会让「同一毫秒内
  连续插入」产生相同时间戳，MRU 顺序不确定、测试也不稳定。
- 搜索 `LIKE %关键词% ESCAPE '\'`，关键词里的 `%` `_` `\` 必须转义，否则用户打一个 `%`
  就变成「匹配全部」，看起来像搜索失灵。
- 连接设置：`journal_mode = WAL` + `busy_timeout = 2s`。

### 并发策略

整个进程**只有一条 `Connection`**，由 `Arc<Mutex<Db>>` 串行化（UI 线程与轮询线程共用）。

- SQLite 是单写者模型，多连接并发写只会换来 `SQLITE_BUSY`，不会真并行；
- 每次操作只是一行文本的读写，持锁微秒级；
- 连接池 / 专用 DB 线程在这个量级属过度设计（YAGNI）。

配套硬约束：**持锁期间绝不调用 cursive**。所有 SQL 走统一入口 `with_db()`，
先读结果、释放锁，再更新视图；否则会形成「UI 持 DB 锁 → 触发 cursive 回调 →
回调里再要 DB 锁」的自锁路径。锁中毒（别的线程持锁时 panic）用 `into_inner()` 恢复，
不让轮询线程静默死掉。

## 4. 线程模型与 `cb_sink` 唤醒机制

```
主线程（cursive 事件循环 + 所有查询）        轮询线程（独占 arboard::Clipboard）
  on_edit / d / D ──with_db()──▶ SQLite ◀──upsert── 每 500ms 读剪贴板
  ◀── cb_sink 闭包（刷新列表） ────────────────┘
  ── Cmd::Write(text) ─────────────────────▶ set_text + 同步 last_seen
```

**`cb_sink` 不需要 `set_fps`**（实测 cursive 0.21.1 / cursive_core 0.4.7）：

1. `CbSink = Sender<Box<dyn FnOnce(&mut Cursive) + Send>>`，来自
   `crossbeam_channel::unbounded()`，`send` 永不阻塞（`cursive_root.rs:75,90`）。
2. 事件循环每轮调用 `process_callback()`，用 `try_recv()` 在 `while` 里排空所有回调
   （`cursive_root.rs:831-839`）。
3. 循环不阻塞在 read 上：crossterm 后端 `poll_event()` = `poll(1ms)`，空闲返回 `None`
   （`backends/crossterm.rs:335`）；空闲时 `post_events` 只 `sleep(30ms)`（`cursive_run.rs:176`）。
4. 处理过回调的那一帧 `boring = false`，`post_events` 直接走 `refresh()`（`cursive_run.rs:151-168`）。

结论：循环本身约 33Hz 空转，后台刷新请求约 30ms 内生效。`fps` 只影响「无事件时的定时重绘」。

**剪贴板单一所有权**：`arboard::Clipboard` 只由轮询线程持有，UI 的写回走 `Cmd::Write`，
由轮询线程 `set_text` 并同步 `last_seen`。好处：自己写回的内容不会被误判为外部新复制；
避免 X11 下双实例争抢剪贴板所有权。退出顺序依赖 FIFO：`Shutdown` 之前排队的 `Write` 先执行，
写回不会丢。

**`last_seen` 顺序**：刷新必须发生在敏感过滤判断之前，否则同一段敏感文本每 500ms 弹一次提示。

## 5. UI

- `LinearLayout::vertical()`：搜索框（`EditView`，高 1 行）→ 列表（`SelectView<Entry>`，
  占满剩余高度）→ 状态栏（`TextView`，高 1 行），整体套一个 `Dialog::full_screen()`。
- 搜索：`EditView::on_edit` → 重新查库并重填列表（实时过滤）。
- `Enter`：`SelectView::on_submit` 与 `EditView::on_submit` **都绑同一个动作**
  （写回 + 退出）。焦点在搜索框时事件不会传给兄弟节点，同一次按键只会命中一个，不会重复触发。
- `d` / `D` / `j` / `k` 包在 `OnEventView` 里，只在**列表聚焦时**生效——做成全局回调会把
  搜索框里的字母抢走。`j`/`k` 需要自己补：cursive 的 `SelectView` 只认方向键与
  Page/Home/End（`select_view.rs:741-746`）。
- `Esc` 退出，`Ctrl+C` 退出，`Tab` 切焦点（`LinearLayout` 内置），`WindowResize` 时重算截断。
- 列表项截断按终端宽度的 70%，用 `unicode-width` 按显示列数算；空间不足时先丢 `(N 字)` 后缀，
  保住正文。

## 6. 终端状态

- 自己装 panic hook：先恢复终端（`disable_raw_mode` + `LeaveAlternateScreen` + 显示光标，
  用 `AtomicBool` 保证幂等）再委托原 hook，保证 panic 信息落在正常屏幕上。
  cursive 自身没有 panic hook，只有 crossterm 后端的 `Drop` 会恢复，而那里是
  `disable_raw_mode().unwrap()`——panic 展开途中再失败就是 double panic 直接 abort。
- 用 `try_run()` 而非 `run()`（后者内部 `unwrap()`）。**实测**：没有控制终端时 crossterm 的
  `tty_fd()` 会以 `ENXIO` 失败并 panic，`try_run` 能把它变成可读的错误提示。

## 7. 测试与验证

- 单测：upsert 保持 `id` 不变、同毫秒插入仍严格有序、`LIKE` 通配符转义、模糊匹配、
  裁剪上限、库重开后数据仍在、WAL 生效、CJK 宽度截断与窄宽度降级、`react` 的过滤只提示一次。
- 端到端（pty，见 `tools/capture_screenshot.py`）：真实终端里跑一遍，验证落库顺序、
  搜索过滤命中数、`Enter` 写回选中项、退出码。
- 两个必须记住的坑：pty 必须成为子进程的**控制终端**（`setsid` + `TIOCSCTTY`），否则
  crossterm 报 `ENXIO`；cursive 是**差分渲染**，字节流里没有完整字符串，
  **不能靠 grep 验证界面**，必须按光标定位重建屏幕网格，并且要截掉退出阶段的擦屏序列。

## 8. 明确不做（YAGNI）

- 图片 / 富文本剪贴板、多选、标签、云同步、配置文件的主题与键位自定义。
- 连接池 / 专用 DB 线程 / 异步运行时。
- `history.json` → SQLite 迁移。

## 9. 已知限制

- `Enter` 写回后直接退出，不会把该条提升到顶部（下次启动顺序不变）。
- 顶部搜索框那行下划线是 cursive `EditView` 的默认 filler，不是渲染问题。
- 终端过窄时状态栏会被裁切。
- `kill -9` 不会执行 `Drop`，终端残留原始模式，需 `reset`（进程内无法解决）。
