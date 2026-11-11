# 翻译源设计

> 状态：待审核 · 目标版本：v0.1（朗读 v1.1）
> 关联：`architecture.md`、`config.md`、`settings.md`

## 1. 统一接口

```rust
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> ProviderId;
    fn capabilities(&self) -> Capabilities;
    async fn translate(&self, req: &TranslateRequest) -> Result<TranslateOutput, ProviderError>;
    // v1.1
    async fn synthesize(&self, _text: &str, _lang: Lang) -> Result<Audio, ProviderError> {
        Err(ProviderError::unsupported("该平台不支持朗读"))
    }
}

pub struct Capabilities {
    pub phonetics: bool,
    pub tts: bool,
    pub auto_detect: bool,
}
```

- `TranslateRequest { text, from, to }`。
- `TranslateOutput { text, detected, phonetics, audio }`。
- provider 只负责协议与解析；不做去抖、缓存、UI。
- 所有 provider 共用 `reqwest::Client`（连接池、rustls、超时）。

## 2. 能力矩阵

| 能力 | Bing | 火山引擎 | 腾讯云 TMT |
| --- | --- | --- | --- |
| 中→英 / 英→中 | ✅ | ✅ | ✅ |
| 自动识别源语言 | ✅ | 待实测 | ✅ |
| 音标 | ❌（v1 不承诺） | ❌ | ❌ |
| 朗读 | v1.1（Edge TTS，非官方） | 需另开语音合成 | 需另开语音合成 |
| 免费额度 | 无官方额度（非官方） | 以控制台账单为准 | 每月免费字符（以官网为准） |
| 官方 Rust SDK | 无 | 无 | 无 |

> 音标、朗读在 v1.1 前一律返回 `None` / `Unsupported`；UI 不显示对应控件。

## 3. Bing（默认，免密钥）

### 3.1 认证

- `GET https://edge.microsoft.com/translate/auth`
- 响应体为 JWT 字符串。
- 解析 JWT payload 的 `exp`，在到期前 60s 刷新。
- 401 时强制刷新一次并重试一次。
- Token 只缓存在内存，不落盘。

### 3.2 翻译

```http
POST https://api-edge.cognitive.microsofttranslator.com/translate?api-version=3.0&from=en&to=zh-Hans
Authorization: Bearer <token>
Content-Type: application/json

[{"Text":"hello"}]
```

响应：

```json
[
  {
    "detectedLanguage": { "language": "en", "score": 1.0 },
    "translations": [{ "text": "你好", "to": "zh-Hans" }]
  }
]
```

- 源语言由本地检测决定，显式传 `from`，不依赖远端自动识别。
- 目标语言：中文用 `zh-Hans`，英文用 `en`。
- 文本按数组单条发送；v1 不批量。
- `User-Agent` 使用固定、真实的浏览器 UA；不随版本变化。

### 3.3 音标 / 词典

- 可选接口：`/dictionary/lookup?api-version=3.0&from=en&to=zh-Hans`。
- 仅对单个英文词有意义；返回词性、置信度、回译，**不保证音标**。
- v1 不调用词典接口，`phonetics = None`；v1.1 再评估是否值得接入。
- 不从网页版 Bing 词典抓 HTML（易碎且不合规）。

### 3.4 朗读（v1.1）

- Edge read-aloud 为非官方 WebSocket 接口，需 SSML 与音频分片处理。
- 优先使用维护良好的 Rust 封装；没有则暂缓。
- 失败时静默隐藏播放按钮；`--speak` 返回非零并提示“朗读不可用”。

### 3.5 风险

- 非官方接口，可能随时变更或封禁；无 SLA。
- 不内置重试风暴：401 刷新一次、5xx 最多重试一次。
- UI 与 README 明确标注“非官方”。

## 4. 火山引擎机器翻译

### 4.1 接口

- 产品：机器翻译（Machine Translation）。
- Action：`TranslateText`。
- Version：`2020-06-01`（实现前以官方文档复核）。
- Endpoint：`https://translate.volcengineapi.com`。
- Region：默认 `cn-north-1`，可在设置中改。

请求：

```json
{
  "SourceLanguage": "en",
  "TargetLanguage": "zh",
  "Text": ["hello"]
}
```

响应（示意）：

```json
{
  "ResponseMetadata": { "RequestId": "..." },
  "TranslationList": [
    { "Translation": "你好", "DetectedLanguage": "en" }
  ]
}
```

### 4.2 鉴权（Signature V4）

1. 计算 payload SHA-256。
2. 构造 canonical request：method、path `/`、query（`Action`、`Version`）、headers（`content-type`、`host`、`x-content-sha256`、`x-date`）、signed headers。
3. 计算 string to sign：`HMAC-SHA256`、`X-Date`、credential scope（date/region/service/request）、canonical request hash。
4. 派生 signing key 并计算 signature。
5. 组装 `Authorization: HMAC-SHA256 Credential=<AK>/<scope>, SignedHeaders=..., Signature=...`。

- 使用 `hmac` + `sha2` + `hex` 实现，不引入官方 SDK（无 Rust 版）。
- `X-Date` 使用 UTC 基本格式（`YYYYMMDDTHHMMSSZ`）。
- 时钟偏差 > 5 分钟会失败；错误信息提示检查系统时间。

### 4.3 错误

| 情况 | 映射 |
| --- | --- |
| `InvalidCredential` / 签名错误 | `Auth` |
| 配额 / QPS 超限 | `RateLimit` |
| 5xx | `Server`（可重试一次） |
| 超时 | `Timeout` |
| 响应字段缺失 | `Parse` |

- 具体错误码以官方文档为准；实现时集中映射，UI 只展示脱敏文案。

## 5. 腾讯云 TMT

### 5.1 接口

- Action：`TextTranslate`。
- Version：`2018-03-21`。
- Endpoint：`https://tmt.tencentcloudapi.com`。
- Region：默认 `ap-guangzhou`。

请求：

```json
{
  "SourceText": "hello",
  "Source": "auto",
  "Target": "zh",
  "ProjectId": 0
}
```

响应：

```json
{
  "Response": {
    "TargetText": "你好",
    "Source": "en",
    "Target": "zh",
    "RequestId": "..."
  }
}
```

- 源语言：本地检测后显式传 `en` / `zh`；无法检测时传 `auto`。
- 目标语言：`zh` / `en`。

### 5.2 鉴权（TC3-HMAC-SHA256）

1. 拼接 canonical request（`content-type`、`host`、`x-tc-action` 等）。
2. 构造 string to sign（算法、时间戳、credential scope、hash）。
3. 派生 signing key（`TC3` + SecretKey → date → service → `tc3_request`）。
4. 组装 `Authorization` 与 `X-TC-*` 头：
   - `X-TC-Action: TextTranslate`
   - `X-TC-Version: 2018-03-21`
   - `X-TC-Timestamp`
   - `X-TC-Region`
- 使用 `hmac` + `sha2` + `hex` 实现。

### 5.3 错误

| 情况 | 映射 |
| --- | --- |
| `AuthFailure.*` | `Auth` |
| `RequestLimitExceeded` | `RateLimit` |
| `InternalError` | `Server`（可重试一次） |
| 超时 | `Timeout` |
| 响应字段缺失 | `Parse` |

- 免费额度、QPS 以腾讯云官网当期政策为准；README 不写死数字。

## 6. 语言检测

纯本地、无网络：

```
trim → 空？ => Empty
含平假名 / 片假名 / 谚文？ => Unsupported
含 CJK 统一表意文字？ => Zh
含 ASCII 字母？ => En
其余 => Unsupported
```

- CJK 范围：`4E00–9FFF`、`3400–4DBF`、`F900–FAFF`、扩展 B 及以上（按需）。
- 混排中英按中文处理（目标英文）。
- `Zh → En`，`En → Zh-Hans`。
- `--to` 可强制目标语言；源语言仍按检测结果，不支持则报错。
- 纯数字 / 符号 / 空白：不请求，CLI 返回 2。

## 7. 并发、超时、重试

- 最多 3 个 provider 并发（当前数量上限）。
- 超时：Bing 5s；火山 / 腾讯 8s。
- 重试：网络错误 / 5xx 最多 1 次，退避 300ms + 抖动；4xx、认证、限流不重试。
- Bing token 401：刷新 token 后重试 1 次（不计入上面的 5xx 重试）。
- 新输入到来时丢弃旧 generation 的结果；可选 abort 未完成请求。
- 限流错误直接展示，不自动排队。

## 8. 缓存

- 仅内存；容量 128；TTL 10 分钟。
- Key：`(provider, text, from, to)`。
- 只缓存成功结果；错误不缓存。
- 进程退出即清空。

## 9. 测试

- 每个 provider 的 mock 响应 fixture：成功、401、429、超时、畸形 JSON。
- 签名黄金测试：固定 AK/SK、时间戳、请求体 → 期望 `Authorization` 字符串。
- CLI E2E 使用 `STFN_*_BASE_URL`（仅 debug 构建）指向本地 mock。
- 真实接口 smoke test 标记 `#[ignore]`，手动运行。

## 10. 待实测 / 待确认

- Bing 翻译接口与 token 端点的当前可用性、UA 要求。
- Bing 词典是否值得接入音标。
- Edge TTS 的 Rust 封装成熟度。
- 火山 `TranslateText` 的字段名、自动识别参数、错误码。
- 腾讯免费额度与 QPS 的当期数值。
- 三家在长文本（>1000 字）下的截断 / 报错行为。
