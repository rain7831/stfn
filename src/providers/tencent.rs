use async_trait::async_trait;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use time::macros::format_description;

use crate::lang::Lang;
use crate::translate::{Capabilities, Provider, ProviderError, TranslateOutput, TranslateRequest};

type HmacSha256 = Hmac<Sha256>;

const BASE_URL: &str = "https://tmt.tencentcloudapi.com";
const SERVICE: &str = "tmt";
const ACTION: &str = "TextTranslate";
const VERSION: &str = "2018-03-21";

pub struct Tencent {
    client: reqwest::Client,
    secret_id: String,
    secret_key: String,
    region: String,
    base_url: String,
    host: String,
}

impl Tencent {
    pub fn new(
        client: reqwest::Client,
        secret_id: String,
        secret_key: String,
        region: String,
    ) -> Self {
        let base_url =
            std::env::var("STFN_TENCENT_BASE_URL").unwrap_or_else(|_| BASE_URL.to_string());
        let host = reqwest::Url::parse(&base_url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
            .unwrap_or_else(|| "tmt.tencentcloudapi.com".to_string());
        Self {
            client,
            secret_id,
            secret_key,
            region,
            base_url,
            host,
        }
    }

    fn sign(&self, body: &str) -> Result<(String, i64, String), ProviderError> {
        let now = time::OffsetDateTime::now_utc();
        let timestamp = now.unix_timestamp();
        let date = now
            .format(format_description!("[year]-[month]-[day]"))
            .map_err(|e| ProviderError::parse(format!("生成签名日期失败：{e}")))?;
        let payload_hash = hex::encode(Sha256::digest(body.as_bytes()));

        let canonical_headers = format!(
            "content-type:application/json; charset=utf-8\nhost:{}\nx-tc-action:{}\n",
            self.host,
            ACTION.to_ascii_lowercase()
        );
        let signed_headers = "content-type;host;x-tc-action";
        let canonical_request = format!(
            "POST\n/\n\n{}\n{}\n{}",
            canonical_headers, signed_headers, payload_hash
        );
        let canonical_hash = hex::encode(Sha256::digest(canonical_request.as_bytes()));
        let scope = format!("{}/{}/{}/tc3_request", date, self.region, SERVICE);
        let string_to_sign = format!(
            "TC3-HMAC-SHA256\n{}\n{}\n{}",
            timestamp, scope, canonical_hash
        );

        let secret_date = hmac_sha256(
            format!("TC3{}", self.secret_key).as_bytes(),
            date.as_bytes(),
        );
        let secret_service = hmac_sha256(&secret_date, SERVICE.as_bytes());
        let secret_signing = hmac_sha256(&secret_service, b"tc3_request");
        let signature = hex::encode(hmac_sha256(&secret_signing, string_to_sign.as_bytes()));

        let authorization = format!(
            "TC3-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
            self.secret_id, scope, signed_headers, signature
        );

        Ok((authorization, timestamp, date))
    }
}

#[async_trait]
impl Provider for Tencent {
    fn id(&self) -> &'static str {
        "tencent"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            phonetics: false,
            tts: false,
            auto_detect: true,
        }
    }

    async fn translate(
        &self,
        request: &TranslateRequest,
    ) -> Result<TranslateOutput, ProviderError> {
        let body = serde_json::json!({
            "SourceText": request.text,
            "Source": request.from.code(),
            "Target": request.to.code(),
            "ProjectId": 0,
        })
        .to_string();

        let (authorization, timestamp, _date) = self.sign(&body)?;
        let response = self
            .client
            .post(&self.base_url)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/json; charset=utf-8",
            )
            .header("X-TC-Action", ACTION)
            .header("X-TC-Version", VERSION)
            .header("X-TC-Timestamp", timestamp.to_string())
            .header("X-TC-Region", &self.region)
            .header(reqwest::header::AUTHORIZATION, authorization)
            .body(body)
            .send()
            .await
            .map_err(map_reqwest_error)?;

        if !response.status().is_success() {
            return Err(status_error(response.status()));
        }

        let bytes = response.bytes().await.map_err(map_reqwest_error)?;
        let parsed: TencentResponse = serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::parse(format!("腾讯云响应解析失败：{e}")))?;

        if let Some(error) = parsed.response.error {
            return Err(tencent_error(&error.code, &error.message));
        }

        let text = parsed
            .response
            .target_text
            .filter(|text| !text.is_empty())
            .ok_or_else(|| ProviderError::parse("腾讯云未返回译文"))?;

        Ok(TranslateOutput {
            text,
            detected: parsed.response.source.as_deref().and_then(Lang::from_code),
            phonetics: None,
            audio: None,
            latency_ms: None,
        })
    }
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key size");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

#[derive(Debug, Deserialize)]
struct TencentResponse {
    #[serde(rename = "Response")]
    response: TencentInner,
}

#[derive(Debug, Deserialize)]
struct TencentInner {
    #[serde(rename = "TargetText")]
    target_text: Option<String>,
    #[serde(rename = "Source")]
    source: Option<String>,
    #[serde(rename = "Error")]
    error: Option<TencentError>,
}

#[derive(Debug, Deserialize)]
struct TencentError {
    #[serde(rename = "Code")]
    code: String,
    #[serde(rename = "Message")]
    message: String,
}

fn tencent_error(code: &str, message: &str) -> ProviderError {
    if code.starts_with("AuthFailure") {
        ProviderError::auth(format!("腾讯云认证失败：{message}"))
    } else if code.contains("LimitExceeded") {
        ProviderError::rate_limit(format!("腾讯云请求受限：{message}"))
    } else if code.starts_with("InternalError") {
        ProviderError::server(format!("腾讯云服务异常：{message}"))
    } else {
        ProviderError::server(format!("腾讯云返回错误：{message}"))
    }
}

fn status_error(status: reqwest::StatusCode) -> ProviderError {
    match status.as_u16() {
        401 | 403 => ProviderError::auth("腾讯云认证失败，请检查密钥"),
        429 => ProviderError::rate_limit("腾讯云请求过于频繁，请稍后再试"),
        500..=599 => ProviderError::server(format!("腾讯云服务异常（{}）", status.as_u16())),
        _ => ProviderError::parse(format!("腾讯云请求被拒绝（{}）", status.as_u16())),
    }
}

fn map_reqwest_error(error: reqwest::Error) -> ProviderError {
    if error.is_timeout() {
        ProviderError::timeout()
    } else {
        ProviderError::network(format!("网络错误：{error}"))
    }
}
