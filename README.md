# Ditto 📋

终端剪贴板历史管理器：后台线程每 500ms 轮询系统剪贴板，用 [cursive](https://github.com/gyscos/cursive)
渲染界面，历史存进 SQLite，`Enter` 把选中内容写回剪贴板。

<!-- ditto-screenshot:start 由 `python3 tools/capture_screenshot.py --write` 生成，请勿手改 -->
```

 ┌─────────────────────────────────────┤ Ditto — 剪贴板历史 ├─────────────────────────────────────┐
 │ ______________________________________________________________________________________________ │
 │ https://example.com/a/very/long/path/that/keeps/going/...  (67 字)                             │
 │ 今天下午的会议纪要：讨论了三个方案，最终决定先做交互原...  (37 字)                             │
 │ curl -H 'Authorization: Bearer eyJhbGciOiJIUzI1NiIsIn...  (101 字)                             │
 │                                                                                                │
 │                                                                                                │
 │                                                                                                │
 │ 共 3 条 · Enter 写回并退出 · Esc 退出 · Tab 切换焦点 · 列表聚焦时 j/k 选择、d 删除、D 清空     │
 └────────────────────────────────────────────────────────────────────────────────────────────────┘

```
<!-- ditto-screenshot:end -->

## 安装

```bash
cargo install --path .   # 项目内置别名：cargo inst
dtto                     # 装完任意目录可用
```

改完代码重跑一次上面的命令覆盖安装。只想临时跑：`cargo run`。

> **命令为什么叫 `dtto` 而不是 `ditto`**：macOS 自带 `/usr/bin/ditto`（文件拷贝工具）。
> 装成 `ditto` 会二选一出问题：`~/.cargo/bin` 排在 `/usr/bin` 之后时（本机是位置 21 vs 12），
> 敲 `ditto` 实际执行的是系统工具；排在之前时（如 Homebrew 的 bin 目录）则会盖掉系统命令。
> 二进制名因此用 `dtto`，而 crate 名 / 产品名 / 数据目录仍是 `ditto`。

macOS 与 Linux（X11 以及 **Wayland 原生**，通过 `arboard` 的 `wayland-data-control`）。
edition 2021，实测 cargo 1.97。

## 使用

| 键 | 行为 |
|---|---|
| 直接打字 | 在顶部搜索框输入关键字，`LIKE %关键字%` 实时过滤列表 |
| `Tab` | 在搜索框与历史列表之间切换焦点 |
| `↑` / `↓` / `j` / `k` | 列表聚焦时上下选择（cursive 默认只认方向键，`j`/`k` 是这里补的） |
| `Enter` | 把选中内容写回系统剪贴板，然后退出 |
| `d` | 删除选中条目（列表聚焦时） |
| `D` | 清空全部历史（弹窗二次确认） |
| `Esc` / `Ctrl+C` | 退出 |

- **MRU 去重**：重复内容不新增行，而是刷新时间戳把它提到最前；上限 500 条，超出裁掉最旧。
- **持久化**：`<data_dir>/ditto/history.db`
  （macOS 在 `~/Library/Application Support/`，Linux 在 `~/.local/share/`）。
  开启 WAL，写库用 `ON CONFLICT(content) DO UPDATE` 的真 upsert。
- **敏感内容默认不入库**，命中时状态栏显示规则名。`DITTO_NO_FILTER=1 dtto` 可整体关闭。
  规则：私钥块、JWT、AWS `AKIA/ASIA`、GitHub `ghp_*`、Slack `xox*-`、OpenAI `sk-*`，
  以及长度 ≥48 且大小写与数字混排的高熵串（长 URL/路径已豁免，避免误伤）。
- **列表项截断**：按终端宽度的 70% 截断，用 `unicode-width` 按**显示列数**算（CJK 占 2 列），
  超长补 `...`；空间不够时优先保住正文，宁可丢掉右边的 `(N 字)`。

## 原理

### 两条线程，一条数据库连接

UI 线程（cursive 事件循环、所有查询）与后台轮询线程（每 500ms 读剪贴板、写库）之间，
**只有一条 `rusqlite::Connection`，由 `Arc<Mutex<Db>>` 串行化**，而不是每个线程各开一条：

- SQLite 本身是单写者模型，多连接并发写只会换来 `SQLITE_BUSY` 重试，不会真的并行；
- 每次操作只是一行文本的读写，持锁时间是微秒级，UI 感觉不到；
- 即便如此仍开了 WAL 并设了 `busy_timeout`，作为将来真引入第二条连接时的保险。

配套约束：**持锁期间绝不调用 cursive**，所有 SQL 调用统一走 `with_db()`，先把结果读出来、
锁释放，再去动 UI —— 否则会形成「UI 持 DB 锁 → 触发回调 → 回调里再要 DB 锁」的自锁。

### `cb_sink` 是怎么被唤醒的（实测源码，别被文档误导）

cursive 文档只说 *"Callbacks will be executed in the order of arrival on the next event
cycle."*，看起来像"要等下一次用户输入"。实测 cursive 0.21.1 / cursive_core 0.4.7：

1. `CbSink` 是 `crossbeam_channel::unbounded()` 的 Sender，`send` 永不阻塞；
2. 事件循环每轮用 `try_recv()` 在 `while` 里一次性排空所有待处理回调；
3. 循环**不是**阻塞在 read 上：crossterm 后端的 `poll_event()` 是 `poll(1ms)`，空闲返回
   `None`，此时只 `sleep(30ms)` 就进入下一轮；
4. 处理过回调的那一帧会被视为 "non-boring"，于是直接走 `refresh()`。

所以**不需要 `set_fps`**：循环本身就在 ~33Hz 空转，后台线程发出的刷新请求约 30ms 内就被
执行并重绘。`fps` 只决定"没有事件时要不要定时重绘"（时钟/动画那类需求）。

### 剪贴板实例单一所有权

`arboard::Clipboard` 只由轮询线程持有。UI 按 `Enter` 时发 `Cmd::Write` 命令过去，由轮询
线程写回并同步 `last_seen`。这样"自己写回的内容"不会被下一轮轮询当成"外部新复制"；
也避免 Linux/X11 下双实例争抢剪贴板所有权（X11 需要进程存活才能应答粘贴请求）。

退出顺序依赖同一件事：写回是异步的，而命令按 FIFO 排队，所以 `Shutdown` 之前排队的
`Write` 一定先被执行，写回不会因为退出而丢失。

### 终端状态兜底

cursive 自己没有 panic hook，只有 crossterm 后端的 `Drop` 会恢复终端（而那里是
`disable_raw_mode().unwrap()`，panic 展开途中再失败就是 double panic 直接 abort）。
所以这里自己装了一个：先恢复终端再打印 panic 信息，幂等。终端初始化失败时用
`try_run()` 而不是 `run()`（后者内部是 `unwrap()`），把错误打成人看得懂的提示。

## 开发

```bash
cargo test                                    # 单测
cargo clippy --all-targets -- -D warnings
cargo inst                                    # 重新装到 PATH
python3 tools/capture_screenshot.py --write   # 重新生成 README 截图
```

| 文件 | 职责 |
|---|---|
| `src/main.rs` | cursive 界面、按键绑定、`with_db()`、终端恢复与 panic hook |
| `src/db.rs` | SQLite 层：建表、upsert、模糊查询、删除/清空、裁剪 |
| `src/clipboard.rs` | 轮询线程、`Cmd` 协议、`cb_sink` 通知 |
| `src/filter.rs` | 敏感内容规则 |
| `src/text.rs` | 按显示宽度折叠换行与截断（CJK 安全） |

单测覆盖 upsert 保持 id 不变、同毫秒内插入仍严格有序、`LIKE` 通配符转义、
关键词模糊匹配、裁剪到上限、数据库重开后数据仍在、CJK 宽度截断与窄宽度降级。

`tools/capture_screenshot.py` 在真实 pty 里跑一遍程序，把 ANSI 流转回屏幕文本：
TUI 的输出是转义流，而且 cursive 是**差分渲染**（只重画变化的格子），所以字节流里
不会出现完整的「共 3 条」这类字符串 —— **靠 grep 字符串验证界面是错的**，必须按光标
定位把字符填回网格。脚本还要注意截掉退出阶段的擦屏序列，否则还原出来是空白。

## 已知限制

- 仅处理纯文本，不处理图片与文件。
- `Enter` 写回后直接退出，因此**不会**把该条提升到顶部（下次启动顺序不变）。
- 顶部搜索框那一行下划线是 cursive `EditView` 的默认 filler，不是渲染问题。
- 终端过窄时状态栏（快捷键提示）会被裁切。
- `kill -9` 不会执行 `Drop`，终端会残留原始模式；在该终端执行一次 `reset` 即可恢复。
  这是终端程序的固有限制，进程内无法解决。
