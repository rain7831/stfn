use std::io::Read;
use std::time::Duration;

use clap::{Parser, Subcommand};
use serde::Serialize;

use crate::config::{self, Config, SourceChoice};
use crate::lang::{self, Detection, Lang};
use crate::providers;
use crate::secrets;
use crate::translate::{self, ProviderError, TranslateRequest};

#[derive(Parser, Debug)]
#[command(name = "stfn", version, about = "Simple Translator For Niri")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[arg(value_name = "TEXT", num_args = 0..)]
    pub text: Vec<String>,

    #[arg(short = 't', long, value_name = "LANG")]
    pub to: Option<String>,

    #[arg(short = 's', long, value_name = "SOURCE")]
    pub source: Option<String>,

    #[arg(long)]
    pub notify: bool,

    #[arg(long)]
    pub speak: bool,

    #[arg(long)]
    pub json: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// 打开配置窗口
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Gui { settings: bool },
    Cli,
}

impl Cli {
    pub fn mode(&self) -> Mode {
        if matches!(self.command, Some(Command::Settings)) {
            return Mode::Gui { settings: true };
        }
        let has_flag =
            self.to.is_some() || self.source.is_some() || self.notify || self.speak || self.json;
        if self.text.is_empty() && !has_flag {
            Mode::Gui { settings: false }
        } else {
            Mode::Cli
        }
    }
}

pub fn run(cli: Cli) -> i32 {
    if cli.notify && cli.json {
        eprintln!("--notify 与 --json 不能同时使用");
        return 2;
    }
    if cli.speak {
        eprintln!("朗读功能将在 v1.1 提供");
        return 1;
    }

    let loaded = config::load();
    for warning in &loaded.warnings {
        eprintln!("配置：{warning}");
    }
    let config = loaded.config;
    let text = match read_text(&cli) {
        Ok(text) => text,
        Err(message) => {
            eprintln!("{message}");
            return 2;
        }
    };

    let detection = lang::detect(&text);
    let from = match detection {
        Detection::Zh => Lang::Zh,
        Detection::En => Lang::En,
        Detection::Empty => {
            eprintln!("输入为空，或只包含数字 / 符号");
            return 2;
        }
        Detection::Unsupported => {
            eprintln!("v1 仅支持中英互译");
            return 2;
        }
    };

    let target = match resolve_target(&cli, &config, detection) {
        Ok(target) => target,
        Err(message) => {
            eprintln!("{message}");
            return 2;
        }
    };

    let ids = match resolve_sources(&cli, &config) {
        Ok(ids) => ids,
        Err(message) => {
            eprintln!("{message}");
            return 2;
        }
    };

    let credentials = if ids.iter().any(|id| *id != "bing") {
        let (credentials, warnings) = secrets::load();
        for warning in &warnings {
            eprintln!("密钥：{warning}");
        }
        credentials
    } else {
        secrets::Credentials::default()
    };

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("无法启动异步运行时：{err}");
            return 1;
        }
    };
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .cookie_store(true)
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            eprintln!("无法创建 HTTP 客户端：{err}");
            return 1;
        }
    };

    let built = providers::build_providers(&ids, &config, &credentials, &client);
    let mut available = Vec::new();
    let mut failures: Vec<(String, ProviderError)> = Vec::new();
    for item in built {
        match item.result {
            Ok(provider) => available.push(provider),
            Err(error) => failures.push((item.id.to_string(), error)),
        }
    }

    if available.is_empty() {
        for (id, error) in &failures {
            eprintln!("{id}: {error}");
        }
        return 1;
    }

    let request = TranslateRequest {
        text: text.clone(),
        from,
        to: target,
    };
    let results = runtime.block_on(translate::run_providers(available, request));

    let mut successes = Vec::new();
    for (id, result) in results {
        match result {
            Ok(output) => successes.push((id, output)),
            Err(error) => failures.push((id, error)),
        }
    }

    if cli.json {
        print_json(&text, from, target, &successes, &failures);
        return if successes.is_empty() { 1 } else { 0 };
    }

    if cli.notify {
        let Some((_, output)) = successes.first() else {
            for (id, error) in &failures {
                eprintln!("{id}: {error}");
            }
            return 1;
        };
        let title = format!("STFN · {} → {}", from.label(), target.label());
        if let Err(message) = crate::notify::send(&title, &truncate(&output.text, 200)) {
            eprintln!("{message}");
            return 1;
        }
        return 0;
    }

    for (id, error) in &failures {
        eprintln!("{id}: {error}");
    }

    if successes.is_empty() {
        return 1;
    }

    if successes.len() == 1 {
        println!("{}", successes[0].1.text);
    } else {
        for (index, (id, output)) in successes.iter().enumerate() {
            if index > 0 {
                println!();
            }
            println!("[{id}]");
            println!("{}", output.text);
        }
    }

    0
}

fn read_text(cli: &Cli) -> Result<String, String> {
    if cli.text.is_empty() {
        return Err("缺少要翻译的文本".to_string());
    }
    if cli.text.iter().any(|part| part == "-") {
        if cli.text.len() != 1 {
            return Err("- 只能作为唯一的文本参数".to_string());
        }
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| format!("读取 stdin 失败：{e}"))?;
        let text = text.trim_end_matches(['\n', '\r']).to_string();
        if text.is_empty() {
            return Err("输入为空".to_string());
        }
        if text.chars().count() > 5000 {
            return Err("输入超过 5000 字符上限".to_string());
        }
        return Ok(text);
    }

    let text = cli.text.join(" ");
    if text.trim().is_empty() {
        return Err("输入为空".to_string());
    }
    if text.chars().count() > 5000 {
        return Err("输入超过 5000 字符上限".to_string());
    }
    Ok(text)
}

fn resolve_target(cli: &Cli, config: &Config, detection: Detection) -> Result<Lang, String> {
    match cli.to.as_deref() {
        Some("auto") | None => config
            .general
            .target
            .resolve(detection)
            .ok_or_else(|| "无法判断翻译方向".to_string()),
        Some("zh") => Ok(Lang::Zh),
        Some("en") => Ok(Lang::En),
        Some(other) => Err(format!("未知目标语言：{other}")),
    }
}

fn resolve_sources(cli: &Cli, config: &Config) -> Result<Vec<&'static str>, String> {
    if let Some(source) = cli.source.as_deref() {
        let mut ids = Vec::new();
        for part in source.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match part {
                "all" => {
                    for id in config.enabled_provider_ids() {
                        if !ids.contains(&id) {
                            ids.push(id);
                        }
                    }
                }
                "bing" => push_unique(&mut ids, "bing"),
                "volcengine" => push_unique(&mut ids, "volcengine"),
                "tencent" => push_unique(&mut ids, "tencent"),
                other => return Err(format!("未知翻译源：{other}")),
            }
        }
        if ids.is_empty() {
            return Err("没有可用的翻译源".to_string());
        }
        return Ok(ids);
    }

    let ids = match config.general.default_source {
        SourceChoice::Bing => vec!["bing"],
        SourceChoice::Volcengine => vec!["volcengine"],
        SourceChoice::Tencent => vec!["tencent"],
        SourceChoice::All => config.enabled_provider_ids(),
    };
    if ids.is_empty() {
        return Err("没有启用的翻译源，请打开设置".to_string());
    }
    Ok(ids)
}

fn push_unique(ids: &mut Vec<&'static str>, id: &'static str) {
    if !ids.contains(&id) {
        ids.push(id);
    }
}

#[derive(Serialize)]
struct JsonOutput {
    input: String,
    source: &'static str,
    target: &'static str,
    results: Vec<JsonResult>,
    errors: Vec<JsonError>,
}

#[derive(Serialize)]
struct JsonResult {
    provider: String,
    text: String,
    detected_source: Option<&'static str>,
    phonetic: Option<String>,
    latency_ms: Option<u64>,
}

#[derive(Serialize)]
struct JsonError {
    provider: String,
    kind: &'static str,
    message: String,
}

fn print_json(
    input: &str,
    from: Lang,
    target: Lang,
    successes: &[(String, translate::TranslateOutput)],
    failures: &[(String, ProviderError)],
) {
    let output = JsonOutput {
        input: input.to_string(),
        source: from.code(),
        target: target.code(),
        results: successes
            .iter()
            .map(|(provider, result)| JsonResult {
                provider: provider.clone(),
                text: result.text.clone(),
                detected_source: result.detected.map(Lang::code),
                phonetic: result.phonetics.clone(),
                latency_ms: result.latency_ms,
            })
            .collect(),
        errors: failures
            .iter()
            .map(|(provider, error)| JsonError {
                provider: provider.clone(),
                kind: error.kind.as_str(),
                message: error.message.clone(),
            })
            .collect(),
    };

    match serde_json::to_string(&output) {
        Ok(json) => println!("{json}"),
        Err(err) => eprintln!("JSON 输出失败：{err}"),
    }
}

fn truncate(text: &str, max: usize) -> String {
    let mut result: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        result.push('…');
    }
    result
}
