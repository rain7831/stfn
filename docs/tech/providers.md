# 翻译源设计

> 状态：v0.2.0 已发布；音标/发音为应用侧后处理
> 关联：`architecture.md`、`config.md`、`settings.md`

## 1. 统一接口

```rust
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    async fn translate(&self, req: &TranslateRequest) -> Result<TranslateOutput, ProviderError>;
}
```

- `TranslateRequest { text, from, to }`。
- `TranslateOutput { text, detected, latency_ms }`；音标/音频由 `pronunciation` 后处理。
- provider 只负责协议与解析；不做去抖、缓存、UI。
- 所有 provider 共用 `reqwest::Client`（连接池、rustls、超时、Bing 需要 cookie store）。

## 2. 能力矩阵

| 能力 | Bing | 火山引擎 | 阿里云 |
| --- | --- | --- | --- |
| 中→英 / 英→中 | ✅ | ✅ | ✅ |
| 自动识别源语言 | ✅ | ✅（可显式传源语言） | ✅ |
| 音标（英文） | ❌（v0.2 由词典后处理） | ❌ | ❌ |
| 朗读 | ❌（应用侧后处理） | ❌（应用侧后处理） | ❌（应用侧后处理） |
| 凭据 | 无 | Access Key / Secret Key | AccessKey ID / Secret |
| 区域配置 | 无 | 无（内部固定 cn-north-1） | 无（内部固定 cn-hangzhou） |

> provider 不负责英文 IPA：Bing 只返回中文拼音 transliteration，火山/阿里只返回译文。v0.2 的英文单词音标/音频由 `pronunciation.md` 的词典服务后处理；provider 不返回 `phonetics`。

## 3. Bing（默认，免密钥）

### 3.1 会话获取

Bing 的旧 `edge.microsoft.com/translate/auth` JWT 端点已不可用。当前实现走网页翻译会话：

1. `GET https://www.bing.com/translator`（失败时回退 `https://cn.bing.com/translator`）。
2. 从 HTML 提取：
   - `IG:"..."`
   - `data-iid="..."`
   - `params_AbusePreventionHelper = [key, "token", ttlMs]`
3. 使用带 cookie store 的 HTTP 客户端，保留页面下发的 cookie。
4. 会话缓存到 TTL 到期前 60s；401/429 时刷新一次并重试一次。

### 3.2 翻译

```http
POST https://www.bing.com/ttranslatev3?isVertical=1&&IG=<IG>&IID=<IID>
Content-Type: application/x-www-form-urlencoded
Origin: https://www.bing.com
Referer: https://www.bing.com/translator

fromLang=en&to=zh-Hans&text=hello&token=<token>&key=<key>&tryFetchingGenderDebiasedTranslations=true
```

响应是数组：

```json
[
  {
    "translations": [{ "text": "你好", "to": "zh-Hans" }],
    "detectedLanguage": { "language": "en" }
  }
]
```

- 中文输入时数组可能包含第二个元素 `{"inputTransliteration": "Nǐ hǎo"}`，解析时跳过没有 `translations` 的元素。
- 源语言由本地检测决定，显式传 `fromLang`；`detected` 使用本地检测结果，不依赖远端误判。
- 目标语言：中文 `zh-Hans`，英文 `en`。
- 使用固定 Edge 浏览器 UA；不随版本变化。

### 3.3 音标 / 朗读

- Bing 的 `translations[].transliteration` 是中文拼音（如 `Nǐ hǎo`），不是英文 IPA；`inputTransliteration` 也是中文输入拼音。
- v0.2 不把 Bing transliteration 当英文音标使用；英文音标由 `dictionaryapi.dev` 后处理。
- 单词音频顺序：词典真人音频 -> 有道 -> Edge TTS；句子朗读：Edge TTS -> 有道兜底。
- `--speak` 使用 Edge TTS，失败返回非零；不阻塞已输出的翻译结果。

### 3.4 风险

- 网页会话接口非官方，页面结构变化会导致失效；无 SLA。
- 不做重试风暴：会话刷新一次、5xx 最多重试一次。
- README 与 UI 中明确标注“非官方”。

## 4. 火山引擎机器翻译

### 4.1 接口

- Action：`TranslateText`。
- Version：`2020-06-01`。
- Endpoint：`https://translate.volcengineapi.com`。
- Host：`translate.volcengineapi.com`。
- Service：`translate`。
- Region：内部固定 `cn-north-1`，UI 与配置不暴露区域。

请求：

```json
{
  "SourceLanguage": "en",
  "TargetLanguage": "zh",
  "TextList": ["hello"]
}
```

> 字段是 `TextList`，不是 `Text`；`SourceLanguage` 可省略表示自动检测，但我们通常显式传入。

响应（实测）：

```json
{
  "TranslationList": [
    {
      "Translation": "你好",
      "DetectedSourceLanguage": "",
      "Extra": { "input_characters": "5", "source_language": "en" }
    }
  ],
  "ResponseMetadata": {
    "RequestId": "202611111602315100C8C3A8668177A84F",
    "Action": "TranslateText",
    "Version": "2020-06-01",
    "Service": "translate",
    "Region": "cn-north-1"
  }
}
```

### 4.2 鉴权（Signature V4）

1. 计算 payload SHA-256。
2. 构造 canonical request：method、path `/`、query（`Action`、`Version`）、headers（`content-type`、`host`、`x-content-sha256`、`x-date`）、signed headers。
3. 计算 string to sign：`HMAC-SHA256`、`X-Date`、credential scope（date/region/service/request）、canonical request hash。
4. 派生 signing key：`kDate = HMAC(secret, date)` → region → service → `request`。
5. 组装 `Authorization: HMAC-SHA256 Credential=<AK>/<scope>, SignedHeaders=..., Signature=...`。

- 使用 `hmac` + `sha2` + `hex` 实现，不引入官方 SDK（无 Rust 版）。
- `X-Date` 使用 UTC 基本格式（`YYYYMMDDTHHMMSSZ`）。
- 时钟偏差 > 5 分钟会失败；错误信息提示检查系统时间。

### 4.3 错误

| 情况 | 映射 |
| --- | --- |
| 认证/签名错误 | `Auth` |
| 配额 / QPS 超限 | `RateLimit` |
| 5xx | `Server`（可重试一次） |
| 超时 | `Timeout` |
| 响应字段缺失 | `Parse` |

## 5. 阿里云机器翻译通用版

### 5.1 接口

- Action：`TranslateGeneral`。
- Version：`2018-10-12`。
- Endpoint：`https://mt.cn-hangzhou.aliyuncs.com/`。
- Region：内部固定 `cn-hangzhou`，UI 与配置不暴露区域。
- 协议：RPC 风格，POST form。

请求参数：

| 参数 | 值 |
| --- | --- |
| `AccessKeyId` | 用户 AccessKey ID |
| `Action` | `TranslateGeneral` |
| `Format` | `JSON` |
| `FormatType` | `text` |
| `RegionId` | `cn-hangzhou` |
| `Scene` | `general` |
| `SignatureMethod` | `HMAC-SHA1` |
| `SignatureNonce` | 每次随机 UUID |
| `SignatureVersion` | `1.0` |
| `SourceLanguage` | `en` / `zh` |
| `SourceText` | 原文 |
| `TargetLanguage` | `en` / `zh` |
| `Timestamp` | UTC ISO8601，如 `2026-11-11T16:02:31Z` |
| `Version` | `2018-10-12` |

### 5.2 签名（HMAC-SHA1）

1. 所有参数（不含 `Signature`）按参数名排序。
2. 对 key/value 做 RFC3986 编码，空格编码为 `%20`，`*` 编码为 `%2A`，`~` 保留。
3. `CanonicalizedQueryString = key=value&...`。
4. `StringToSign = "POST" + "&%2F&" + percentEncode(CanonicalizedQueryString)`。
5. `Signature = Base64(HMAC-SHA1(AccessKeySecret + "&", StringToSign))`。
6. `Signature` 加入 form body 一起 POST。

- 不引入阿里云 SDK；签名直接用 `hmac` + `sha1` + `base64`。
- 免费额度：每月前 100 万字符（以阿里云当期政策为准）。

### 5.3 响应

```json
{
  "Code": 200,
  "Message": "success",
  "RequestId": "86D18195-D89C-4C8C-9DC4-5FCE789CE6D5",
  "Data": {
    "Translated": "Hello",
    "WordCount": "10",
    "DetectedLanguage": "zh"
  }
}
```

- 兼容 `TranslateGeneralResponse` 包裹层。
- 错误码：10005 语向不支持、10008 字符过长、10009–10013 权限/服务/欠费，分别映射到 `Unsupported` / `Parse` / `Auth`。

## 6. 语言检测

纯本地、无网络：

```
trim → 空？ => Empty
含平假名 / 片假名 / 谚文？ => Unsupported
含 CJK 统一表意文字？ => Zh
含 ASCII 字母？ => En
其余 => Unsupported
```

- `Zh → En`，`En → Zh`。
- 混排中英按中文处理（目标英文）。
- 纯数字 / 符号 / 空白：不请求，CLI 返回 2。

## 7. 并发、超时、重试

- 最多 3 个 provider 并发。
- 超时：Bing 8s；火山 / 阿里 8s。
- 重试：网络错误 / 5xx 最多 1 次，退避 300ms；4xx、认证、限流不重试。
- Bing 会话 401/429：刷新会话后重试 1 次。
- 新输入到来时丢弃旧 generation 的结果。

## 8. 缓存

- 仅内存；容量 128；TTL 10 分钟。
- Key：`(provider, text, from, to)`。
- 只缓存成功结果；进程退出即清空。

## 9. 测试

- Bing mock：页面 HTML + 翻译响应 + 中文输入第二数组元素。
- 阿里云 mock：`Code=200` 响应；校验 CLI 与解析；真实密钥已验证。
- 火山需真实密钥做 smoke test（已在 v0.1 实测通过）。
- 签名黄金测试：固定 AK/SK、时间戳、请求体 → 期望 `Authorization`（后续补充）。

## 10. 待实测 / 待确认

- Bing 网页会话结构变化、UA 要求。
- 阿里云免费额度与长文本行为。
- Edge TTS 非官方且偶发 403；已加一次重试与有道兜底。
- 长文本（>1000 字）在各平台的截断 / 报错行为。
