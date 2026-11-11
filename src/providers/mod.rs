//! Provider 构造层。
//!
//! 把配置/凭据转换成 `Arc<dyn Provider>`；缺凭据等构造错误也作为
//! `BuiltProvider` 返回，让 UI/CLI 能按平台展示“缺少凭据”等状态。
pub mod alibaba;
pub mod bing;
pub mod volcengine;

use std::sync::Arc;

use crate::config::Config;
use crate::secrets::Credentials;
use crate::translate::{Provider, ProviderError};

/// 构造结果：id 固定存在，provider 可能因缺凭据而失败。
pub struct BuiltProvider {
    pub id: &'static str,
    pub result: Result<Arc<dyn Provider>, ProviderError>,
}

/// v0.1 支持的翻译源 id；顺序用于 CLI --source all。
pub fn all_ids() -> [&'static str; 3] {
    ["bing", "volcengine", "alibaba"]
}

/// 按 id 构造 provider；Bing 无需凭据，火山/阿里缺凭据时返回 Auth 错误。
pub fn build_providers(
    ids: &[&'static str],
    _config: &Config,
    credentials: &Credentials,
    client: &reqwest::Client,
) -> Vec<BuiltProvider> {
    ids.iter()
        .map(|id| match *id {
            "bing" => BuiltProvider {
                id: "bing",
                result: Ok(Arc::new(bing::Bing::new(client.clone()))),
            },
            "volcengine" => match credentials.volcengine_pair() {
                Some((access_key, secret_key)) => BuiltProvider {
                    id: "volcengine",
                    result: Ok(Arc::new(volcengine::Volcengine::new(
                        client.clone(),
                        access_key,
                        secret_key,
                    ))),
                },
                None => BuiltProvider {
                    id: "volcengine",
                    result: Err(ProviderError::auth("缺少火山引擎密钥，请在设置中填写")),
                },
            },
            "alibaba" => match credentials.alibaba_pair() {
                Some((access_key_id, access_key_secret)) => BuiltProvider {
                    id: "alibaba",
                    result: Ok(Arc::new(alibaba::Alibaba::new(
                        client.clone(),
                        access_key_id,
                        access_key_secret,
                    ))),
                },
                None => BuiltProvider {
                    id: "alibaba",
                    result: Err(ProviderError::auth("缺少阿里云 AccessKey，请在设置中填写")),
                },
            },
            other => BuiltProvider {
                id: "unknown",
                result: Err(ProviderError::unsupported(format!("未知翻译源：{other}"))),
            },
        })
        .collect()
}
