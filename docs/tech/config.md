# 配置与密钥存储

> 状态：待审核 · 目标版本：v0.1
> 关联：`README.md`、`settings.md`、`architecture.md`、`cli.md`

## 1. 文件位置

| 文件 | 路径 | 权限 | 内容 |
| --- | --- | --- | --- |
| 配置 | `$XDG_CONFIG_HOME/stfn/config.toml`（默认 `~/.config/stfn/config.toml`） | `0644` | 非敏感设置 |
| 凭据回退 | `$XDG_CONFIG_HOME/stfn/credentials.toml` | `0600` | 云厂商密钥 |
| 目录 | `$XDG_CONFIG_HOME/stfn/` | `0700` | — |

- 首次启动生成最小 `config.toml`；不生成空的 `credentials.toml`。
- 密钥优先存 Secret Service；文件仅作为回退（见 `settings.md`）。

## 2. `config.toml`

```toml
version = 1

[general]
auto_translate = true
debounce_ms = 300
target = "auto"            # auto | zh | en
default_source = "bing"    # bing | volcengine | tencent | all

[providers.bing]
enabled = true

[providers.volcengine]
enabled = false
region = "cn-north-1"

[providers.tencent]
enabled = false
region = "ap-guangzhou"
```

## 3. 默认值

| 字段 | 默认 | 说明 |
| --- | --- | --- |
| `version` | `1` | 配置格式版本 |
| `general.auto_translate` | `true` | 输入停止后自动翻译 |
| `general.debounce_ms` | `300` | 200–2000 |
| `general.target` | `"auto"` | `auto` 时按检测结果决定 |
| `general.default_source` | `"bing"` | CLI 与 GUI 的默认源 |
| `providers.bing.enabled` | `true` | 无密钥 |
| `providers.volcengine.enabled` | `false` | 需要 AK/SK |
| `providers.volcengine.region` | `"cn-north-1"` | — |
| `providers.tencent.enabled` | `false` | 需要 SecretId/Key |
| `providers.tencent.region` | `"ap-guangzhou"` | — |

## 4. 优先级

### 4.1 普通配置

```
CLI 参数 > config.toml > 内置默认值
```

- `--to` 覆盖 `general.target`。
- `--source` 覆盖 `general.default_source`。

### 4.2 密钥

```
环境变量 > Secret Service > credentials.toml > 空
```

- 环境变量一旦存在，优先级最高，且不会被写回任何存储。
- CLI 不写密钥；设置窗口负责写入。

## 5. 环境变量

| 变量 | 对应字段 |
| --- | --- |
| `STFN_VOLC_ACCESS_KEY` | 火山引擎 Access Key |
| `STFN_VOLC_SECRET_KEY` | 火山引擎 Secret Key |
| `STFN_TENCENT_SECRET_ID` | 腾讯云 SecretId |
| `STFN_TENCENT_SECRET_KEY` | 腾讯云 SecretKey |
| `RUST_LOG` | 日志级别（`stfn=debug`） |

- 环境变量只覆盖对应字段，不影响其他配置。
- 设置窗口中，来自环境变量的字段只读，并显示来源。
- 文档与错误信息中不打印变量值。

## 6. 凭据文件（回退）

```toml
# credentials.toml
[providers.volcengine]
access_key = "..."
secret_key = "..."

[providers.tencent]
secret_id = "..."
secret_key = "..."
```

- 仅在 Secret Service 不可用且用户明确确认后写入。
- 权限 `0600`；目录 `0700`。
- 不生成 `.bak`，避免额外明文副本。
- 读取时若权限过宽：尝试 `chmod 0600`，失败则警告并继续读取。

## 7. Secret Service

- 通过 `oo7`（zbus）访问默认 collection。
- Item 属性：
  - `application = "stfn"`
  - `provider = "volcengine" | "tencent"`
  - `field = "access_key" | "secret_key" | "secret_id"`
- Label：`STFN · <Provider> <Field>`。
- 保存时按属性查找并替换；同一属性多条时取第一条并警告。
- 禁用 provider 不删除密钥；清空字段并保存才会删除。
- keyring 锁定：请求解锁；失败则按 `settings.md` 的流程提示用户。

## 8. 校验

| 规则 | 处理 |
| --- | --- |
| `version` 缺失 | 视为 1 |
| `version` > 1 | 警告，尽力按 1 读取 |
| 未知字段 | 忽略并记录 warn |
| `debounce_ms` 越界 | 加载时 clamp 到 200–2000，并警告 |
| `target` / `default_source` 非法 | 回退默认值并警告 |
| `region` 为空 | 回退该 provider 默认区域 |
| 启用 provider 但缺密钥 | 配置仍加载；请求时该行报“缺少凭据” |
| 所有 provider 关闭 | 允许；UI 显示空状态 |

- 配置错误不阻塞启动；以可用部分继续运行。
- 设置窗口保存时严格校验，越界直接阻止保存并就地提示。

## 9. 读写

### 9.1 读取

1. 读取 `config.toml`；不存在则创建最小配置。
2. 解析失败：原文件重命名为 `config.toml.bak`，使用默认值，记录 warn。
3. 读取密钥（env → keyring → 文件）；失败不阻塞启动，仅记录 warn。
4. 校验并 clamp。

### 9.2 写入

- 使用 `toml_edit` 读改写，保留注释、空行与字段顺序。
- 原子写：
  1. 同目录创建临时文件（`config.toml.tmp.<pid>`）。
  2. 写入并 `fsync`。
  3. `rename` 覆盖目标。
  4. `fsync` 目录。
- 失败时保留原文件，UI 显示错误，设置窗口保持 dirty。
- 配置目录权限 `0700`；`config.toml` `0644`；`credentials.toml` `0600`。

## 10. 迁移

- `version` 用于未来迁移；v0.1 只有版本 1。
- 迁移前备份为 `config.toml.v<old>.bak`。
- 迁移失败：保留备份，使用默认值，提示用户。
- 不迁移密钥；密钥存储格式由 Secret Service 属性 / 文件 schema 版本控制。

## 11. 故障处理

| 场景 | 行为 |
| --- | --- |
| 配置目录不可写 | 启动正常；保存时报错，设置窗口保持打开 |
| `config.toml` 损坏 | 备份 + 默认值 + 警告 |
| `credentials.toml` 损坏 | 忽略 + 警告；不使用其中任何字段 |
| keyring 不可用 | 设置窗口提示；用户确认后回退文件 |
| 环境变量拼写错误 | 不生效；不猜测别名 |
| 时钟偏差导致云 API 签名失败 | 错误提示检查系统时间 |

## 12. 示例

```toml
version = 1

[general]
auto_translate = false
debounce_ms = 500
target = "zh"
default_source = "bing"

[providers.bing]
enabled = true

[providers.volcengine]
enabled = true
region = "cn-north-1"

[providers.tencent]
enabled = true
region = "ap-guangzhou"
```

对应密钥通过环境变量提供：

```bash
export STFN_VOLC_ACCESS_KEY="..."
export STFN_VOLC_SECRET_KEY="..."
export STFN_TENCENT_SECRET_ID="..."
export STFN_TENCENT_SECRET_KEY="..."
```

## 13. 测试

- 临时 `XDG_CONFIG_HOME` 下读写。
- 注释 / 顺序在保存后保留。
- 权限：目录 0700、凭据 0600。
- 环境变量覆盖 keyring 与文件。
- keyring fake：写入、替换、读取、锁定失败。
- 损坏配置：备份 + 默认值。
- 迁移：v0 → v1 的 fixture。
