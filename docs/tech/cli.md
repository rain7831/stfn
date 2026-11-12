# 命令行设计

> 状态：v0.2.0 已发布（`selection` / `--speak` / JSON `phonetic`）
> 关联：`README.md`、`architecture.md`、`providers.md`、`config.md`

## 1. 命令模型

```
stfn [OPTIONS] [TEXT]...
stfn settings
stfn selection
```

- 无 `TEXT` 且无子命令：新进程打开 GUI 主窗口。
- `TEXT` 为普通参数：翻译并输出到 stdout。
- `TEXT` 为 `-`：从 stdin 读取到 EOF。
- `settings`：新进程打开主窗口并进入设置页。
- `selection`：新进程读取 primary selection，填入并自动翻译（v0.2）。
- GUI 模式不使用单实例；每个进程一个窗口。CLI 模式不初始化 GTK。

## 2. 参数

| 参数 | 取值 | 默认 | 说明 |
| --- | --- | --- | --- |
| `-t, --to` | `auto` / `zh` / `en` | 配置 `general.target` | 强制目标语言 |
| `-s, --source` | `bing` / `volcengine` / `alibaba` / `all`，可用逗号分隔 | 配置 `general.default_source` | 指定翻译源 |
| `--notify` | flag | off | 结果发送桌面通知，不输出 stdout |
| `--speak` | flag | off | 翻译后朗读英文译文（v0.2；默认 US） |
| `--json` | flag | off | JSON 输出 |
| `-h, --help` | flag | — | 帮助 |
| `-V, --version` | flag | — | 版本 |

约束：

- `--notify` 与 `--json` 互斥，同时出现返回 2。
- `--notify` 与 `--speak` 可以同时使用（先通知后朗读）。
- `--json` 与 `--speak` 可以同时使用（JSON 到 stdout，音频为副作用）。
- `--source all` 展开为所有已启用平台；缺凭据的平台以错误行展示。
- `stfn selection` 不输出 stdout；它是 GUI 启动意图。
- 文本参数用单个空格连接；`-` 只能作为唯一的文本参数。
- 单次输入上限 5000 字符；超出返回 2。

## 3. 输出

### 3.1 默认（纯文本）

单平台：只输出译文 + 换行。

```
$ stfn hello
你好
```

多平台：按 provider 分块，块头为 `[provider]`：

```
$ stfn --source all hello
[bing]
你好
[alibaba]
你好
```

- 错误写到 stderr，不混入 stdout。
- 成功结果至少一个即返回 0。

### 3.2 `--json`

```json
{
  "input": "hello",
  "source": "en",
  "target": "zh",
  "results": [
    {
      "provider": "bing",
      "text": "你好",
      "detected_source": "en",
      "phonetic": null,
      "latency_ms": 123
    }
  ],
  "errors": [
    {
      "provider": "alibaba",
      "kind": "auth",
      "message": "认证失败，请检查密钥"
    }
  ]
}
```

- `source` / `target` 为最终使用的语言；`auto` 解析后不出现。
- `phonetic` 对英文单词填 US > UK > generic IPA；未命中/非单词为 `null`。
- `errors` 无错误时为空数组。
- 字段稳定；新增字段只增不改。

### 3.3 `--notify`

- 标题：`STFN · EN → 中`。
- 正文：译文，超过 200 字符截断并加省略号。
- 超时 5s；无图标。
- 不包含输入文本、密钥、请求头。
- 通知服务不可用：stderr 报错并返回 1。

### 3.4 `--speak`

- 先翻译，再用 TTS 播放译文；v0.2。
- 当前仅支持英文译文；中文目标返回 1 并提示。
- 默认 US 音色，暂不提供 `--accent`。
- 无可用 TTS：stderr 提示“朗读不可用”，返回 1。
- 音频播放不阻塞 stdout / JSON 输出。

## 4. 退出码

| 码 | 含义 |
| --- | --- |
| 0 | 至少一个平台成功 |
| 1 | 运行时失败：全部平台失败、通知 / 朗读不可用 |
| 2 | 用法 / 配置 / 输入错误：参数冲突、空输入、语言不支持、配置损坏 |

- 不使用其他退出码，脚本可稳定判断。
- 错误信息只写 stderr，且脱敏。

## 5. stdin

```bash
echo "hello" | stfn -
printf '你好' | stfn --json -
```

- 读到 EOF；去掉末尾单个换行；保留内部换行。
- 空 stdin：返回 2，提示“输入为空”。
- 超过 5000 字符：返回 2，提示上限。
- 不自动把管道输入当成隐式 stdin；必须显式 `-`，避免 `stfn` 在脚本里挂起。

## 6. 配置与密钥

- 配置读取顺序见 `config.md`。
- 密钥优先级：环境变量 > Secret Service（`keyring`）> `credentials.toml`。
- CLI 不修改配置，不写密钥。
- `--source` 指定的平台若缺少凭据：该平台报错；若全部失败返回 1。
- `--source all` 只包含已启用且凭据齐全的平台；一个都没有则返回 2。

## 7. 示例

```bash
# GUI
stfn

# 打开设置（新进程）
stfn settings

# 选区翻译（v0.2）：先在其它应用选中文本再执行
stfn selection

# 默认源，自动识别方向
stfn hello

# 强制目标英文
stfn --to en 你好

# 指定阿里云
stfn --source alibaba hello

# 聚合所有可用平台，JSON 给脚本
stfn --source all --json hello

# 桌面通知
stfn --notify hello

# 管道
echo "hello" | stfn --json -
```

## 8. 错误文案规范

- 面向用户，一句话说明“发生了什么 + 怎么办”。
- 不包含 URL、请求头、密钥、堆栈。
- 示例：
  - `认证失败，请在设置中检查阿里云 AccessKey`
  - `网络超时，请检查网络后重试`
  - `v1 仅支持中英互译`
  - `未启用任何翻译源，请打开设置`

## 9. 测试

- `assert_cmd` + 本地 mock：
  - 默认输出、`--json` schema、`--source all` 分块。
  - 退出码 0 / 1 / 2。
  - stdin、空输入、超长输入。
  - `--notify` / `--json` 冲突。
  - 配置缺失、配置损坏、密钥缺失。
- 通知 / 朗读用 trait 注入 fake，避免真实 D-Bus / 音频。
