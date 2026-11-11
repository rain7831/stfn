use serde::{Deserialize, Deserializer, Serialize};
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use toml_edit::{value, DocumentMut, Item, Table};

pub const APP_DIR: &str = "stfn";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    #[default]
    Auto,
    Zh,
    En,
}

impl<'de> Deserialize<'de> for Target {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "zh" => Self::Zh,
            "en" => Self::En,
            _ => Self::Auto,
        })
    }
}

impl Target {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Zh => "zh",
            Self::En => "en",
        }
    }

    pub fn resolve(self, detection: crate::lang::Detection) -> Option<crate::lang::Lang> {
        use crate::lang::{Detection, Lang};
        match self {
            Self::Zh => Some(Lang::Zh),
            Self::En => Some(Lang::En),
            Self::Auto => match detection {
                Detection::Zh => Some(Lang::En),
                Detection::En => Some(Lang::Zh),
                _ => None,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SourceChoice {
    #[default]
    Bing,
    Volcengine,
    Tencent,
    All,
}

impl<'de> Deserialize<'de> for SourceChoice {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "volcengine" => Self::Volcengine,
            "tencent" => Self::Tencent,
            "all" => Self::All,
            _ => Self::Bing,
        })
    }
}

impl SourceChoice {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bing => "bing",
            Self::Volcengine => "volcengine",
            Self::Tencent => "tencent",
            Self::All => "all",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    pub auto_translate: bool,
    pub debounce_ms: u64,
    pub target: Target,
    pub default_source: SourceChoice,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            auto_translate: true,
            debounce_ms: 300,
            target: Target::Auto,
            default_source: SourceChoice::Bing,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProvidersConfig {
    pub bing: BingConfig,
    pub volcengine: VolcengineConfig,
    pub tencent: TencentConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BingConfig {
    pub enabled: bool,
}

impl Default for BingConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VolcengineConfig {
    pub enabled: bool,
    pub region: String,
}

impl Default for VolcengineConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            region: "cn-north-1".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TencentConfig {
    pub enabled: bool,
    pub region: String,
}

impl Default for TencentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            region: "ap-guangzhou".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub general: GeneralConfig,
    pub providers: ProvidersConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            general: GeneralConfig::default(),
            providers: ProvidersConfig::default(),
        }
    }
}

impl Config {
    pub fn enabled_provider_ids(&self) -> Vec<&'static str> {
        let mut ids = Vec::new();
        if self.providers.bing.enabled {
            ids.push("bing");
        }
        if self.providers.volcengine.enabled {
            ids.push("volcengine");
        }
        if self.providers.tencent.enabled {
            ids.push("tencent");
        }
        ids
    }
}

pub fn config_dir() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        PathBuf::from(dir).join(APP_DIR)
    } else if let Some(home) = env::var_os("HOME").filter(|v| !v.is_empty()) {
        PathBuf::from(home).join(".config").join(APP_DIR)
    } else {
        PathBuf::from(".config").join(APP_DIR)
    }
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub struct LoadedConfig {
    pub config: Config,
    pub warnings: Vec<String>,
}

pub fn load() -> LoadedConfig {
    let path = config_path();
    let mut warnings = Vec::new();
    let config = if !path.exists() {
        let config = Config::default();
        if let Err(err) = save(&config) {
            warnings.push(format!("无法创建默认配置：{err}"));
        }
        config
    } else {
        match fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(mut config) => {
                    validate(&mut config, &mut warnings);
                    config
                }
                Err(err) => {
                    backup(&path);
                    warnings.push(format!("配置解析失败，已备份并使用默认值：{err}"));
                    let config = Config::default();
                    let _ = save(&config);
                    config
                }
            },
            Err(err) => {
                warnings.push(format!("无法读取配置，使用默认值：{err}"));
                Config::default()
            }
        }
    };

    LoadedConfig { config, warnings }
}

fn validate(config: &mut Config, warnings: &mut Vec<String>) {
    if config.version > 1 {
        warnings.push(format!(
            "配置版本 {} 高于当前支持的 1，按 v1 读取",
            config.version
        ));
        config.version = 1;
    }

    let clamped = config.general.debounce_ms.clamp(200, 2000);
    if clamped != config.general.debounce_ms {
        warnings.push(format!(
            "debounce_ms={} 超出范围，已调整为 {clamped}",
            config.general.debounce_ms
        ));
        config.general.debounce_ms = clamped;
    }

    if config.providers.volcengine.region.trim().is_empty() {
        config.providers.volcengine.region = "cn-north-1".to_string();
    }
    if config.providers.tencent.region.trim().is_empty() {
        config.providers.tencent.region = "ap-guangzhou".to_string();
    }
}

pub fn save(config: &Config) -> std::io::Result<()> {
    let path = config_path();
    let dir = config_dir();
    fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }

    let mut doc = fs::read_to_string(&path)
        .ok()
        .and_then(|text| text.parse::<DocumentMut>().ok())
        .unwrap_or_default();

    doc["version"] = value(config.version as i64);

    let general = ensure_table(&mut doc, "general");
    general.insert("auto_translate", value(config.general.auto_translate));
    general.insert("debounce_ms", value(config.general.debounce_ms as i64));
    general.insert("target", value(config.general.target.as_str()));
    general.insert(
        "default_source",
        value(config.general.default_source.as_str()),
    );

    let providers = ensure_table(&mut doc, "providers");
    providers.set_implicit(true);
    let bing = ensure_subtable(providers, "bing");
    bing.insert("enabled", value(config.providers.bing.enabled));

    let volc = ensure_subtable(providers, "volcengine");
    volc.insert("enabled", value(config.providers.volcengine.enabled));
    volc.insert("region", value(config.providers.volcengine.region.as_str()));

    let tencent = ensure_subtable(providers, "tencent");
    tencent.insert("enabled", value(config.providers.tencent.enabled));
    tencent.insert("region", value(config.providers.tencent.region.as_str()));

    atomic_write(&path, doc.to_string().as_bytes(), 0o644)
}

fn ensure_table<'a>(doc: &'a mut DocumentMut, key: &str) -> &'a mut Table {
    if !doc.as_table().contains_key(key) {
        doc.as_table_mut().insert(key, Item::Table(Table::new()));
    }
    doc.as_table_mut()
        .get_mut(key)
        .and_then(Item::as_table_mut)
        .expect("table just inserted")
}

fn ensure_subtable<'a>(table: &'a mut Table, key: &str) -> &'a mut Table {
    if !table.contains_key(key) {
        table.insert(key, Item::Table(Table::new()));
    }
    table
        .get_mut(key)
        .and_then(Item::as_table_mut)
        .expect("table just inserted")
}

fn backup(path: &Path) {
    let backup = path.with_extension("toml.bak");
    let _ = fs::rename(path, backup);
}

pub fn atomic_write(path: &Path, data: &[u8], mode: u32) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .map(|v| v.to_string_lossy().to_string())
        .unwrap_or_else(|| "config.toml".to_string());
    let tmp = parent.join(format!(".{name}.tmp.{}", std::process::id()));

    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(mode);
        }
        let mut file = opts.open(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
    }

    fs::rename(&tmp, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
    if let Ok(dir) = fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}
