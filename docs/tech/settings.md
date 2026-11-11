# 应用内设置页与密钥存储

> 状态：v0.1 已实现；v0.2 进程模型设计进行中
> 关联：`README.md`、`ui.md`、`config.md`、`architecture.md`、`selection.md`

## 1. 入口与形态

- 主窗口顶栏齿轮按钮。
- `Ctrl+,`。
- 命令行 `stfn settings`：**每次启动新进程**，构建主窗口并直接 push 设置页；不再复用已有实例。
- 设置页使用 `AdwNavigationView` 在主窗口内 push，不新开浮动窗口；窗口内 `Ctrl+,` 也进入设置页。
- 顶部栏：返回按钮、标题“设置”、保存按钮。

## 2. 布局

```
┌────────────────────────────────────────────┐
│  ‹   设置                          [保存]  │
├────────────────────────────────────────────┤
│  翻译                                      │
│  ┌──────────────────────────────────────┐  │
│  │ 自动翻译                      [开关] │  │
│  │ 延迟                      700 ms     │  │
│  └──────────────────────────────────────┘  │
│                                            │
│  翻译源                                    │
│  ┌──────────────────────────────────────┐  │
│  │ Bing                          [开关] │  │
│  │ 火山引擎                      [开关] │  │
│  │   Access Key  [••••••]  ✎ 👁         │  │
│  │   Secret Key  [••••••]  ✎ 👁         │  │
│  │ 阿里云                        [开关] │  │
│  │   AccessKey ID     [••••••]  ✎ 👁    │  │
│  │   AccessKey Secret [••••••]  ✎ 👁    │  │
│  └──────────────────────────────────────┘  │
└────────────────────────────────────────────┘
```

- 使用 `AdwPreferencesPage` + `AdwPreferencesGroup`。
- 火山 / 阿里使用 `AdwExpanderRow`：16px 厂商 logo + 开关 + 可折叠的密钥行；开关关闭时密钥行不展示。
- Bing 无密钥，只保留开关。
- 不提供区域配置；区域由后端固定（火山 `cn-north-1`，阿里 `cn-hangzhou`）。

## 3. 字段

| 字段 | 控件 | 默认 | 规则 |
| --- | --- | --- | --- |
| `general.auto_translate` | `AdwSwitchRow` | `true` | 关闭后主窗口显示“翻译”按钮 |
| `general.debounce_ms` | `AdwSpinRow` | `700` | 200–2000，步进 50 |
| `providers.bing.enabled` | `AdwSwitchRow` | `true` | 无密钥 |
| `providers.volcengine.enabled` | `AdwExpanderRow` 开关 | `false` | 开启时要求 Access Key / Secret Key |
| `providers.alibaba.enabled` | `AdwExpanderRow` 开关 | `false` | 开启时要求 AccessKey ID / Secret |
| 火山密钥 | `AdwPasswordEntryRow` | 空 | 见第 4 节 |
| 阿里密钥 | `AdwPasswordEntryRow` | 空 | 见第 4 节 |

## 4. 密钥显示与修改语义

已保存的密钥在输入框中显示为 `******`（密码框实际显示为圆点），标题显示“· 已保存”；清空显示“· 将删除”，输入新值显示“· 将更新”：

| 用户操作 | 保存结果 |
| --- | --- |
| 不改动 `******` | 保留原密钥 |
| 清空输入框并保存 | 删除该密钥 |
| 输入新值并保存 | 替换为新密钥 |
| 环境变量提供 | 输入框只读，显示“（环境变量）” |

- 聚焦 `******` 时全选，输入新值直接替换。
- 若在 `******` 后追加内容，保存时去掉前缀，按新值处理。
- 开启某厂商但密钥缺失/被清空时，保存会被拦截并提示。
- 密钥默认掩码；不写日志、不打印、不进入桌面通知、不写剪贴板。
- 环境变量优先且只读，不会被写回 keyring 或文件。

## 5. 密钥存储

优先级：

```
环境变量 > Secret Service > credentials.toml (0600) > 空
```

### 5.1 Secret Service

- 通过 `keyring` crate（底层 zbus Secret Service，纯 Rust）访问默认 collection。
- Item 的 `service = "stfn"`，`username` 为字段名：
  - `volcengine.access_key`
  - `volcengine.secret_key`
  - `alibaba.access_key_id`
  - `alibaba.access_key_secret`
- 保存时按字段替换；不产生重复条目。
- 写失败（如 keyring 锁定）时返回错误，不退化为文件。

### 5.2 文件回退

- 路径：`$XDG_CONFIG_HOME/stfn/credentials.toml`（默认 `~/.config/stfn/credentials.toml`）。
- 目录权限 `0700`，文件权限 `0600`。
- 仅在 Secret Service 不可用、且用户打开“允许保存到本地文件”开关后才写入。
- 写入方式：`toml_edit` + 临时文件 + `rename`。
- 不保留凭据文件的 `.bak`。

```toml
# credentials.toml
[providers.volcengine]
access_key = "..."
secret_key = "..."

[providers.alibaba]
access_key_id = "..."
access_key_secret = "..."
```

## 6. 保存流程

1. 读取所有控件值，校验延迟、启用平台的密钥完整性。
2. 若有新密钥且密钥环不可用，要求“允许保存到本地文件”已打开。
3. 按第 4 节语义写入 / 删除 / 保留密钥。
4. 用 `toml_edit` 读改写 `config.toml`，保留注释。
5. 原子写：同目录临时文件 → `fsync` → `rename`。
6. 更新内存配置与凭据，主窗口重建结果行。
7. 清除 dirty 状态，返回主页面。

- 保存失败时保持设置页打开，并在“状态”组显示脱敏错误。
- 保存成功后不保留设置页实例。

## 7. 未保存修改

- 任一开关、延迟、密钥发生变化即标记 dirty。
- 点击返回、按 `Esc`、关闭窗口时：
  - dirty → `AdwAlertDialog`“放弃未保存的修改？”（取消 / 放弃）。
  - 不 dirty → 直接返回 / 关闭。
- 选择“放弃”后：返回主页面；若是关闭窗口则退出应用。
- 保存成功后 dirty 清除，不再弹确认。

## 8. 校验规则

- `debounce_ms` ∈ [200, 2000]。
- 开启火山引擎：Access Key、Secret Key 必须已保存或本次填写。
- 开启阿里云：AccessKey ID、AccessKey Secret 必须已保存或本次填写。
- 无区域字段。
- 至少启用一个翻译源；全部关闭时保存前提示“将没有可用的翻译源”，允许保存但主窗口显示空状态。

## 9. 测试连接（P1，未实现）

- 每个厂商可提供“测试连接”，使用当前输入值或环境变量发送 `hello`，超时 5s。
- 成功显示延迟；失败显示脱敏错误。
- 不保存配置、不写密钥。

## 10. 验收清单

- [x] 齿轮 / `Ctrl+,` / `stfn settings` 都能切到设置页，不新开窗口
- [x] 自动翻译与延迟可调
- [x] 厂商开关打开后才显示密钥行
- [x] 已保存密钥显示 `******`；不改动保留，清空删除，新值替换
- [x] 环境变量密钥只读
- [x] 开启厂商但缺密钥时保存被拦截
- [x] 修改未保存返回 / 退出时弹确认
- [x] 无区域配置
- [x] 阿里云已接入
- [x] keyring 不可用时需明确允许才写 0600 文件
