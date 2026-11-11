//! 配置模型与读写。
//!
//! 位置：`$XDG_CONFIG_HOME/stfn/config.toml`（默认 `~/.config/stfn/config.toml`）。
//! 约定：
//! - 这里只存非敏感设置，密钥由 `secrets` 模块处理；
//! - 保存时用 `toml_edit` 读改写，尽量保留注释和字段顺序；
//! - 原子写：同目录临时文件 + fsync + rename。
use serde::{Deserialize, Deserializer, Serialize};
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use toml_edit::{value, DocumentMut, Item, Table};

pub const APP_DIR: &str = "stfn";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
/// 输出语言；Auto 时由输入检测决定中译英或英译中。
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
/// CLI/配置中的默认翻译源。
pub enum SourceChoice {
    #[default]
    Bing,
    Volcengine,
    Alibaba,
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
            "alibaba" => Self::Alibaba,
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
            Self::Alibaba => "alibaba",
            Self::All => "all",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
/// 通用行为配置；`debounce_ms` 默认 700ms。
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
            debounce_ms: 700,
            target: Target::Auto,
            default_source: SourceChoice::Bing,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
/// 三家 provider 的启用开关；区域由后端固定，不写入配置。
pub struct ProvidersConfig {
    pub bing: BingConfig,
    pub volcengine: VolcengineConfig,
    pub alibaba: AlibabaConfig,
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct VolcengineConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AlibabaConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
/// 顶层配置。`version` 为未来迁移预留。
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
    /// 按 UI 展示顺序返回启用平台 id。
    pub fn enabled_provider_ids(&self) -> Vec<&'static str> {
        let mut ids = Vec::new();
        if self.providers.bing.enabled {
            ids.push("bing");
        }
        if self.providers.volcengine.enabled {
            ids.push("volcengine");
        }
        if self.providers.alibaba.enabled {
            ids.push("alibaba");
        }
        ids
    }
}

/// XDG 配置目录；测试通过 XDG_CONFIG_HOME 指向临时目录。
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

/// 加载结果：配置本体 + 非致命 warning，供 CLI/GUI 提示。
pub struct LoadedConfig {
    pub config: Config,
    pub warnings: Vec<String>,
}

/// 加载流程：缺失则创建默认；解析失败先备份再用默认值；最后做范围校验。
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

// 容错校验：版本和延迟范围；配置错误不应阻塞启动。
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
}

/// 用 toml_edit 更新已知字段，移除历史 tencent/region 字段，然后原子写。
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
    providers.remove("tencent");

    let bing = ensure_subtable(providers, "bing");
    bing.insert("enabled", value(config.providers.bing.enabled));

    let volc = ensure_subtable(providers, "volcengine");
    volc.insert("enabled", value(config.providers.volcengine.enabled));
    volc.remove("region");

    let alibaba = ensure_subtable(providers, "alibaba");
    alibaba.insert("enabled", value(config.providers.alibaba.enabled));

    atomic_write(&path, doc.to_string().as_bytes(), 0o644)
}

// 确保父表存在；providers 设置为 implicit，避免输出空的 [providers] 头。
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

/// 原子写：写临时文件、fsync、rename，再 fsync 父目录。
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
