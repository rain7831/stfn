pub mod alibaba;
pub mod bing;
pub mod volcengine;

use std::sync::Arc;

use crate::config::Config;
use crate::secrets::Credentials;
use crate::translate::{Provider, ProviderError};

pub struct BuiltProvider {
    pub id: &'static str,
    pub result: Result<Arc<dyn Provider>, ProviderError>,
}

pub fn all_ids() -> [&'static str; 3] {
    ["bing", "volcengine", "alibaba"]
}

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
