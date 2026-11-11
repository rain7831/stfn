//! 密钥读取、写入和删除。
//!
//! 优先级：环境变量 > Secret Service（keyring）> `credentials.toml`（0600）。
//! 设计原则：密钥不进入 config.toml；UI 只显示 `******`；写 keyring 失败不静默降级。
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::PathBuf;
use toml_edit::{value, DocumentMut, Item, Table};

use crate::config::{atomic_write, config_dir};

const SERVICE: &str = "stfn";

#[derive(Debug, Clone, Default)]
/// 内存中的云厂商密钥；使用 SecretString 避免意外 Debug/日志输出。
pub struct Credentials {
    pub volcengine: VolcCredentials,
    pub alibaba: AlibabaCredentials,
}

#[derive(Debug, Clone, Default)]
// 火山引擎凭据：Access Key / Secret Key。
pub struct VolcCredentials {
    pub access_key: Option<SecretString>,
    pub secret_key: Option<SecretString>,
}

#[derive(Debug, Clone, Default)]
// 阿里云凭据：AccessKey ID / AccessKey Secret。
pub struct AlibabaCredentials {
    pub access_key_id: Option<SecretString>,
    pub access_key_secret: Option<SecretString>,
}

impl Credentials {
    /// 两个字段都齐全时返回 (access_key, secret_key)，否则 None。
    pub fn volcengine_pair(&self) -> Option<(String, String)> {
        Some((
            self.volcengine
                .access_key
                .as_ref()?
                .expose_secret()
                .to_string(),
            self.volcengine
                .secret_key
                .as_ref()?
                .expose_secret()
                .to_string(),
        ))
    }

    /// 两个字段都齐全时返回 (access_key_id, access_key_secret)。
    pub fn alibaba_pair(&self) -> Option<(String, String)> {
        Some((
            self.alibaba
                .access_key_id
                .as_ref()?
                .expose_secret()
                .to_string(),
            self.alibaba
                .access_key_secret
                .as_ref()?
                .expose_secret()
                .to_string(),
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 密钥实际保存位置，用于 UI 提示。
pub enum Stored {
    Keyring,
    File,
}

/// 一次性探测 Secret Service 是否可用；keyring crate 底层走 zbus。
pub fn keyring_available() -> bool {
    keyring::Entry::store_status().is_ok()
}

/// 字段名 -> 环境变量名；未知字段返回 None。
pub fn env_var_for(field: &str) -> Option<&'static str> {
    match field {
        "volcengine.access_key" => Some("STFN_VOLC_ACCESS_KEY"),
        "volcengine.secret_key" => Some("STFN_VOLC_SECRET_KEY"),
        "alibaba.access_key_id" => Some("STFN_ALI_ACCESS_KEY_ID"),
        "alibaba.access_key_secret" => Some("STFN_ALI_ACCESS_KEY_SECRET"),
        _ => None,
    }
}

pub fn env_is_set(field: &str) -> bool {
    env_var_for(field)
        .and_then(env::var_os)
        .is_some_and(|v| !v.is_empty())
}

// 空字符串视为未设置，避免把空值覆盖到有效凭据。
fn env_secret(field: &str) -> Option<SecretString> {
    let name = env_var_for(field)?;
    env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .map(SecretString::from)
}

/// 按 env -> keyring -> file 的优先级填充每个字段。
pub fn load() -> (Credentials, Vec<String>) {
    let mut warnings = Vec::new();
    let keyring_ok = keyring_available();
    if !keyring_ok {
        warnings.push("系统密钥环不可用，将从 0600 凭据文件读取".to_string());
    }

    let mut credentials = Credentials::default();
    let file = load_file(&mut warnings);

    load_field(
        &mut credentials.volcengine.access_key,
        "volcengine.access_key",
        &file.providers.volcengine.access_key,
        keyring_ok,
    );
    load_field(
        &mut credentials.volcengine.secret_key,
        "volcengine.secret_key",
        &file.providers.volcengine.secret_key,
        keyring_ok,
    );
    load_field(
        &mut credentials.alibaba.access_key_id,
        "alibaba.access_key_id",
        &file.providers.alibaba.access_key_id,
        keyring_ok,
    );
    load_field(
        &mut credentials.alibaba.access_key_secret,
        "alibaba.access_key_secret",
        &file.providers.alibaba.access_key_secret,
        keyring_ok,
    );

    (credentials, warnings)
}

// 单字段优先级：环境变量 > keyring > 回退文件。
fn load_field(
    slot: &mut Option<SecretString>,
    field: &str,
    file_value: &Option<String>,
    keyring_ok: bool,
) {
    if let Some(value) = env_secret(field) {
        *slot = Some(value);
        return;
    }
    if keyring_ok {
        if let Some(value) = keyring_get(field) {
            *slot = Some(value);
            return;
        }
    }
    if let Some(value) = file_value.as_ref().filter(|v| !v.is_empty()) {
        *slot = Some(SecretString::from(value.clone()));
    }
}

// 从 keyring 读取；条目不存在或读取失败都返回 None。
fn keyring_get(field: &str) -> Option<SecretString> {
    let entry = keyring::Entry::new(SERVICE, field).ok()?;
    entry.get_password().ok().map(SecretString::from)
}

/// 写入密钥：环境变量字段拒绝修改；keyring 可用时必须写成功，否则写 0600 文件。
pub fn store_secret(field: &str, value: &str) -> Result<Stored, String> {
    if env_is_set(field) {
        return Err(format!(
            "{} 由环境变量提供，不能在设置中修改",
            env_var_for(field).unwrap_or(field)
        ));
    }
    if value.is_empty() {
        return Ok(Stored::Keyring);
    }

    if keyring_available() {
        if let Ok(entry) = keyring::Entry::new(SERVICE, field) {
            if entry.set_password(value).is_ok() {
                return Ok(Stored::Keyring);
            }
        }
        return Err("无法写入系统密钥环，请解锁密钥环后重试".to_string());
    }

    save_file_secret(field, value)?;
    Ok(Stored::File)
}

/// 删除密钥：同时清理 keyring 条目和回退文件字段；环境变量字段拒绝。
pub fn delete_secret(field: &str) -> Result<(), String> {
    if env_is_set(field) {
        return Err(format!(
            "{} 由环境变量提供，不能在设置中删除",
            env_var_for(field).unwrap_or(field)
        ));
    }
    if let Ok(entry) = keyring::Entry::new(SERVICE, field) {
        let _ = entry.delete_credential();
    }
    let path = credentials_path();
    if !path.exists() {
        return Ok(());
    }
    let text = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut doc = text.parse::<DocumentMut>().map_err(|e| e.to_string())?;
    let (provider, key) = split_field(field)?;
    if let Some(table) = doc
        .as_table_mut()
        .get_mut("providers")
        .and_then(Item::as_table_mut)
        .and_then(|t| t.get_mut(provider))
        .and_then(Item::as_table_mut)
    {
        table.remove(key);
    }
    atomic_write(&path, doc.to_string().as_bytes(), 0o600).map_err(|e| e.to_string())
}

/// 回退凭据文件路径。
pub fn credentials_path() -> PathBuf {
    config_dir().join("credentials.toml")
}

// 回退文件写入：目录 0700、文件 0600，使用 toml_edit 保留其他字段。
fn save_file_secret(field: &str, secret: &str) -> Result<(), String> {
    let (provider, key) = split_field(field)?;
    let path = credentials_path();
    fs::create_dir_all(config_dir()).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(config_dir(), fs::Permissions::from_mode(0o700));
    }

    let mut doc = fs::read_to_string(&path)
        .ok()
        .and_then(|text| text.parse::<DocumentMut>().ok())
        .unwrap_or_default();

    if !doc.as_table().contains_key("providers") {
        doc.as_table_mut()
            .insert("providers", Item::Table(Table::new()));
    }
    let providers = doc
        .as_table_mut()
        .get_mut("providers")
        .and_then(Item::as_table_mut)
        .expect("providers table");
    if !providers.contains_key(provider) {
        providers.insert(provider, Item::Table(Table::new()));
    }
    let table = providers
        .get_mut(provider)
        .and_then(Item::as_table_mut)
        .expect("provider table");
    table.insert(key, value(secret));

    atomic_write(&path, doc.to_string().as_bytes(), 0o600).map_err(|e| e.to_string())
}

// 把 `provider.key` 拆成 (provider, key)。
fn split_field(field: &str) -> Result<(&str, &str), String> {
    let (provider, key) = field
        .split_once('.')
        .ok_or_else(|| format!("非法密钥字段：{field}"))?;
    Ok((provider, key))
}

#[derive(Debug, Default, Deserialize)]
// 回退文件的反序列化结构；不实现 Serialize，写入由 toml_edit 完成。
struct FileCredentials {
    #[serde(default)]
    providers: FileProviders,
}

#[derive(Debug, Default, Deserialize)]
struct FileProviders {
    #[serde(default)]
    volcengine: FileVolcengine,
    #[serde(default)]
    alibaba: FileAlibaba,
}

#[derive(Debug, Default, Deserialize)]
struct FileVolcengine {
    access_key: Option<String>,
    secret_key: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct FileAlibaba {
    access_key_id: Option<String>,
    access_key_secret: Option<String>,
}

// 读取回退文件；权限过宽时尝试收紧为 0600，解析失败只警告不阻塞启动。
fn load_file(warnings: &mut Vec<String>) -> FileCredentials {
    let path = credentials_path();
    if !path.exists() {
        return FileCredentials::default();
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(&path) {
            if meta.permissions().mode() & 0o077 != 0 {
                warnings.push(format!("{} 权限过宽，尝试收紧为 0600", path.display()));
                let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
            }
        }
    }

    match fs::read_to_string(&path) {
        Ok(text) => toml::from_str::<FileCredentials>(&text).unwrap_or_else(|err| {
            warnings.push(format!("凭据文件解析失败，已忽略：{err}"));
            FileCredentials::default()
        }),
        Err(err) => {
            warnings.push(format!("无法读取凭据文件：{err}"));
            FileCredentials::default()
        }
    }
}
