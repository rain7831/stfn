//! 翻译领域模型与 provider 抽象。
//!
//! `Provider` 只负责一次协议调用；去抖、并发、缓存和 UI 更新都在调用方。
//! `run_one` 负责单 provider 的超时、一次重试和 latency 统计。
use std::fmt;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use lru::LruCache;

use crate::lang::Lang;

#[derive(Debug, Clone)]
/// 一次翻译请求：原文 + 已确定的源/目标语言。
pub struct TranslateRequest {
    pub text: String,
    pub from: Lang,
    pub to: Lang,
}

#[derive(Debug, Clone, Default)]
/// 一个 provider 的成功结果；音标/音频由 `pronunciation` 后处理，不经过 provider。
pub struct TranslateOutput {
    pub text: String,
    pub detected: Option<Lang>,
    pub latency_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default)]
/// provider 能力声明，UI 可据此决定是否显示音标/朗读入口。
pub struct Capabilities {
    pub phonetics: bool,
    pub tts: bool,
    pub auto_detect: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 统一错误分类；`retryable` 由分类推导，避免各 provider 自行判断。
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

// 构造器：错误文案面向用户，不包含密钥或请求头。
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
/// 所有翻译源的最小接口。
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

/// provider id + 成功/失败结果；id 用于把异步结果映射回 UI 行。
pub type ProviderResult = (String, Result<TranslateOutput, ProviderError>);

/// 跑单个 provider：超时保护 + 可重试错误退避一次，并记录总耗时。
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

/// 并发跑多个 provider；`join_all` 保持输入顺序，便于 UI 稳定排序。
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

/// 仅内存 LRU 缓存；退出即清空，不落盘翻译内容。
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

    /// 命中时克隆结果，避免把锁的生命周期带到 UI。
    pub fn get(&self, key: &str) -> Option<TranslateOutput> {
        self.inner.lock().ok()?.get(key).cloned()
    }

    pub fn put(&self, key: String, output: TranslateOutput) {
        if let Ok(mut cache) = self.inner.lock() {
            cache.put(key, output);
        }
    }
}

/// 缓存 key 用 \u{1f} 分隔，避免原文拼接造成碰撞。
pub fn cache_key(provider: &str, request: &TranslateRequest) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}",
        provider,
        request.from.code(),
        request.to.code(),
        request.text
    )
}
