# 配置与密钥存储

> 状态：已按 v0.2.0 实现校准
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
debounce_ms = 700
target = "auto"            # auto | zh | en
default_source = "bing"    # bing | volcengine | alibaba | all
color_scheme = "default"   # default | light | dark

[providers.bing]
enabled = true

[providers.volcengine]
enabled = false

[providers.alibaba]
enabled = false
```

- 不保存区域：火山内部固定 `cn-north-1`，阿里内部固定 `cn-hangzhou`。
- 旧版本遗留的 `[providers.tencent]` 与 `region` 字段在保存时自动移除。

## 3. 默认值

| 字段 | 默认 | 说明 |
| --- | --- | --- |
| `version` | `1` | 配置格式版本 |
| `general.auto_translate` | `true` | 输入停止后自动翻译 |
| `general.debounce_ms` | `700` | 200–2000 |
| `general.target` | `"auto"` | `auto` 时按检测结果决定 |
| `general.default_source` | `"bing"` | CLI 与 GUI 的默认源 |
| `general.color_scheme` | `"default"` | `default` 跟随系统；`light`/`dark` 手动覆盖 |
| `providers.bing.enabled` | `true` | 无密钥 |
| `providers.volcengine.enabled` | `false` | 需要 Access Key / Secret Key |
| `providers.alibaba.enabled` | `false` | 需要 AccessKey ID / Secret |

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

- 环境变量优先级最高，且不会被写回任何存储。
- CLI 不写密钥；设置页负责写入。

## 5. 环境变量

| 变量 | 对应字段 |
| --- | --- |
| `STFN_VOLC_ACCESS_KEY` | 火山引擎 Access Key |
| `STFN_VOLC_SECRET_KEY` | 火山引擎 Secret Key |
| `STFN_ALI_ACCESS_KEY_ID` | 阿里云 AccessKey ID |
| `STFN_ALI_ACCESS_KEY_SECRET` | 阿里云 AccessKey Secret |
| `RUST_LOG` | 日志级别（`stfn=debug`） |

- 环境变量只覆盖对应字段，不影响其他配置。
- 设置页中，来自环境变量的字段只读，并显示“（环境变量）”。
- 文档与错误信息中不打印变量值。

## 6. 凭据文件（回退）

```toml
# credentials.toml
[providers.volcengine]
access_key = "..."
secret_key = "..."

[providers.alibaba]
access_key_id = "..."
access_key_secret = "..."
```

- 仅在 Secret Service 不可用且用户明确允许后写入。
- 权限 `0600`；目录 `0700`。
- 不生成 `.bak`，避免额外明文副本。
- 读取时若权限过宽：尝试 `chmod 0600`，失败则警告并继续读取。

## 7. Secret Service

- 通过 `keyring` crate（zbus Secret Service）访问默认 collection。
- Item 属性：`service = "stfn"`，`username = <field>`。
- 字段：`volcengine.access_key`、`volcengine.secret_key`、`alibaba.access_key_id`、`alibaba.access_key_secret`。
- 保存时按字段替换；同一字段多条时取第一条并警告。
- 禁用 provider 不删除密钥；清空输入并保存才删除。

## 8. 密钥显示语义

- 已保存：输入框显示 `******`（密码框显示圆点）。
- 不改动 `******`：保留。
- 清空并保存：删除。
- 输入新值：替换。
- 环境变量：只读，显示“（环境变量）”。

## 9. 校验

| 规则 | 处理 |
| --- | --- |
| `version` 缺失 | 视为 1 |
| `version` > 1 | 警告，尽力按 1 读取 |
| 未知字段 | 忽略并记录 warn |
| `debounce_ms` 越界 | 加载时 clamp 到 200–2000，并警告 |
| `target` / `default_source` 非法 | 回退默认值并警告 |
| `color_scheme` 非法 | 回退 `default` 并警告 |
| 启用 provider 但缺密钥 | 配置仍加载；请求时该行报“缺少凭据” |
| 所有 provider 关闭 | 允许；UI 显示空状态 |
| 旧 `tencent` / `region` 字段 | 读取时忽略；保存时移除 |

- 配置错误不阻塞启动；以可用部分继续运行。
- 设置页保存时严格校验，越界/缺密钥直接阻止保存并就地提示。

## 10. 读写

### 10.1 读取

1. 读取 `config.toml`；不存在则创建最小配置。
2. 解析失败：原文件重命名为 `config.toml.bak`，使用默认值，记录 warn。
3. 读取密钥（env → keyring → 文件）；失败不阻塞启动，仅记录 warn。
4. 校验并 clamp。

### 10.2 写入

- 使用 `toml_edit` 读改写，保留注释、空行与字段顺序。
- 原子写：
  1. 同目录创建临时文件（`.config.toml.tmp.<pid>`）。
  2. 写入并 `fsync`。
  3. `rename` 覆盖目标。
  4. `fsync` 目录。
- 失败时保留原文件，UI 显示错误，设置页保持 dirty。
- 配置目录权限 `0700`；`config.toml` `0644`；`credentials.toml` `0600`。

## 11. 迁移

- v0.1 仍使用 `version = 1`；旧配置中的 `providers.tencent` 与 `region` 被忽略。
- 保存时移除旧字段，写入 `providers.alibaba`。
- 旧腾讯云密钥不迁移；如存在请手动删除或忽略。
- 迁移失败：保留备份，使用默认值，提示用户。

## 12. 故障处理

| 场景 | 行为 |
| --- | --- |
| 配置目录不可写 | 启动正常；保存时报错，设置页保持打开 |
| `config.toml` 损坏 | 备份 + 默认值 + 警告 |
| `credentials.toml` 损坏 | 忽略 + 警告；不使用其中任何字段 |
| keyring 不可用 | 设置页提示；用户允许后回退文件 |
| keyring 写失败（锁定） | 保存失败，提示解锁；不静默写文件 |
| 环境变量拼写错误 | 不生效；不猜测别名 |
| 时钟偏差导致云 API 签名失败 | 错误提示检查系统时间 |

## 13. 示例

```toml
version = 1

[general]
auto_translate = false
debounce_ms = 700
target = "zh"
default_source = "bing"
color_scheme = "dark"

[providers.bing]
enabled = true

[providers.volcengine]
enabled = true

[providers.alibaba]
enabled = true
```

对应密钥通过环境变量提供：

```bash
export STFN_VOLC_ACCESS_KEY="..."
export STFN_VOLC_SECRET_KEY="..."
export STFN_ALI_ACCESS_KEY_ID="..."
export STFN_ALI_ACCESS_KEY_SECRET="..."
```

## 14. 测试

- 临时 `XDG_CONFIG_HOME` 下读写。
- 注释 / 顺序在保存后保留。
- 权限：目录 0700、凭据 0600。
- 环境变量覆盖 keyring 与文件。
- keyring fake：写入、替换、读取、锁定失败。
- 损坏配置：备份 + 默认值。
- 旧 `tencent` / `region` 字段迁移。
