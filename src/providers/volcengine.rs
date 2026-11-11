//! 火山引擎机器翻译 provider（TranslateText / Signature V4）。
//!
//! 区域固定为 cn-north-1，不在 UI 暴露；签名使用 HMAC-SHA256 派生
//! kDate -> kRegion -> kService -> kSigning。
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use time::macros::format_description;

use crate::lang::Lang;
use crate::translate::{Capabilities, Provider, ProviderError, TranslateOutput, TranslateRequest};

type HmacSha256 = Hmac<Sha256>;

const BASE_URL: &str = "https://translate.volcengineapi.com";
const SERVICE: &str = "translate";
const ACTION: &str = "TranslateText";
const VERSION: &str = "2020-06-01";

/// 火山 provider：凭据、固定区域和 HTTP client。
pub struct Volcengine {
    client: reqwest::Client,
    access_key: String,
    secret_key: String,
    region: String,
    base_url: String,
    host: String,
}

// 火山私有实现：Signature V4 签名。
impl Volcengine {
    /// 允许 STFN_VOLC_BASE_URL 覆盖，区域固定 cn-north-1。
    pub fn new(client: reqwest::Client, access_key: String, secret_key: String) -> Self {
        let base_url = std::env::var("STFN_VOLC_BASE_URL").unwrap_or_else(|_| BASE_URL.to_string());
        let host = reqwest::Url::parse(&base_url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
            .unwrap_or_else(|| "translate.volcengineapi.com".to_string());
        Self {
            client,
            access_key,
            secret_key,
            region: "cn-north-1".to_string(),
            base_url,
            host,
        }
    }

    /// 构造 canonical request / string to sign / signing key，返回 Authorization 与时间头。
    fn sign(&self, body: &str) -> Result<(String, String, String), ProviderError> {
        let now = time::OffsetDateTime::now_utc();
        let x_date = now
            .format(format_description!(
                "[year][month][day]T[hour][minute][second]Z"
            ))
            .map_err(|e| ProviderError::parse(format!("生成签名时间失败：{e}")))?;
        let short_date = &x_date[..8];
        let payload_hash = hex::encode(Sha256::digest(body.as_bytes()));

        let query = format!("Action={ACTION}&Version={VERSION}");
        let canonical_headers = format!(
            "content-type:application/json\nhost:{}\nx-content-sha256:{}\nx-date:{}\n",
            self.host, payload_hash, x_date
        );
        let signed_headers = "content-type;host;x-content-sha256;x-date";
        let canonical_request = format!(
            "POST\n/\n{}\n{}\n{}\n{}",
            query, canonical_headers, signed_headers, payload_hash
        );
        let canonical_hash = hex::encode(Sha256::digest(canonical_request.as_bytes()));
        let scope = format!("{}/{}/{}/request", short_date, self.region, SERVICE);
        let string_to_sign = format!("HMAC-SHA256\n{}\n{}\n{}", x_date, scope, canonical_hash);

        let k_date = hmac_sha256(self.secret_key.as_bytes(), short_date.as_bytes());
        let k_region = hmac_sha256(&k_date, self.region.as_bytes());
        let k_service = hmac_sha256(&k_region, SERVICE.as_bytes());
        let k_signing = hmac_sha256(&k_service, b"request");
        let signature = hex::encode(hmac_sha256(&k_signing, string_to_sign.as_bytes()));

        let authorization = format!(
            "HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
            self.access_key, scope, signed_headers, signature
        );

        Ok((authorization, x_date, payload_hash))
    }
}

#[async_trait]
// Provider 实现：请求体字段是 TextList，响应读取 DetectedSourceLanguage。
impl Provider for Volcengine {
    fn id(&self) -> &'static str {
        "volcengine"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            phonetics: false,
            tts: false,
            auto_detect: true,
        }
    }

    /// 发送 TranslateText；响应为空时从 ResponseMetadata.Error 提取原因。
    async fn translate(
        &self,
        request: &TranslateRequest,
    ) -> Result<TranslateOutput, ProviderError> {
        let body = serde_json::json!({
            "SourceLanguage": request.from.code(),
            "TargetLanguage": request.to.code(),
            "TextList": [request.text],
        })
        .to_string();

        let (authorization, x_date, payload_hash) = self.sign(&body)?;
        let url = format!("{}/?Action={ACTION}&Version={VERSION}", self.base_url);
        let response = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("X-Date", x_date)
            .header("X-Content-Sha256", payload_hash)
            .header(reqwest::header::AUTHORIZATION, authorization)
            .body(body)
            .send()
            .await
            .map_err(map_reqwest_error)?;

        if !response.status().is_success() {
            return Err(status_error(response.status()));
        }

        let bytes = response.bytes().await.map_err(map_reqwest_error)?;
        let parsed: VolcResponse = serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::parse(format!("火山引擎响应解析失败：{e}")))?;

        if let Some(items) = parsed.translation_list {
            if let Some(item) = items.into_iter().next() {
                let text = item.translation.unwrap_or_default();
                if !text.is_empty() {
                    return Ok(TranslateOutput {
                        text,
                        detected: item.detected_language.as_deref().and_then(Lang::from_code),
                        phonetics: None,
                        audio: None,
                        latency_ms: None,
                    });
                }
            }
        }

        Err(volc_error(parsed.metadata.as_ref()))
    }
}

// HMAC-SHA256 小工具。
fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key size");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

#[derive(Debug, Deserialize)]
// 火山响应：TranslationList 与通用元数据。
struct VolcResponse {
    #[serde(rename = "TranslationList")]
    translation_list: Option<Vec<VolcItem>>,
    #[serde(rename = "ResponseMetadata")]
    metadata: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
// 单条翻译结果。
struct VolcItem {
    #[serde(rename = "Translation")]
    translation: Option<String>,
    #[serde(rename = "DetectedSourceLanguage")]
    detected_language: Option<String>,
}

// 把火山错误码/消息映射为统一错误分类。
fn volc_error(metadata: Option<&serde_json::Value>) -> ProviderError {
    let error = metadata.and_then(|m| m.get("Error"));
    let code = error
        .and_then(|e| e.get("Code"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let message = error
        .and_then(|e| e.get("Message"))
        .and_then(|v| v.as_str())
        .unwrap_or("火山引擎未返回译文");

    if code.contains("Credential") || code.contains("Signature") || code.contains("Auth") {
        ProviderError::auth(format!("火山引擎认证失败：{message}"))
    } else if code.contains("Quota") || code.contains("Limit") {
        ProviderError::rate_limit(format!("火山引擎请求受限：{message}"))
    } else {
        ProviderError::server(format!("火山引擎返回错误：{message}"))
    }
}

// HTTP 状态 -> 统一错误分类。
fn status_error(status: reqwest::StatusCode) -> ProviderError {
    match status.as_u16() {
        401 | 403 => ProviderError::auth("火山引擎认证失败，请检查密钥"),
        429 => ProviderError::rate_limit("火山引擎请求过于频繁，请稍后再试"),
        500..=599 => ProviderError::server(format!("火山引擎服务异常（{}）", status.as_u16())),
        _ => ProviderError::parse(format!("火山引擎请求被拒绝（{}）", status.as_u16())),
    }
}

// 网络错误统一映射，超时单独分类以便重试。
fn map_reqwest_error(error: reqwest::Error) -> ProviderError {
    if error.is_timeout() {
        ProviderError::timeout()
    } else {
        ProviderError::network(format!("网络错误：{error}"))
    }
}
