//! Bing 免密钥 provider。
//!
//! 旧 JWT auth 端点已失效，这里模拟网页翻译：
//! 1. GET `/translator` 提取 IG、IID、防滥用 key/token；
//! 2. POST `/ttranslatev3` 表单；
//! 3. 缓存会话，401/429 刷新一次；www.bing.com 失败时回退 cn.bing.com。
use std::time::{Duration, Instant};

use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::translate::{Capabilities, Provider, ProviderError, TranslateOutput, TranslateRequest};

const BASE_URL: &str = "https://www.bing.com";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36 Edg/151.0.4129.59";

#[derive(Debug, Clone)]
// 网页会话参数；带过期时间，避免每次翻译都重新抓页面。
struct Session {
    base_url: String,
    ig: String,
    iid: String,
    key: String,
    token: String,
    expires_at: Instant,
}

/// Bing provider：持有 HTTP client、会话缓存和可覆盖的 base URL。
pub struct Bing {
    client: reqwest::Client,
    session: Mutex<Option<Session>>,
    base_url: String,
}

// Bing 的私有实现：会话管理、页面解析、单次请求。
impl Bing {
    /// 允许 STFN_BING_BASE_URL 覆盖，便于本地 mock 测试。
    pub fn new(client: reqwest::Client) -> Self {
        let base_url = std::env::var("STFN_BING_BASE_URL").unwrap_or_else(|_| BASE_URL.to_string());
        Self {
            client,
            session: Mutex::new(None),
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    /// 获取有效会话；过期或不存在时刷新。
    async fn session(&self) -> Result<Session, ProviderError> {
        {
            let guard = self.session.lock().await;
            if let Some(session) = guard.as_ref() {
                if session.expires_at > Instant::now() {
                    return Ok(session.clone());
                }
            }
        }

        let session = self.fetch_session().await?;
        *self.session.lock().await = Some(session.clone());
        Ok(session)
    }

    /// 401/429 后强制丢弃会话，下次重新抓页面。
    async fn invalidate_session(&self) {
        *self.session.lock().await = None;
    }

    /// 先试 www.bing.com，失败再试 cn.bing.com。
    async fn fetch_session(&self) -> Result<Session, ProviderError> {
        let mut candidates = vec![self.base_url.clone()];
        if self.base_url != "https://cn.bing.com" {
            candidates.push("https://cn.bing.com".to_string());
        }

        let mut last_error = None;
        for base_url in candidates {
            match self.fetch_session_from(&base_url).await {
                Ok(session) => return Ok(session),
                Err(error) => last_error = Some(error),
            }
        }

        Err(last_error.unwrap_or_else(|| ProviderError::parse("Bing 会话获取失败")))
    }

    /// 抓取翻译页并用正则解析 IG/IID/防滥用参数。
    async fn fetch_session_from(&self, base_url: &str) -> Result<Session, ProviderError> {
        let url = format!("{base_url}/translator");
        let response = self
            .client
            .get(url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .send()
            .await
            .map_err(map_reqwest_error)?;

        if !response.status().is_success() {
            return Err(status_error(response.status()));
        }

        let html = response.text().await.map_err(map_reqwest_error)?;

        let ig = capture(&html, r#"IG:"([^"]+)""#)
            .ok_or_else(|| ProviderError::parse("Bing 页面缺少 IG 参数"))?;
        let iid = capture(&html, r#"data-iid="([^"]+)""#)
            .ok_or_else(|| ProviderError::parse("Bing 页面缺少 IID 参数"))?;
        let params = capture(&html, r"params_AbusePreventionHelper\s*=\s*\[([^\]]+)\]")
            .ok_or_else(|| ProviderError::parse("Bing 页面缺少防滥用参数"))?;

        let value: serde_json::Value = serde_json::from_str(&format!("[{}]", params))
            .map_err(|e| ProviderError::parse(format!("Bing 防滥用参数解析失败：{e}")))?;
        let array = value
            .as_array()
            .ok_or_else(|| ProviderError::parse("Bing 防滥用参数格式异常"))?;

        let key = array
            .first()
            .and_then(|v| v.as_i64())
            .map(|v| v.to_string())
            .ok_or_else(|| ProviderError::parse("Bing 缺少 key"))?;
        let token = array
            .get(1)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| ProviderError::parse("Bing 缺少 token"))?;
        let ttl_ms = array.get(2).and_then(|v| v.as_u64()).unwrap_or(3_600_000);

        Ok(Session {
            base_url: base_url.to_string(),
            ig,
            iid,
            key,
            token,
            expires_at: Instant::now()
                + Duration::from_millis(ttl_ms.saturating_sub(60_000).max(60_000)),
        })
    }

    /// 构造 ttranslatev3 表单请求；cookie 由共享 client 保留。
    async fn translate_once(
        &self,
        request: &TranslateRequest,
        session: &Session,
    ) -> Result<reqwest::Response, ProviderError> {
        let url = format!(
            "{}/ttranslatev3?isVertical=1&&IG={}&IID={}",
            session.base_url, session.ig, session.iid
        );
        let referer = format!("{}/translator", session.base_url);

        self.client
            .post(url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header(reqwest::header::REFERER, referer)
            .header(reqwest::header::ORIGIN, session.base_url.clone())
            .form(&[
                ("fromLang", request.from.bing_code()),
                ("to", request.to.bing_code()),
                ("text", request.text.as_str()),
                ("token", session.token.as_str()),
                ("key", session.key.as_str()),
                ("tryFetchingGenderDebiasedTranslations", "true"),
            ])
            .send()
            .await
            .map_err(map_reqwest_error)
    }
}

#[async_trait]
// Provider 实现：Bing 网页接口通常较慢，超时放宽到 8s。
impl Provider for Bing {
    fn id(&self) -> &'static str {
        "bing"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            phonetics: false,
            tts: false,
            auto_detect: true,
        }
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(8)
    }

    /// 401/429 刷新会话并重试一次；响应可能含第二个 inputTransliteration 元素。
    async fn translate(
        &self,
        request: &TranslateRequest,
    ) -> Result<TranslateOutput, ProviderError> {
        let mut refreshed = false;
        loop {
            let session = self.session().await?;
            let response = self.translate_once(request, &session).await?;

            if matches!(
                response.status(),
                reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::TOO_MANY_REQUESTS
            ) && !refreshed
            {
                refreshed = true;
                self.invalidate_session().await;
                continue;
            }

            if !response.status().is_success() {
                return Err(status_error(response.status()));
            }

            let bytes = response.bytes().await.map_err(map_reqwest_error)?;
            let parsed: BingResponse = serde_json::from_slice(&bytes)
                .map_err(|e| ProviderError::parse(format!("Bing 响应解析失败：{e}")))?;

            let item = parsed
                .0
                .into_iter()
                .find(|item| !item.translations.is_empty())
                .ok_or_else(|| ProviderError::parse("Bing 返回空结果"))?;
            let translation = item
                .translations
                .into_iter()
                .next()
                .filter(|t| !t.text.is_empty())
                .ok_or_else(|| ProviderError::parse("Bing 未返回译文"))?;

            return Ok(TranslateOutput {
                text: translation.text,
                detected: Some(request.from),
                phonetics: None,
                audio: None,
                latency_ms: None,
            });
        }
    }
}

// 正则捕获第一组；页面结构变化时返回 None，由上层转成 Parse 错误。
fn capture(html: &str, pattern: &str) -> Option<String> {
    let regex = Regex::new(pattern).ok()?;
    regex
        .captures(html)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_string())
}

#[derive(Debug, Deserialize)]
// Bing 响应数组；中文输入时可能包含多个元素，只取有 translations 的那个。
struct BingResponse(Vec<BingItem>);

#[derive(Debug, Deserialize)]
// 单个响应元素；translations 用 default 兼容只有拼音的附加元素。
struct BingItem {
    #[serde(rename = "detectedLanguage")]
    #[allow(dead_code)]
    detected_language: Option<BingDetected>,
    #[serde(default)]
    translations: Vec<BingTranslation>,
}

#[derive(Debug, Deserialize)]
// 远端的语言检测结果；当前仅保留结构，方向以本地检测为准。
struct BingDetected {
    #[allow(dead_code)]
    language: String,
}

#[derive(Debug, Deserialize)]
// 译文条目。
struct BingTranslation {
    text: String,
}

// HTTP 状态 -> 统一错误分类。
fn status_error(status: reqwest::StatusCode) -> ProviderError {
    match status.as_u16() {
        401 | 403 => ProviderError::auth("Bing 认证失败"),
        429 => ProviderError::rate_limit("Bing 请求过于频繁，请稍后再试"),
        500..=599 => ProviderError::server(format!("Bing 服务异常（{}）", status.as_u16())),
        _ => ProviderError::parse(format!("Bing 请求被拒绝（{}）", status.as_u16())),
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
