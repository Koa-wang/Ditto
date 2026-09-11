# Ditto 📋

终端里的剪贴板历史管理器。后台监听系统剪贴板，用 Ratatui 渲染一个可选择的列表，选中即写回。

```
┌ 📋 Ditto  剪贴板历史  3/100 ─────────────────────────────┐
│ ▸  1  curl -H 'Authorization: Bearer eyJ...'      (312 字) │
│    2  今天下午的会议纪要：讨论了三个方案……          (86 字) │
│    3  https://example.com/a/very/long/path/t...    (74 字) │
└───────────────────────────────────────────────────────────┘
 j/k ↑/↓ 选择 · Enter 写回剪贴板 · d 删除 · D 清空 · q/Esc 退出
 ✓ 已复制到剪贴板
```

## 运行

```bash
cargo run            # 开发
cargo run --release  # 日常使用
```

需要 Rust 2021（实测 1.97）。macOS 与 Linux（X11 / Wayland）均可，Linux 会自动启用
`wayland-data-control`。

## 快捷键

| 键 | 行为 |
|---|---|
| `j` / `↓` | 向下选择 |
| `k` / `↑` | 向上选择 |
| `Enter` | 把选中内容写回系统剪贴板（并提升到顶部） |
| `d` | 删除选中条目 |
| `D` | 清空全部历史（再按 `y` 确认，其他键取消） |
| `q` / `Esc` / `Ctrl+C` | 退出 |

## 行为

- **轮询**：后台线程每 500ms 读一次剪贴板；UI 心跳 50ms。
- **行内降级**：列表行按 `正文 > 序号 > 字数统计` 的优先级占用宽度。窄终端下会先丢掉 `(N 字)`，
  而不是把正文挤成空白；任何宽度下拼出的行都不会超过可用列数（ratatui 对超宽行是从右侧静默裁掉的）。
- **宽度计算**：全程用 `unicode-width` 按终端**显示列数**计算（CJK 全角算 2 列），
  绝不用 `&s[..n]` 按字节切片。
- **MRU 去重**：重复内容不产生新条目，而是提升到顶部；历史上限 100 条，超出淘汰最旧。
- **持久化**：`<data_dir>/ditto/history.json`
  - macOS：`~/Library/Application Support/ditto/history.json`
  - Linux：`~/.local/share/ditto/history.json`
  - 原子写入（临时文件 + rename），权限收紧到 `0600`。
- **敏感内容默认不入库**，命中时状态栏提示规则名。想全部放行：

  ```bash
  DITTO_NO_FILTER=1 cargo run
  ```

  规则：私钥块、JWT、AWS `AKIA/ASIA`、GitHub `ghp_*`、Slack `xox*-`、OpenAI `sk-*`、
  以及长度 ≥48 且大小写与数字混排的高熵串（长 URL/路径已豁免，避免误伤）。

## 结构

```
src/main.rs       入口、事件循环、TerminalGuard(RAII)、panic hook
src/app.rs        应用状态机与按键分发（纯逻辑，可单测）
src/clipboard.rs  监听线程、AppEvent/MonitorCmd 通道协议
src/history.rs    MRU 去重、100 条上限、删除/清空
src/storage.rs    原子落盘、0600、损坏文件容错
src/filter.rs     敏感内容规则
src/ui.rs         Ratatui 渲染、按显示宽度截断
```

两处关键设计：

1. **`arboard::Clipboard` 只由监听线程持有**，主线程写回剪贴板也走 `MonitorCmd::Write` 命令。
   这样写回与轮询共用同一个 `last_seen`，不会把自己写回的内容误当成「外部新复制」；
   同时避免 Linux/X11 下双实例争抢剪贴板所有权（X11 需要进程存活以应答粘贴请求）。
2. **剪贴板事件永远用 `try_recv` 非阻塞排空**，键盘读取带 50ms 超时，两者互不阻塞。

终端状态由 RAII `Drop` + panic hook 共同兜底，正常退出与 panic 都会恢复原始模式、
退出备用屏幕并显示光标。

## 测试

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

79 个单测覆盖 MRU/去重/上限淘汰/选中项修正、CJK 宽度截断与行内降级阶梯、7 条过滤规则的正反例、
落盘往返与损坏容错。其中 `ui::render_tests` 用 `ratatui::backend::TestBackend` 直接断言**渲染后的缓冲区**
（行宽不溢出、首字符不被覆盖、边框不被写穿、字数右对齐），不依赖肉眼看终端；
`cargo test dump_render_evidence -- --nocapture` 可打印各宽度下的实际渲染结果。

## 已知限制

- 仅处理纯文本，不处理图片与文件。
- 标题行与快捷键提示行是固定文案，终端过窄时会被裁切（列表行已按优先级降级，这两行未做）。
- `kill -9` 不会执行 `Drop`，终端会残留原始模式；在该终端执行一次 `reset` 即可恢复
  （这是终端程序的固有限制，无法在进程内解决）。
