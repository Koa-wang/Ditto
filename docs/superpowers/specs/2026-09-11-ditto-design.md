# Ditto — 终端剪贴板历史管理器 设计文档

- 日期：2026-09-11
- 状态：已通过设计评审，待写实现计划
- 目标：Rust + Ratatui 的终端剪贴板历史管理器，`cargo run` 可直接运行

## 1. 目标与非目标

### 目标

1. 后台线程每 500ms 轮询系统剪贴板，抓取纯文本，过滤空文本、去重、上限 100 条。
2. MRU 语义：重复内容提升到列表顶部。
3. 历史持久化到磁盘，跨重启保留。
4. TUI：标题栏 + 列表 + 底部快捷键/状态栏，支持选择、写回剪贴板、删除、清空。
5. 终端 Raw Mode 与 panic 场景下都能安全恢复终端状态。

### 非目标（YAGNI，明确不做）

- 图片/文件/富文本剪贴板（仅纯文本）。
- 搜索/过滤输入框、鼠标操作、多选、标签分类。
- 剪贴板同步、多机共享、加密存储。
- 配置文件（唯一可调项是环境变量 `DITTO_NO_FILTER`）。
- `~/.ditto` 之类的自定义路径（使用系统 data dir）。

## 2. 技术选型

| 关注点 | 选择 |
|---|---|
| TUI 渲染 | `ratatui` |
| 终端底层/事件 | `crossterm`（若 ratatui 已 re-export 则改用 `ratatui::crossterm`，避免两份版本） |
| 剪贴板 | `arboard`（Linux 追加 `wayland-data-control` feature） |
| 并发 | `std::thread` + `std::sync::mpsc`，**不使用 tokio** |
| 序列化 | `serde` + `serde_json` |
| 目录 | `dirs` |
| 显示宽度 | `unicode-width`（CJK/emoji 截断对齐） |

依赖版本在实现时用 `cargo add` 取当前可解析的最新版，并以 `cargo tree -d` 确认 crossterm 未被解析出多份版本。

## 3. 事件循环方案（已选定方案①）

选中 **std::thread + mpsc + `poll(50ms)`**：

- 主线程 `event::poll(Duration::from_millis(50))`；为真则 `event::read()` 处理键盘/Resize。
- 随后 `try_recv()` 排空剪贴板通道与监听线程状态通道（非阻塞，因此剪贴板更新永远不会被按键读取卡住）。
- 50ms 心跳（约 20 次/秒唤醒）同时承担：状态消息 TTL 过期、后续可能的定时需求。

被否决的方案：

- **tokio + `EventStream` + `select!`**：arboard 是同步 API，仍需通道桥接，异步收益不抵额外依赖与复杂度。
- **独立事件读取线程 + 主线程 `recv()` 阻塞**：空闲零唤醒，但主线程被阻塞，无法承载状态消息过期与 resize 之外的定时需求，且线程数更多。

## 4. 线程模型与数据流

```
┌─ 主线程 (UI) ─────────────────────┐        ┌─ clipboard 监听线程 ───────────┐
│ EventLoop:                        │        │ 独占 arboard::Clipboard         │
│   poll(50ms) → read() 键/Resize   │        │ loop {                         │
│   try_recv() ← AppEvent           │        │   recv_timeout(500ms) 处理命令  │
│   try_recv() ← MonitorStatus      │        │   read 文本 → 空过滤 → 敏感过滤  │
│   draw()                          │        │   != last_seen 则上报并刷新      │
│   ── MonitorCmd::Write(text) ─────┼───────▶│   set_text 并刷新 last_seen }   │
└───────────────────────────────────┘        └────────────────────────────────┘
```

### 4.1 单一所有权原则

`arboard::Clipboard` **只由监听线程持有**，主线程从不直接调用剪贴板 API：

- 「写回剪贴板」是主线程发 `MonitorCmd::Write(text)` 请求，由监听线程在同一实例上 `set_text`。
- 好处一：写回与轮询共用同一个 `last_seen`，天然消除「自己写回的内容又被当成外部新复制」的竞态。
- 好处二：避免 Linux/X11 下双实例争抢剪贴板所有权（X11 需要进程存活以应答粘贴请求，实例必须活到程序结束）。

### 4.2 通道协议

```rust
// 监听线程 → 主线程
enum AppEvent {
    ClipboardChanged(String),      // 外部新复制的文本（已通过空/敏感过滤）
    Filtered(&'static str),        // 命中敏感规则的规则名，仅用于状态提示
    WriteDone(Result<(), String>), // 写回结果，失败时携带错误信息
    MonitorError(String),          // 剪贴板不可用（初始化失败），仅发送一次
}

// 主线程 → 监听线程
enum MonitorCmd {
    Write(String),                 // 写回系统剪贴板
    Shutdown,                      // 立即退出（用于秒级 join，不必等 500ms 超时）
}
```

### 4.3 节奏与退出

- 监听线程用 `recv_timeout(500ms)` **兼作**命令处理与轮询节拍：期间到达的 `Write` 会立即执行，不会被打断睡眠。
- 收到 `Shutdown` 或通道 `Disconnected` → 退出循环，主线程 `join` 后返回，无悬挂线程。
- 剪贴板初始化失败（无显示服务器等）→ 发送 `MonitorError`；UI 正常启动但状态栏持久提示"剪贴板不可用"，不 panic。

### 4.4 `last_seen` 规则

- 每次读取后与 `last_seen` 比较：相同 → 跳过；不同 → 刷新 `last_seen`，再决定上报什么。
- **刷新 `last_seen` 先于过滤判断**：命中敏感规则时同样刷新，否则同一段敏感文本会每 500ms 重复上报 `Filtered`，把状态栏刷屏。
- `Write` 成功后同样刷新 `last_seen`，因此 `Enter` 写回不会产生新事件。
- 外部重新复制一个已在历史中的条目 → 视为事件 → 触发 MRU 提升。
- 读取返回 `Err`（例如 macOS 上其他应用正持有剪贴板）→ 本轮跳过，不刷新 `last_seen`，不算错误。

### 4.5 过滤与落盘的责任边界

- **空文本过滤、敏感过滤都只发生在监听线程**（第 4 节数据流的那一步），`History` 因此不需要「拒绝」分支。
- **落盘由主线程负责**：主线程在每次成功改变 `History` 之后（插入/提升/删除/清空）调用 `storage::save`，失败只进状态栏、不中断运行。

## 5. 数据结构与业务规则

```rust
struct ClipEntry { id: u64, text: String }
struct History { items: Vec<ClipEntry>, next_id: u64 }

const CAP: usize = 100;

// 入参文本已由监听线程完成空/敏感过滤与 trim 检查，此处只处理去重与容量
enum Outcome { Inserted(u64), Promoted(u64) }

impl History {
    fn push_or_promote(&mut self, text: String) -> Outcome;
    fn remove(&mut self, id: u64) -> bool;
    fn clear(&mut self);
    fn index_of(&self, id: u64) -> Option<usize>;
}
```

- **空文本过滤**：`text.trim().is_empty()` → 忽略（不发事件），在监听线程完成。
- **敏感过滤**：命中 `filter::is_sensitive` 时**不写入历史**，改发 `Filtered(规则名)`，同样在监听线程完成。
- **去重 / MRU**：文本完全相等即视为同一条目。已存在 → 移到 index 0，返回 `Promoted(id)`。
- **插入**：插入 index 0；若 `items.len() > CAP` 则 `pop()` 尾部淘汰最旧。
- **选中项稳定性**：`selected_id: Option<u64>`。MRU 重排后按 `id` 重新计算列表索引，不用索引加减（提升只会让目标变 0、其余 +1，但 id 方案对将来的重排更稳）。
- **Enter 写回**：发送 `Write`，成功后把该条提升到顶部（算一次"使用"），**不新增重复条目**。
- **删除 `d`**：删除选中项；选中移到原索引位置的下一条，若删除的是最后一条则选中新的最后一条；历史为空则 `selected_id = None`。
- **清空 `D`**：进入 `ConfirmClear` 状态，需再按 `y` 生效，其他任意键取消。

## 6. 持久化

- 路径：`dirs::data_dir().join("ditto").join("history.json")`
  - macOS：`~/Library/Application Support/ditto/history.json`
  - Linux：`~/.local/share/ditto/history.json`
- 格式（version 字段用于将来演进）：

```json
{ "version": 1, "items": ["最近使用的在最前", "..."] }
```

- **原子写**：写 `<path>.tmp` → `rename` 覆盖目标，避免半截文件。
- **权限**：写完后设 `0600`（剪贴板历史含敏感内容的概率高）。POSIX 特化用 `#[cfg(unix)]`。
- **写入时机**：每次历史变更后写一次（≤100 条小文本，单次约几 KB）；删除/清空后**立即**写，不等退出。调用点集中在主线程（见 4.5）。
- **容错**：文件缺失 / JSON 损坏 / version 不认识 / 权限不足 → 一律以空历史启动，并在状态栏提示原因，绝不 panic。
- **读取顺序**：文件顺序即 MRU 顺序，`next_id` 顺序分配。

## 7. 敏感内容过滤

`src/filter.rs` 导出 `is_sensitive(text: &str) -> Option<&'static str>`，返回命中的规则名。每条规则独立函数、独立测试。

| # | 规则名 | 匹配条件 |
|---|---|---|
| 1 | `private-key` | 含 `-----BEGIN ... PRIVATE KEY-----` |
| 2 | `jwt` | 两段 `.` 分隔、首段以 `eyJ` 开头、总长 > 40 |
| 3 | `aws-access-key` | `AKIA` / `ASIA` 后接 16 位 `[0-9A-Z]` |
| 4 | `github-token` | `gh[pousr]_` 后接 ≥36 位 |
| 5 | `slack-token` | `xox[baprs]-` 开头 |
| 6 | `openai-key` | `sk-` 后接 ≥20 位 |
| 7 | `high-entropy` | 长度 ≥48、无空白、且**同时含大写、小写、数字**；URL/路径额外豁免 |

- 规则 7 是唯一可能误伤的规则，阈值故意取高：
  - 不采用「四类字符占三类（大写/小写/数字/符号）」的写法。长 URL 天然满足大小写+数字+符号，那样会把用户复制的长链接吃掉。改成必须同时具备大小写字母与数字——这才是 base64 凭据的典型特征。
  - 再加一层豁免：含 `://`、以 `www.` / `/` / `~` 开头的文本一律放行。
  - 反例覆盖：正常中文散文、长 URL、多行代码片段、长小写 hex 串、短 token 均不得命中（`filter` 单测保证）。
- 整体关闭：环境变量 `DITTO_NO_FILTER=1`（值为 `1` 时跳过全部规则）。
- 命中只影响「是否入历史」，不阻塞剪贴板本身。

## 8. 界面与交互

### 布局

```
┌ 📋 Ditto — 剪贴板历史  (12/100) ──────────────────────┐
│ ▸ 1  curl -H 'Authorization: Bearer eyJ...'           │   ← 标题栏 1 行
│   2  今天下午的会议纪要：讨论了三个方案……       (86 字) │   ← 列表填满剩余
│   3  https://example.com/a/very/long/url/that...      │
├───────────────────────────────────────────────────────┤
│ j/k ↑/↓ 选择 · Enter 写回 · d 删除 · D 清空 · q/Esc 退出 │   ← 快捷键提示 1 行
│ ✓ 已复制到剪贴板         ⚠ 已过滤疑似 github-token      │   ← 状态行 1 行
└───────────────────────────────────────────────────────┘
```

- 垂直布局：标题 `Length(1)` / 列表 `Min(0)` / 快捷键提示 `Length(1)` / 状态行 `Length(1)`。
- 每行：`{选中箭头} {序号} {预览}{右侧对齐的字符数}`；高亮用 `ListState` + 主题色背景。
- 空历史显示占位文案「剪贴板历史为空，复制点文本试试」。

### 截断规则

1. 先把 `\n` 折叠为 `⏎`、`\t` 折叠为空格，避免多行文本破坏布局。
2. 再用 `unicode-width` 按**显示宽度**截断，上限 `min(40, 列表可用宽度 - 前缀宽度 - 后缀宽度)`。
3. 被截断时补 `...`（按显示宽度计入）。
4. 恰好等宽不截断、不补省略号（测试覆盖）。

### 状态消息

- `status: Option<(String, Instant)>`，TTL 3 秒，由 50ms 心跳负责过期。
- 消息来源：写回成功/失败、已删除、已清空、已过滤（附规则名）、存储失败、剪贴板不可用。
- 右侧可同时显示一条持久告警（如剪贴板不可用），与左侧瞬时消息互不覆盖。

### 按键映射

| 键 | 行为 |
|---|---|
| `j` / `Down` | 向下选择 |
| `k` / `Up` | 向上选择 |
| `Enter` | 写回选中文本到系统剪贴板 |
| `d` | 删除选中项 |
| `D` | 进入清空确认（`y` 确认，其他键取消） |
| `q` / `Esc` | 退出（在确认态下 `Esc` 只取消确认） |
| `Ctrl+C` | 退出（等同 `q`） |

## 9. 终端状态安全

- `TerminalGuard`（RAII）：构造时 `enable_raw_mode` + `EnterAlternateScreen` + 隐藏光标；`Drop` 时 `disable_raw_mode` + `LeaveAlternateScreen` + `Show`。
  - 正常退出与 `?` 提前返回走同一条恢复路径。
  - 恢复动作幂等：重复执行无害（hook 与 guard 可能都触发）。
- **panic hook**：`panic::set_hook` 保存旧 hook → 先恢复终端 → 再调用旧 hook，保证 panic 时不留乱码。
- 渲染输出用 `BufWriter<Stdout>` 包装 backend，避免每帧多次小 write。
- `enable_raw_mode()` 失败（非 TTY、无权限）→ 打印友好错误并退出码 1，不进入备用屏幕。
- 已知固有限制：`kill -9` 不会执行 `Drop`，终端会残留；README 中说明用 `reset` 恢复。
- 验证方式：手动注入一次 panic 跑一遍，确认终端干净；正常 `q` 退出确认光标可见、无残留画面。

## 10. 文件布局

```
Cargo.toml
README.md
src/main.rs       入口、事件循环、TerminalGuard、panic hook
src/app.rs        App 状态、按键分发、状态消息、ConfirmClear
src/clipboard.rs  监听线程、AppEvent/MonitorCmd 协议
src/history.rs    ClipEntry/History（MRU/去重/上限/删除）+ 单测
src/storage.rs    路径解析、原子读写、0600、容错 + 单测
src/filter.rs     敏感规则与开关 + 单测
src/ui.rs         draw()、按显示宽度截断 + 单测
tests/            如有跨模块集成测试放这里
```

每个模块单一职责、可独立测试；`history` / `filter` / `ui` 为纯逻辑，不依赖终端与线程。

## 11. 测试与验收标准

### 单元测试

- `history`：MRU 提升顺序、重复内容不产生新条目、第 101 条淘汰最旧、删除后选中项修正（删中间/删末尾/删空）、清空、id 稳定。
- `ui`：CJK 宽度截断、恰好等宽不补省略号、含 `\n`/`\t` 折叠、超长截断、空字符串。
- `filter`：7 条规则各一组正例；反例包括正常中文散文、普通 URL、多行代码片段、短 token（不应命中）；`DITTO_NO_FILTER=1` 时全部放行。
- `storage`：往返一致、文件缺失、JSON 损坏、未知 version、目录不可写（用临时目录）、权限位为 0600。

### 手工验收

1. 另一应用复制文本 → 500ms 内出现在列表顶部。
2. 重复复制同一内容 → 提升到顶部，不产生第二条。
3. `Enter` → 在别处粘贴成功，且列表不出现重复条目。
4. 退出后重启 → 历史仍在且顺序一致。
5. `d` 删除、`D`+`y` 清空 → 重启后确认已落盘。
6. 复制一个 `ghp_...` token → 不入历史，状态栏显示过滤提示。
7. `kill -9` 后终端可用 `reset` 恢复（README 记录）。

### 每步执行的验证命令

```
cargo build
cargo test
cargo clippy -- -D warnings
cargo tree -d          # 确认 crossterm 无多版本
```

结论以命令实际输出为准，不出现未经验证的"已完成"表述。

## 12. 风险与权衡

| 风险 | 处理 |
|---|---|
| X11 下剪贴板所有权需进程存活 | 监听线程持有唯一实例至程序退出 |
| macOS 上其他应用写入时 `get_text` 可能返回 `Err` | 视为"本轮无内容"，不视为致命错误 |
| 高熵规则误伤长 URL/正常长文本 | 阈值取高（长度 ≥48 + 必须大小写与数字混排） + URL/路径显式豁免 + `DITTO_NO_FILTER=1` 逃生阀 + 命中时状态栏明示规则名；反例已进单测 |
| 每次变更都落盘带来的 I/O | ≤100 条小文本，原子写开销可忽略；若将来变大再加 debounce（不改接口） |
| 持久化文件含敏感内容 | 0600 权限 + 删除/清空立即落盘 + 敏感内容默认不入库 |
