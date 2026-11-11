# STFN — Simple Translator For Niri

为 [niri](https://github.com/YaLTeR/niri) 打造的极简中英翻译工具：打开窗口输入即译，也可以在终端一条命令拿到结果。

## 定位

- 只解决一件事：**中英互译**，快、轻、少依赖。
- 默认零配置可用；需要更高配额或企业服务时，再接入云厂商。
- 只做 niri / Wayland 下体验正确的版本（输入法候选框、窗口行为）。

**非目标**：OCR / 划词翻译、生词本与复习、多语种、离线词典、文档翻译。

## 功能

### 翻译窗口

- 单窗口、单列布局：第一行是多行输入框，之后每个已配置的翻译平台占一行，纵向排列。
- 关闭窗口即退出，不常驻托盘；重复执行 `stfn` 只聚焦已有窗口。
- 输入框支持多行；Enter 换行，Ctrl+Enter 立即翻译。
- 输入停顿 300ms 后自动翻译（可配置），无需按回车。
- 输入法组合（preedit）期间不触发；候选词上屏后才开始计时，避免把拼音当输入。
- 中文输入依赖 fcitx5 / ibus 正常显示候选框（text-input-v3）。
- 输入框所在行显示检测到的方向（如 EN → 中）。
- 结果行按配置顺序展示，只显示已配置且可用的平台；未启用的平台不占位。
- 音标与朗读（v1.1）：数据存在时才显示，不占位；v1 只做文本翻译。
- 单个平台失败不影响其他行，该行显示错误并支持重试。
- 结果文本可选中复制，每行结果提供复制按钮；Esc 清空输入，Ctrl+L 聚焦输入框。

### 配置窗口

- 主窗口齿轮按钮，或 `stfn settings` 打开。
- 自动翻译开关；开启时可调输入延迟（200–2000ms，默认 300ms）。
- Bing / 火山引擎 / 腾讯云的启用状态、区域与密钥。
- 密钥默认存入系统 Secret Service（gnome-keyring 等）；不可用时退回 `0600` 权限的本地凭据文件。
- 环境变量优先级最高；来自环境变量的密钥在 UI 中只读。
- 密钥输入默认掩码，不写日志、不打印、不进入桌面通知。

### 翻译源

- **Bing**：默认，免 key（非官方接口，可能随时失效）。
- **火山引擎机器翻译**：配置 AK/SK 后启用。
- **腾讯云 TMT**：配置 SecretId / SecretKey 后启用。
- 多平台并行请求，结果独立渲染。

### 语言方向

- 自动判断：输入含 CJK 字符 → 译为英文；否则译为中文。
- 纯数字 / 符号 / 空白：不发起请求。
- v1 仅支持中英；其他语种明确提示不支持。

### 命令行

GUI 与 CLI 共用同一套 provider 层，不做第二套逻辑。

| 命令 | 行为 |
| --- | --- |
| `stfn` | 打开翻译窗口；已在运行则聚焦已有窗口 |
| `stfn settings` | 打开配置窗口 |
| `stfn hello` | 翻译并输出到 stdout |
| `stfn --notify hello` | 翻译结果发送到桌面通知 |
| `stfn --speak hello` | 翻译并朗读译文（v1.1） |
| `stfn --json hello` | JSON 输出，便于脚本使用 |
| `stfn --to en hello` | 强制目标语言 |
| `stfn --source all hello` | 指定翻译源 / 聚合全部可用源 |
| `echo hello \| stfn -` | 从 stdin 读取 |

- 退出码：0 成功，非 0 失败；`--json` 字段保持稳定，便于脚本消费。
- `--source` 不指定时使用配置里的 `default_source`。

### 性能与依赖

- Rust 单二进制；除 GTK4 等系统库外，无 Python / Node 等运行时依赖。
- 目标：GUI 冷启动 < 300ms，常驻内存 < 100MB（后续实测校准）。
- 空闲不轮询、不发请求；同一会话内相同输入走内存缓存。

## 平台能力矩阵

| 能力 | Bing（默认） | 火山引擎 | 腾讯云 TMT |
| --- | --- | --- | --- |
| 凭据 | 无需 | AK/SK | SecretId / Key |
| 中→英 / 英→中 | ✅ | ✅ | ✅ |
| 自动识别源语言 | ✅ | 待实测 | ✅ |
| 音标 | v1.1 再评估 | ❌ | ❌ |
| 朗读 | v1.1（Edge TTS，非官方） | 需另开语音合成 | 需另开语音合成 |
| 成本 | 免费（非官方） | 按量 / 有免费额度 | 有每月免费额度 |

> Bing 为社区常用的非官方接口，无 SLA 保证；这是"默认零配置"的代价，README 与 UI 中都要说明。

## 隐私

- 不保存输入历史，不落盘翻译内容。
- 输入只发送到当前启用的翻译平台；未启用的平台不会收到请求。
- 不做遥测、不内置统计上报。

## 配置

配置文件：`$XDG_CONFIG_HOME/stfn/config.toml`（默认 `~/.config/stfn/config.toml`），首次启动自动生成最小配置。

```toml
[general]
auto_translate = true
debounce_ms = 300
target = "auto"          # auto | zh | en
default_source = "bing"

[providers.bing]
enabled = true

[providers.volcengine]
enabled = false
region = "cn-north-1"

[providers.tencent]
enabled = false
region = "ap-guangzhou"
```

密钥不写在 `config.toml`：设置窗口会写入 Secret Service，或回退到 `credentials.toml`。

- 只需填写要用的平台；`enabled` 为 true 且凭据齐全才会出现在结果区。
- 启用多个平台时 GUI 全部展示；`default_source` 只决定 CLI 未指定 `--source` 时的默认源。
- 凭据可用环境变量覆盖（如 `STFN_VOLC_ACCESS_KEY`），推荐放环境变量以免明文落盘。
- 配置窗口直接改写 `config.toml`（保留注释）与凭据存储，保存后立即生效，无需重启。
- 密钥优先写 Secret Service；不可用时写 `$XDG_CONFIG_HOME/stfn/credentials.toml`（权限 0600）。

## niri 集成

默认按普通平铺窗口打开，占一个列；绑定快捷键即可随时呼出。`Mod+T` 是 niri 默认终端键，这里用 `Mod+Shift+T`（Translate）：

```kdl
binds {
    Mod+Shift+T { spawn "stfn"; }
}

window-rule {
    match app-id="^stfn$"
    default-column-width { proportion 0.25; }
}
```

初始宽度取屏幕的 1/4，翻译工具不需要占满整个工作区；需要时用 `Mod+R` 或拖拽调宽。

如果更想要悬浮弹窗，可以额外加一条窗口规则（可选）：

```kdl
window-rule {
    match app-id="^stfn$"
    open-floating true
    default-floating-position x=0 y=0 relative-to="top"
    default-column-width { proportion 0.25; }
    default-window-height { proportion 0.6; }
}
```

## 技术架构

- 语言：Rust
- GUI：GTK4 + libadwaita（gtk4-rs / libadwaita-rs），优先保证 fcitx5 / text-input-v3 输入体验
- HTTP：reqwest + rustls，异步运行时 tokio
- 单实例：GApplication 唯一实例，重复执行 `stfn` 只聚焦已有窗口
- 密钥：Secret Service（oo7/zbus）优先，`0600` 凭据文件兜底
- 通知：D-Bus `org.freedesktop.Notifications`
- 音频：PipeWire

设计文档见 `docs/tech/`：

- `architecture.md` —— 进程模型、单实例、模块划分
- `ui.md` —— 主窗口布局、交互、输入法
- `settings.md` —— 配置窗口、密钥存储与校验
- `providers.md` —— 各翻译平台接口与能力降级
- `cli.md` —— 命令行参数与输出格式
- `config.md` —— 配置项与环境变量

## 安装

Arch Linux（AUR / 预编译包待发布）：

```bash
cargo install --path .   # 从源码安装
```

## 开发状态

v0.1 目标：翻译窗口 + 配置窗口 + Bing + CLI 回显 / 通知 + 配置与密钥存储。
后续版本：朗读、更多平台、快捷键与交互打磨。

## License

MIT
