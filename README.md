# Ditto 📋

终端里的剪贴板历史管理器：后台线程每 500ms 监听系统剪贴板，用 Ratatui 渲染一个可选择的列表，
`Enter` 把选中内容写回剪贴板。

<!-- ditto-screenshot:start 由 `cargo test dump_readme_screenshot` 生成，请勿手改 -->
```
 📋 Ditto  剪贴板历史  3/100
┌────────────────────────────────────────────────────────────────────────┐
│▸   1 curl -H 'Authorization: Bearer eyJhbG...                  (101 字)│
│    2 今天下午的会议纪要：讨论了三个方案，...                    (37 字)│
│    3 https://example.com/a/very/long/path/...                   (67 字)│
└────────────────────────────────────────────────────────────────────────┘
 j/k ↑/↓ 选择 · Enter 写回剪贴板 · d 删除 · D 清空 · q/Esc 退出
 ✓ 已复制到剪贴板
```
<!-- ditto-screenshot:end -->

## 安装

```bash
cargo install --path .   # 项目内置别名：cargo inst
dtto                     # 装完任意目录可用
```

改完代码重跑一次上面的命令覆盖安装。只想临时跑：`cargo run`（debug）/ `cargo run --release`。

> **命令为什么叫 `dtto` 而不是 `ditto`**：macOS 自带 `/usr/bin/ditto`（文件拷贝工具）。
> 装成 `ditto` 会二选一出问题：`~/.cargo/bin` 排在 `/usr/bin` 之后时（本机是位置 21 vs 12），
> 敲 `ditto` 实际执行的是系统工具；排在之前时（如 Homebrew 的 bin 目录）则会盖掉系统命令。
> 二进制名因此用 `dtto`，而 crate 名 / 产品名 / 数据目录仍是 `ditto`。

macOS 与 Linux（X11 / Wayland，后者自动启用 `wayland-data-control`）。edition 2021，实测
cargo 1.97 —— `cargo install` 自 1.97 起默认 release 构建，不再接受 `--release`。

## 使用

| 键 | 行为 |
|---|---|
| `j` / `↓` | 向下选择 |
| `k` / `↑` | 向上选择 |
| `Enter` | 把选中内容写回系统剪贴板（并提升到顶部） |
| `d` | 删除选中条目 |
| `D` | 清空全部历史（再按 `y` 确认，其他键取消） |
| `q` / `Esc` / `Ctrl+C` | 退出 |

- **MRU 去重**：重复内容不新增条目，而是提升到顶部；上限 100 条，超出淘汰最旧。
- **持久化**：`<data_dir>/ditto/history.json`（macOS 在 `~/Library/Application Support/`，
  Linux 在 `~/.local/share/`）。原子写入（临时文件 + rename），权限收紧到 `0600`。
- **敏感内容默认不入库**，命中时状态栏显示命中的规则名。`DITTO_NO_FILTER=1 dtto` 可整体关闭。
  规则：私钥块、JWT、AWS `AKIA/ASIA`、GitHub `ghp_*`、Slack `xox*-`、OpenAI `sk-*`，
  以及长度 ≥48 且大小写与数字混排的高熵串（长 URL/路径已豁免，避免误伤）。
- **窄终端降级**：一行内的宽度按 `正文 > 序号 > 字数统计` 的优先级分配，宁可丢掉 `(N 字)`
  也不把正文挤成空白。

## 原理

1. **剪贴板实例单一所有权**：`arboard::Clipboard` 只由监听线程持有，主线程写回剪贴板也走
   `MonitorCmd::Write` 命令。写回与轮询共用同一个 `last_seen`，因此不会把自己写回的内容
   误判成「外部新复制」；同时避免 Linux/X11 下双实例争抢剪贴板所有权（X11 需要进程存活
   才能应答粘贴请求）。
2. **事件循环互不阻塞**：剪贴板事件永远只做非阻塞 `try_recv`，键盘读取带 50ms 超时。
3. **终端状态兜底**：RAII `Drop` + panic hook 双保险，正常退出与 panic 都会恢复原始模式、
   退出备用屏幕并显示光标；恢复动作幂等，重复触发无害。
4. **行宽计算**：全程用 `unicode-width` 按显示列数算（CJK 占 2 列）。实现约束是**不得用
   `&s[..n]` 按字节切片**——中文会直接 panic，宽字符还可能被切成半截。

## 开发

```bash
cargo test                                    # 单测
cargo clippy --all-targets -- -D warnings
cargo inst                                    # 重新装到 PATH
```

| 文件 | 职责 |
|---|---|
| `src/main.rs` | 入口、事件循环、TerminalGuard(RAII)、panic hook |
| `src/app.rs` | 应用状态机与按键分发（纯逻辑，不依赖终端） |
| `src/clipboard.rs` | 监听线程、`AppEvent` / `MonitorCmd` 通道协议 |
| `src/history.rs` | MRU 去重、100 条上限、删除 / 清空 |
| `src/storage.rs` | 原子落盘、`0600`、损坏文件容错 |
| `src/filter.rs` | 敏感内容规则 |
| `src/ui.rs` | Ratatui 渲染、按显示宽度截断 |

测试覆盖 MRU / 去重 / 上限淘汰 / 选中项修正、CJK 宽度截断与行内降级阶梯、7 条过滤规则的
正反例、落盘往返与损坏容错。两处值得一提：

- `ui::render_tests` 用 `ratatui::backend::TestBackend` 直接断言**渲染后的缓冲区**
  （行宽不溢出、首字符不被覆盖、边框不被写穿），不靠肉眼看终端。
- 上面那张截图同样由代码渲染生成，并被 `readme_screenshot_is_up_to_date` 校验：
  **手改截图会让测试失败**。重新生成：`cargo test dump_readme_screenshot -- --nocapture`。

`cargo test dump_render_evidence -- --nocapture` 可打印各宽度下的实际渲染结果。

## 已知限制

- 仅处理纯文本，不处理图片与文件。
- 标题行与快捷键提示行是固定文案，终端过窄时会被裁切（列表行已按优先级降级，这两行未做）。
- `kill -9` 不会执行 `Drop`，终端会残留原始模式；在该终端执行一次 `reset` 即可恢复。
  这是终端程序的固有限制，进程内无法解决。
