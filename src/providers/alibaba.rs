use async_trait::async_trait;
use base64::Engine;
use hmac::{Hmac, Mac};
use sha1::Sha1;
use time::macros::format_description;
use uuid::Uuid;

use crate::lang::Lang;
use crate::translate::{Capabilities, Provider, ProviderError, TranslateOutput, TranslateRequest};

type HmacSha1 = Hmac<Sha1>;

const ENDPOINT: &str = "https://mt.cn-hangzhou.aliyuncs.com/";
const VERSION: &str = "2018-10-12";
const REGION: &str = "cn-hangzhou";

pub struct Alibaba {
    client: reqwest::Client,
    access_key_id: String,
    access_key_secret: String,
    endpoint: String,
}

impl Alibaba {
    pub fn new(client: reqwest::Client, access_key_id: String, access_key_secret: String) -> Self {
        let endpoint = std::env::var("STFN_ALI_BASE_URL").unwrap_or_else(|_| ENDPOINT.to_string());
        Self {
            client,
            access_key_id,
            access_key_secret,
            endpoint,
        }
    }

    fn sign(&self, params: &[(String, String)]) -> Result<String, ProviderError> {
        let mut sorted = params.to_vec();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));

        let canonical = sorted
            .iter()
            .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        let string_to_sign = format!("POST&%2F&{}", percent_encode(&canonical));

        let key = format!("{}&", self.access_key_secret);
        let mut mac = HmacSha1::new_from_slice(key.as_bytes())
            .map_err(|e| ProviderError::parse(format!("阿里云签名密钥无效：{e}")))?;
        mac.update(string_to_sign.as_bytes());
        Ok(base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes()))
    }
}

#[async_trait]
impl Provider for Alibaba {
    fn id(&self) -> &'static str {
        "alibaba"
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
        let timestamp = time::OffsetDateTime::now_utc()
            .format(format_description!(
                "[year]-[month]-[day]T[hour]:[minute]:[second]Z"
            ))
            .map_err(|e| ProviderError::parse(format!("生成阿里云时间戳失败：{e}")))?;

        let params = vec![
            ("AccessKeyId".to_string(), self.access_key_id.clone()),
            ("Action".to_string(), "TranslateGeneral".to_string()),
            ("Format".to_string(), "JSON".to_string()),
            ("FormatType".to_string(), "text".to_string()),
            ("RegionId".to_string(), REGION.to_string()),
            ("Scene".to_string(), "general".to_string()),
            ("SignatureMethod".to_string(), "HMAC-SHA1".to_string()),
            ("SignatureNonce".to_string(), Uuid::new_v4().to_string()),
            ("SignatureVersion".to_string(), "1.0".to_string()),
            (
                "SourceLanguage".to_string(),
                request.from.code().to_string(),
            ),
            ("SourceText".to_string(), request.text.clone()),
            ("TargetLanguage".to_string(), request.to.code().to_string()),
            ("Timestamp".to_string(), timestamp),
            ("Version".to_string(), VERSION.to_string()),
        ];

        let signature = self.sign(&params)?;
        let mut body_params = params;
        body_params.push(("Signature".to_string(), signature));
        let body = body_params
            .iter()
            .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
            .collect::<Vec<_>>()
            .join("&");

        let response = self
            .client
            .post(&self.endpoint)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .header(reqwest::header::ACCEPT, "application/json")
            .body(body)
            .send()
            .await
            .map_err(map_reqwest_error)?;

        if !response.status().is_success() {
            return Err(status_error(response.status()));
        }

        let bytes = response.bytes().await.map_err(map_reqwest_error)?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::parse(format!("阿里云响应解析失败：{e}")))?;

        let (code, message, data) = extract_response(&value);
        if code != 200 {
            return Err(ali_error(code, &message));
        }

        let translated = data
            .and_then(|data| data.get("Translated"))
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ProviderError::parse("阿里云未返回译文"))?;

        Ok(TranslateOutput {
            text: translated.to_string(),
            detected: data
                .and_then(|data| data.get("DetectedLanguage"))
                .and_then(|value| value.as_str())
                .and_then(Lang::from_code)
                .or(Some(request.from)),
            phonetics: None,
            audio: None,
            latency_ms: None,
        })
    }
}

fn extract_response(value: &serde_json::Value) -> (i64, String, Option<&serde_json::Value>) {
    let root = value.get("TranslateGeneralResponse").unwrap_or(value);
    let code = root
        .get("Code")
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str().and_then(|v| v.parse().ok()))
        })
        .unwrap_or(200);
    let message = root
        .get("Message")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    (code, message, root.get("Data"))
}

fn ali_error(code: i64, message: &str) -> ProviderError {
    let message = if message.is_empty() {
        "阿里云返回错误".to_string()
    } else {
        format!("阿里云返回错误：{message}")
    };
    match code {
        10001 => ProviderError::timeout(),
        10005 => ProviderError::unsupported(message),
        10008 => ProviderError::parse(message),
        10009..=10013 => ProviderError::auth(message),
        _ => ProviderError::server(message),
    }
}

fn percent_encode(value: &str) -> String {
    let encoded: String = url::form_urlencoded::byte_serialize(value.as_bytes()).collect();
    encoded
        .replace('+', "%20")
        .replace('*', "%2A")
        .replace("%7E", "~")
}

fn status_error(status: reqwest::StatusCode) -> ProviderError {
    match status.as_u16() {
        401 | 403 => ProviderError::auth("阿里云认证失败，请检查 AccessKey"),
        429 => ProviderError::rate_limit("阿里云请求过于频繁，请稍后再试"),
        500..=599 => ProviderError::server(format!("阿里云服务异常（{}）", status.as_u16())),
        _ => ProviderError::parse(format!("阿里云请求被拒绝（{}）", status.as_u16())),
    }
}

fn map_reqwest_error(error: reqwest::Error) -> ProviderError {
    if error.is_timeout() {
        ProviderError::timeout()
    } else {
        ProviderError::network(format!("网络错误：{error}"))
    }
}
