# STFN 架构设计

> 状态：v0.2.0 已发布
> 关联：`README.md`、`v0.2-plan.md`、`pronunciation.md`、`selection.md`、`ui.md`、`settings.md`、`providers.md`、`cli.md`、`config.md`

## 1. 目标与约束

- 单二进制 `stfn`，同时提供 GUI 与 CLI。
- 冷启动 < 300ms，常驻内存 < 100MB；无 Python / Node 运行时。
- niri / Wayland 下 fcitx5 输入可靠；窗口按普通平铺窗口运行。
- 默认零配置可用（Bing），云厂商按需启用。
- 不保存翻译历史，不做遥测。

## 2. 进程模型

| 模式 | 触发 | 行为 |
| --- | --- | --- |
| GUI | `stfn` | 新进程，启动 GTK4 主窗口 |
| Settings | `stfn settings`、主窗口齿轮、`Ctrl+,` | 新进程启动主窗口并 push 设置页；窗口内 `Ctrl+,` 也进入设置页 |
| Selection | `stfn selection` | 新进程，读取 primary selection、填入并自动翻译 |
| CLI | `stfn <text>`、`stfn -` | 不初始化 GTK；翻译后输出并退出 |

- 无常驻 daemon；每个窗口一个进程，关闭窗口即退出。
- v0.2 取消单实例：`adw::Application` 使用 `gio::ApplicationFlags::NON_UNIQUE`，每次启动独立进程/窗口，不维护 `state_slot`，不做 command-line 转发。
- GApplication ID 使用 `dev.stfn.Stfn`；窗口 realize 后调用 `gdk_wayland_toplevel_set_application_id(toplevel, "stfn")`，使 niri 的 `match app-id="^stfn$"` 生效。
- `stfn settings` 每次启动新进程并直接进入设置页；`stfn selection` 每次启动新进程读取 primary selection。
- CLI 进程不注册 GApplication。

## 3. 启动流程

### GUI

1. 解析 argv，得到 `GuiIntent`（Open / Settings / Selection）或 CLI。
2. 读取 `config.toml` 与凭据（见 `config.md`）。
3. 初始化 GTK / libadwaita，创建 `NON_UNIQUE` 的 `StfnApplication`。
4. 构建主窗口与 `AdwNavigationView`（见 `ui.md`），按启用平台创建结果行。
5. Settings 意图 push 设置页；Selection 意图读取 primary selection。
6. 懒加载后台 runtime（翻译 / 词典 / TTS）。
7. present 后 idle 聚焦输入框。

### CLI

1. 解析参数。
2. 读取配置与凭据（env > Secret Service > 文件）。
3. 构建 provider 集合；`--source` 覆盖默认源。
4. 读取文本（argv 或 stdin）。
5. 语言检测 → 并发请求 → 输出。
6. 退出码见 `cli.md`。

## 4. 模块划分

| 模块 | 职责 | 主要依赖 |
| --- | --- | --- |
| `main` | 参数分派、日志初始化、退出码 | clap |
| `cli` | 文本收集、输出格式、通知、朗读 | providers, notify |
| `app` | GtkApplication、窗口生命周期、GuiIntent | gtk4, libadwaita |
| `ui` | 主窗口、结果卡片、设置页、选区注入 | gtk4, libadwaita |
| `core` | 语言检测、去抖、请求编排、缓存 | tokio, lru |
| `providers` | Bing / 火山 / 阿里适配器、统一 trait | reqwest, hmac, sha2, sha1 |
| `pronunciation` | 英文单词词典查询、音标解析、音频缓存 | reqwest, lru |
| `tts` | Edge TTS 合成、句子朗读、PipeWire 音频播放 | msedge-tts, rodio |
| `config` | 配置读写、校验、迁移 | serde, toml_edit |
| `secrets` | Secret Service / 0600 文件 | keyring, zeroize |
| `platform` | niri / Wayland 细节（app-id） | gdk4-wayland |

依赖方向：`ui -> core -> providers`；`app -> ui`；`config` / `secrets` 被 `app` 与 `cli` 使用；provider 不依赖 UI。

## 5. 线程与异步

- GTK 主线程只做 UI，不做网络与文件 I/O。
- 专用 tokio runtime（2 worker）在后台线程，首次需要翻译时创建。
- UI → core：`async_channel::Sender<Command>`。
- core → UI：`glib::MainContext::invoke` 回到主线程。
- 配置保存是短文件 I/O，可在主线程同步完成（原子写），避免跨线程复杂度。
- 词典查询、Edge TTS 合成走后台 runtime；音频播放由独立线程持有 `rodio::OutputStream`。
- primary selection 读取在 GTK 主循环发起，使用异步 `read_text_future` + 2s 超时。
- 请求取消：每次输入递增 `generation: AtomicU64`；结果携带 generation，UI 丢弃过期结果；可选 abort 旧请求。

## 6. 翻译数据流

```
GtkTextView buffer changed
  → 若 auto_translate：重置 debounce 计时器
  → 计时器触发 / Ctrl+Enter
  → detect_language(text)
  → generation += 1
  → 对每个启用 provider 并发 translate（各自 timeout）
  → 结果 / 错误按 provider 顺序回传
  → UI 丢弃旧 generation，更新对应行
```

- 空输入：清空结果，不请求。
- 输入法 preedit：`GtkTextView` 的 buffer 在 commit 后才变化；若实测 fcitx5 在 preedit 期间触发 `changed`，则在 IM context 的 preedit 区间内暂停计时（见 `ui.md` 风险）。
- 同一 `(provider, text, from, to)` 命中内存缓存则直接返回。

## 7. 数据模型

```rust
struct TranslateRequest {
    text: String,
    from: Lang,
    to: Lang,
}

struct TranslateOutput {
    text: String,
    detected: Option<Lang>,
    phonetics: Option<Phonetics>,
    audio: Option<Audio>,
}

enum ProviderErrorKind {
    Auth,
    RateLimit,
    Network,
    Timeout,
    Unsupported,
    Server,
    Parse,
}

struct ProviderError {
    kind: ProviderErrorKind,
    message: String,
    retryable: bool,
}
```

- `Lang`: `Zh` / `En` / `Auto`。
- 错误消息面向用户，不得包含密钥或请求头。
- provider 顺序 = 配置顺序；UI 行顺序一致。

## 8. 错误与降级

- 单 provider 失败：该行显示错误 + 重试，不影响其他行。
- 全部失败：主窗口顶部显示全局错误；CLI 返回 1。
- provider 启用但缺少凭据：该行显示“缺少凭据，打开设置”，不发送请求。
- Bing 接口失效：显示网络 / 解析错误，提示切换云厂商；不连续重试。
- 配置损坏：备份为 `config.toml.bak` 并使用默认值，UI 提示。

## 9. 安全与隐私

- 密钥优先级：环境变量 > Secret Service（`keyring` crate）> `credentials.toml`（0600）。
- 词典/ TTS attribution 见 `pronunciation.md`。
- 日志只记录 provider、状态码、耗时；不记录请求 / 响应正文、Authorization、密钥。
- 不落盘翻译内容；缓存仅内存，退出即清。
- 所有云 API 走 TLS（rustls）。
- `--notify` 会把译文交给桌面通知服务，属于用户显式操作。
- 英文词典查询会把单词发送给 dictionaryapi.dev；单词/句子音频会发送给有道；Edge TTS 会把待朗读文本发送给微软。均不记录、不遥测。
- 选区翻译只读取 primary selection，不覆盖剪贴板。
- 设置页修改未保存时返回/退出会弹确认；密钥在 UI 中显示为 `******`，清空保存才删除。

## 10. 依赖清单

| 用途 | crate |
| --- | --- |
| GUI | `gtk4`, `libadwaita`, `glib`, `gdk4-wayland` |
| 异步 | `tokio`, `futures`, `async-channel` |
| HTTP | `reqwest`（`rustls-tls`, `json`） |
| 序列化 | `serde`, `toml`, `toml_edit` |
| 参数 | `clap`（derive） |
| 签名 | `hmac`, `sha2`, `sha1`, `hex`, `base64`, `uuid` |
| 密钥 | `keyring`（zbus Secret Service）, `zeroize`, `secrecy` |
| 通知 | `notify-rust` |
| 日志 | `tracing`, `tracing-subscriber` |
| 缓存 | `lru` |
| 错误 | `thiserror`, `anyhow` |
| 朗读 / TTS（v0.2） | `msedge-tts`, `rodio` |
| 词典测试 | `httpmock`（dictionaryapi fixture） |
| 测试 | `wiremock` / `httpmock`, `assert_cmd` |

> 不引入 WebKit、Electron、Qt，也不引入 OpenSSL（使用 rustls）。

## 11. 构建与打包

- Release：`lto = "thin"`、`codegen-units = 1`、`strip = true`、`opt-level = 3`。
- 系统依赖：GTK4、libadwaita；朗读功能额外需要音频输出（PipeWire）。
- Arch：PKGBUILD + `.desktop` + 图标；后续发布 AUR。
- 单二进制大小目标 < 15MB（不含系统库）。

## 12. 测试策略

按 `AGENTS.md`，以集成 / 端到端为主：

- provider 契约测试：mock HTTP + 固定 JSON fixture；覆盖成功、401、429、超时、畸形响应。
- CLI E2E：`assert_cmd` 验证 stdout、`--json`、退出码、stdin。
- 配置 / 密钥：临时 XDG 目录；0600 权限；环境变量覆盖；keyring 不可用降级。
- GUI 手工验收：fcitx5 中文输入、preedit、多行、复制、错误重试、设置保存。
- 性能：`hyperfine` 测冷启动；`/proc/<pid>/status` 观察 RSS。

## 13. 风险与待验证

| 风险 | 影响 | 处理 |
| --- | --- | --- |
| Bing 网页会话结构变更 | 默认源失效 | 会话解析失败时提示；可切换云厂商 |
| GTK4 app-id / 焦点细节 | niri 规则不生效 | 用 `gdk_wayland_toplevel_set_application_id`；实测 |
| preedit 触发 changed | 拼音被翻译 | 实测 fcitx5；必要时监听 IM context |
| Secret Service 不可用 / 锁定 | 密钥无法保存 | 明确提示；用户确认后回退 0600 |
| 云厂商签名实现偏差 | 401 | 火山已实测；阿里用官方参数 + mock 测试，真实密钥待验 |
| 词典 API 不可用 | 音标缺失 | 超时/404 静默隐藏；瞬时失败不缓存，不阻塞翻译 |
| Edge TTS 非官方 | 发音不可用 | 有道兜底；仍失败保留播放按钮供重试 |
| primary selection 不被应用设置 | 选区翻译失败 | 明确提示；不做剪贴板/Ctrl+C 回退 |
| 多进程内存重复 | 多窗口占用高 | 接受短生命周期；关闭即退出 |
