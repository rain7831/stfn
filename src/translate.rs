use std::fmt;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use lru::LruCache;

use crate::lang::Lang;

#[derive(Debug, Clone)]
pub struct TranslateRequest {
    pub text: String,
    pub from: Lang,
    pub to: Lang,
}

#[derive(Debug, Clone, Default)]
pub struct TranslateOutput {
    pub text: String,
    pub detected: Option<Lang>,
    pub phonetics: Option<String>,
    pub audio: Option<Vec<u8>>,
    pub latency_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Capabilities {
    pub phonetics: bool,
    pub tts: bool,
    pub auto_detect: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    Auth,
    RateLimit,
    Network,
    Timeout,
    Unsupported,
    Server,
    Parse,
}

impl ProviderErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::RateLimit => "rate_limit",
            Self::Network => "network",
            Self::Timeout => "timeout",
            Self::Unsupported => "unsupported",
            Self::Server => "server",
            Self::Parse => "parse",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub message: String,
    pub retryable: bool,
}

impl ProviderError {
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        let retryable = matches!(
            kind,
            ProviderErrorKind::Network | ProviderErrorKind::Timeout | ProviderErrorKind::Server
        );
        Self {
            kind,
            message: message.into(),
            retryable,
        }
    }

    pub fn auth(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Auth, message)
    }

    pub fn rate_limit(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::RateLimit, message)
    }

    pub fn network(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Network, message)
    }

    pub fn timeout() -> Self {
        Self::new(ProviderErrorKind::Timeout, "网络超时，请检查网络后重试")
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Unsupported, message)
    }

    pub fn server(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Server, message)
    }

    pub fn parse(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Parse, message)
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    fn timeout(&self) -> Duration {
        Duration::from_secs(8)
    }
    async fn translate(&self, request: &TranslateRequest)
        -> Result<TranslateOutput, ProviderError>;
}

pub type ProviderResult = (String, Result<TranslateOutput, ProviderError>);

pub async fn run_one(
    provider: std::sync::Arc<dyn Provider>,
    request: TranslateRequest,
) -> ProviderResult {
    let id = provider.id().to_string();
    let mut retried = false;
    let started = std::time::Instant::now();

    loop {
        let result = tokio::time::timeout(provider.timeout(), provider.translate(&request))
            .await
            .unwrap_or_else(|_| Err(ProviderError::timeout()));

        match result {
            Ok(mut output) => {
                output.latency_ms = Some(started.elapsed().as_millis() as u64);
                return (id, Ok(output));
            }
            Err(err) if err.retryable && !retried => {
                retried = true;
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            Err(err) => return (id, Err(err)),
        }
    }
}

pub async fn run_providers(
    providers: Vec<std::sync::Arc<dyn Provider>>,
    request: TranslateRequest,
) -> Vec<ProviderResult> {
    futures::future::join_all(
        providers
            .into_iter()
            .map(|provider| run_one(provider, request.clone())),
    )
    .await
}

pub struct TranslateCache {
    inner: Mutex<LruCache<String, TranslateOutput>>,
}

impl TranslateCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(LruCache::new(
                std::num::NonZeroUsize::new(capacity).expect("cache capacity > 0"),
            )),
        }
    }

    pub fn get(&self, key: &str) -> Option<TranslateOutput> {
        self.inner.lock().ok()?.get(key).cloned()
    }

    pub fn put(&self, key: String, output: TranslateOutput) {
        if let Ok(mut cache) = self.inner.lock() {
            cache.put(key, output);
        }
    }
}

pub fn cache_key(provider: &str, request: &TranslateRequest) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}",
        provider,
        request.from.code(),
        request.to.code(),
        request.text
    )
}
