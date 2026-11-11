use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::PathBuf;
use toml_edit::{value, DocumentMut, Item, Table};

use crate::config::{atomic_write, config_dir};

const SERVICE: &str = "stfn";

#[derive(Debug, Clone, Default)]
pub struct Credentials {
    pub volcengine: VolcCredentials,
    pub alibaba: AlibabaCredentials,
}

#[derive(Debug, Clone, Default)]
pub struct VolcCredentials {
    pub access_key: Option<SecretString>,
    pub secret_key: Option<SecretString>,
}

#[derive(Debug, Clone, Default)]
pub struct AlibabaCredentials {
    pub access_key_id: Option<SecretString>,
    pub access_key_secret: Option<SecretString>,
}

impl Credentials {
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
pub enum Stored {
    Keyring,
    File,
}

pub fn keyring_available() -> bool {
    keyring::Entry::store_status().is_ok()
}

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

fn env_secret(field: &str) -> Option<SecretString> {
    let name = env_var_for(field)?;
    env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .map(SecretString::from)
}

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

fn keyring_get(field: &str) -> Option<SecretString> {
    let entry = keyring::Entry::new(SERVICE, field).ok()?;
    entry.get_password().ok().map(SecretString::from)
}

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

pub fn credentials_path() -> PathBuf {
    config_dir().join("credentials.toml")
}

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

fn split_field(field: &str) -> Result<(&str, &str), String> {
    let (provider, key) = field
        .split_once('.')
        .ok_or_else(|| format!("非法密钥字段：{field}"))?;
    Ok((provider, key))
}

#[derive(Debug, Default, Deserialize)]
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
