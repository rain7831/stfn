# niri 选区翻译设计

> 状态：v0.2.0 已发布
> 关联：`v0.2-plan.md`、`architecture.md`、`cli.md`、`ui.md`

## 1. 目标与范围

- 用户在任意应用选中一段文本，按快捷键打开 STFN，窗口自动填入并翻译该文本。
- 只面向 **niri / Wayland**，不做通用桌面方案。
- 只读取 **primary selection**；不模拟 Ctrl+C，不覆盖用户剪贴板。
- 每次调用启动新进程和独立窗口；不维持单实例/常驻进程。

## 2. 进程模型

v0.2 取消单实例：

```text
stfn            -> 新进程，新主窗口
stfn settings   -> 新进程，新主窗口，并 push 设置页
stfn selection  -> 新进程，新主窗口，读取 primary selection 后自动翻译
```

- `adw::Application` 使用 `gio::ApplicationFlags::NON_UNIQUE`。
- 不使用 `HANDLES_COMMAND_LINE`、`connect_command_line`、`state_slot`。
- 每个进程只创建一个 `UiState`；窗口关闭即进程退出。
- 多个窗口/进程允许同时存在；niri 会按普通窗口平铺它们。
- 代价：每个窗口重复 GTK/runtime 内存；符合短生命周期工具定位。

## 3. 入口与快捷键

### 3.1 CLI

新增子命令：

```bash
stfn selection
```

- 不是 CLI 输出模式，而是 GUI 启动意图。
- 参数由 clap 解析成 `GuiIntent::Selection`，随后 `ui::run(intent)`。

### 3.2 niri

推荐绑定：

```kdl
binds {
    Mod+Shift+T { spawn "stfn"; }
    Mod+Shift+Y { spawn "stfn" "selection"; }
}
```

- `Mod+Shift+Y` 在 niri 默认配置和当前 `bind.kdl` 中未占用。
- `Mod+Shift+T` 保持普通打开。
- 不使用 `spawn-sh`，不需要 shell 管道或 `wl-paste`。

## 4. 启动流程

```text
main.rs
  Cli::parse()
  Mode::Gui { intent: Selection }
  ui::run(GuiIntent::Selection)

ui::run
  adw::Application (NON_UNIQUE)
  connect_activate
    UiState::new(app, intent)
      build window / navigation / main page
      present
      if intent == Selection -> translate_selection()
```

- `translate_selection` 在窗口 present 后启动，避免读取选区时窗口尚未就绪。
- 读取完成后设置输入框、聚焦、立即翻译。
- 空选区不关闭窗口；显示状态提示，用户可手动输入。

## 5. 读取 primary selection

使用 GTK4 原生 API，不引入 `wl-paste`/`wl-clipboard` 依赖：

```rust
let display = gtk::gdk::Display::default()
    .ok_or("no display")?;
let clipboard = display.primary_clipboard();
let text = clipboard.read_text_future().await?;
```

- 读取在 GTK 主循环中发起，异步等待数据。
- 增加 2s 超时：选区 owner 无响应时返回空并在 UI 提示。
- 只接受 `text/plain`/UTF-8；如果 owner 只提供 HTML，视为空。
- 读取失败/超时不弹全局错误，只在输入区下方显示状态。

### 5.1 文本处理

- trim 首尾空白；保留内部换行。
- 空/纯空白：状态提示“没有读到选中文本，请先选中或复制”。
- 超过 5000 字符：**直接拒绝**，不填入输入框、不自动翻译；状态提示“选中文本超过 5000 字符”。
- 去除 NUL 等控制字符（保留 `\n`、`\t`）。
- 写入 `GtkTextView` buffer；光标置于文末。

### 5.2 自动翻译与聚焦

- 设置文本后：
  - `input.grab_focus()`；
  - 跳过 debounce，直接 `translate_now()`。
- 如果设置文本触发了 buffer changed，正常 debounce 会被 generation/计时器覆盖；实现时用一个“注入中”标记避免重复请求。
- 窗口 present 后仍按 v0.2 聚焦策略做一次 idle 聚焦。

## 6. 空选区与失败

不提供回退链：

- 不读普通剪贴板：避免把旧剪贴板内容当作用户选区。
- 不调用 `wtype` 发送 Ctrl+C：避免外部依赖、权限问题和覆盖剪贴板。
- primary selection 为空时只提示：
  “没有读到选中文本，请先选中或复制。”

## 7. 与设置页的关系

- v0.2 每个进程独立：
  - `stfn settings` 是新进程，有自己的窗口/设置页；
  - `stfn selection` 是新进程，不存在其他实例的设置页。
- 同一窗口内仍可通过 `Ctrl+,` 打开应用内设置页；关闭/返回时沿用现有 dirty 确认。
- 选区注入发生在主页面；如果用户在同一个 selection 窗口里随后打开设置页，不影响已经注入的文本。

## 8. 边界与异常

| 场景 | 行为 |
| --- | --- |
| 应用不设置 primary selection | 读取为空，提示用户先选中/复制 |
| 选区 owner 退出 | 读取失败/空，提示 |
| 选区只有 HTML | 视为空，提示 |
| 文本 > 5000 字符 | 直接拒绝，不填入、不翻译，并提示 |
| 无 Wayland display | GUI 启动失败，stderr 报错 |
| 连续按快捷键 | 每个进程/窗口独立，允许打开多个 |
| 选区包含换行 | 保留内部换行，trim 首尾 |
| 窗口启动慢 | 读取在 present 后执行，选区仍由 compositor 持有 |

- primary selection 是 compositor 级数据，窗口获得焦点不会清空它；但不能保证所有应用都设置。
- 不做“选中后自动弹出”监听；只在快捷键触发时读取一次。

## 9. 架构改动

| 位置 | 改动 |
| --- | --- |
| `src/main.rs` | `Mode::Gui` 携带 `GuiIntent`；调用 `ui::run(intent)` |
| `src/cli.rs` | 新增 `selection` 子命令；`GuiIntent` 枚举 |
| `src/ui/mod.rs` | 移除 `state_slot`/`connect_command_line`/单实例；`UiState::new(app, intent)`；`translate_selection()` |
| `src/ui` | 窗口 `NON_UNIQUE`；focus idle；空选区状态 |
| `README` | v0.2 niri 绑定、进程模型说明 |

### 9.1 需移除/简化

- `HANDLES_COMMAND_LINE` / `connect_command_line`
- `state_slot: Rc<RefCell<Option<Rc<UiState>>>>`
- pending `open_settings` / `GuiIntent` 的跨实例转发
- “重复执行 stfn 只聚焦已有窗口”的文档与验收

## 10. 测试

- 单元/集成：
  - `GuiIntent` 解析：`stfn` / `stfn settings` / `stfn selection`。
  - 文本清洗：trim、控制字符、超长直接拒绝、换行保留。
  - 读取抽象：注入 fake primary reader，覆盖空/超时/正常文本。
- 手工：
  - niri 下在 Firefox、GTK 应用、终端分别选中文本，按 `Mod+Shift+Y`。
  - 选区为空、选区在窗口打开后变化、连续按快捷键。
  - selection 窗口内打开设置页、关闭窗口。
- 自动化不依赖真实 compositor；真实 primary selection 作为手工验收。

## 11. 验收

- [x] `stfn selection` 新进程打开窗口，不依赖已有实例。
- [x] `stfn selection` 读取 primary selection（用 `wl-copy --primary` 模拟）并自动填入、翻译。
- [ ] 在 niri 的浏览器/GTK/终端中通过 `Mod+Shift+Y` 做跨应用手工抽查（依赖用户绑定）。
- [x] 空选区显示提示，不读取旧剪贴板。
- [x] 超过 5000 字符直接拒绝：不填入、不翻译，并提示。
- [x] 注入后输入框聚焦，可直接编辑。
- [x] 连续触发可打开多个独立窗口；关闭即退出。
- [x] 不引入 `wl-clipboard`/`wtype` 运行时依赖。

> 未勾选项为跨应用 owner + niri 快捷键的手工抽查；`stfn selection` 主流程已通过。

## 12. 风险

- primary selection 不是所有应用都设置；这是 Wayland/niri 的固有限制。
- GTK primary clipboard 读取依赖合 compositor 的 data device 实现；niri 已实测 `wl-paste --primary` 可用。
- 多进程会重复内存；用户接受短生命周期。
- 选区读取可能被 owner 延迟；需要超时和明确提示。
