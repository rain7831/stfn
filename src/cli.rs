//! CLI：参数解析、文本读取、翻译源选择、翻译执行和输出格式。
//!
//! 退出码约定：0 至少一个成功；1 运行时失败；2 用法/输入/配置错误。
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use serde::Serialize;

use crate::config::{self, Config, SourceChoice};
use crate::lang::{self, Detection, Lang};
use crate::pronunciation::{self, Accent, PronunciationService};
use crate::providers;
use crate::secrets;
use crate::translate::{self, ProviderError, TranslateOutput, TranslateRequest};
use crate::tts::{self, EdgeTts};

#[derive(Parser, Debug)]
#[command(name = "stfn", version, about = "Simple Translator For Niri")]
/// clap 参数模型；`text` 为空且无 flag 时进入 GUI。
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
/// 子命令：应用内设置页或选区翻译；两者都是 GUI 启动意图。
pub enum Command {
    /// 打开应用内设置页
    Settings,
    /// 读取 primary selection 并自动翻译
    Selection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// GUI 启动意图；由 clap 参数解析，不涉及跨进程转发。
pub enum GuiIntent {
    Open,
    Settings,
    Selection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 参数分派结果：GUI（带启动意图）或一次性 CLI。
pub enum Mode {
    Gui { intent: GuiIntent },
    Cli,
}

impl Cli {
    /// 无文本且无 flag -> GUI 主窗口；settings / selection -> 对应 GUI 意图。
    pub fn mode(&self) -> Mode {
        match self.command {
            Some(Command::Settings) => {
                return Mode::Gui {
                    intent: GuiIntent::Settings,
                };
            }
            Some(Command::Selection) => {
                return Mode::Gui {
                    intent: GuiIntent::Selection,
                };
            }
            None => {}
        }
        let has_flag =
            self.to.is_some() || self.source.is_some() || self.notify || self.speak || self.json;
        if self.text.is_empty() && !has_flag {
            Mode::Gui {
                intent: GuiIntent::Open,
            }
        } else {
            Mode::Cli
        }
    }
}

/// CLI 主流程：校验参数 -> 配置/输入 -> 检测方向 -> 选源 -> 并发翻译 -> 输出。
pub fn run(cli: Cli) -> i32 {
    if cli.notify && cli.json {
        eprintln!("--notify 与 --json 不能同时使用");
        return 2;
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

    // v0.2 朗读只支持英文译文；中文目标在请求网络前直接拒绝。
    if cli.speak && target != Lang::En {
        eprintln!("朗读仅支持英文译文");
        return 1;
    }

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
        let phonetics = json_phonetics(&runtime, &client, &successes);
        print_json(&text, from, target, &successes, &failures, &phonetics);
        if successes.is_empty() {
            return 1;
        }
    } else if successes.is_empty() {
        for (id, error) in &failures {
            eprintln!("{id}: {error}");
        }
        return 1;
    } else if cli.notify {
        let output = &successes[0].1;
        let title = format!("STFN · {} → {}", from.label(), target.label());
        if let Err(message) = crate::notify::send(&title, &truncate(&output.text, 200)) {
            eprintln!("{message}");
            return 1;
        }
    } else {
        for (id, error) in &failures {
            eprintln!("{id}: {error}");
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
    }

    // 先输出译文/通知/JSON，再朗读；朗读失败不吞掉已经输出的结果，但退出码为 1。
    if cli.speak {
        if let Err(message) = speak(&runtime, &successes[0].1.text) {
            eprintln!("{message}");
            return 1;
        }
    }

    0
}

// --speak：英文译文用 Edge TTS 合成并播放到结束；默认 US 音色。
fn speak(runtime: &tokio::runtime::Runtime, text: &str) -> Result<(), String> {
    let tts = Arc::new(EdgeTts::new());
    let bytes = runtime
        .block_on(tts.synthesize(text, Accent::Us))
        .ok_or_else(|| "朗读不可用，请检查网络或音频设备".to_string())?;
    tts::play_blocking(bytes)
}

// --json 的 phonetic 字段：只查英文单词，取 US > UK > generic 的首选 IPA。
fn json_phonetics(
    runtime: &tokio::runtime::Runtime,
    client: &reqwest::Client,
    successes: &[(String, TranslateOutput)],
) -> Vec<Option<String>> {
    let service = PronunciationService::new(client.clone());
    runtime.block_on(async {
        let lookups = successes.iter().map(|(_, output)| {
            let text = output.text.trim().to_string();
            let service = &service;
            async move {
                if !pronunciation::is_word(&text) {
                    return None;
                }
                service
                    .lookup(&text)
                    .await
                    .and_then(|result| result.preferred_ipa().map(str::to_string))
            }
        });
        futures::future::join_all(lookups).await
    })
}

// 文本来源：argv 用空格连接，或唯一参数 `-` 从 stdin 读取。
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

// 目标语言：--to 优先，否则用配置 target；Auto 时由检测结果决定。
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

// 翻译源：--source 优先（支持逗号列表/all），否则使用配置默认源。
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
                "alibaba" => push_unique(&mut ids, "alibaba"),
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
        SourceChoice::Alibaba => vec!["alibaba"],
        SourceChoice::All => config.enabled_provider_ids(),
    };
    if ids.is_empty() {
        return Err("没有启用的翻译源，请打开设置".to_string());
    }
    Ok(ids)
}

// 去重追加，保持配置/参数顺序。
fn push_unique(ids: &mut Vec<&'static str>, id: &'static str) {
    if !ids.contains(&id) {
        ids.push(id);
    }
}

#[derive(Serialize)]
// --json 的稳定输出结构。
struct JsonOutput {
    input: String,
    source: &'static str,
    target: &'static str,
    results: Vec<JsonResult>,
    errors: Vec<JsonError>,
}

#[derive(Serialize)]
// 单平台成功结果。
struct JsonResult {
    provider: String,
    text: String,
    detected_source: Option<&'static str>,
    phonetic: Option<String>,
    latency_ms: Option<u64>,
}

#[derive(Serialize)]
// 单平台失败结果；message 已脱敏。
struct JsonError {
    provider: String,
    kind: &'static str,
    message: String,
}

// 输出 JSON；错误进 errors 数组，不混入 stdout 普通文本。
fn print_json(
    input: &str,
    from: Lang,
    target: Lang,
    successes: &[(String, translate::TranslateOutput)],
    failures: &[(String, ProviderError)],
    phonetics: &[Option<String>],
) {
    let output = JsonOutput {
        input: input.to_string(),
        source: from.code(),
        target: target.code(),
        results: successes
            .iter()
            .enumerate()
            .map(|(index, (provider, result))| JsonResult {
                provider: provider.clone(),
                text: result.text.clone(),
                detected_source: result.detected.map(Lang::code),
                phonetic: phonetics.get(index).cloned().flatten(),
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

// 按字符数截断通知正文，避免按字节切断中文。
fn truncate(text: &str, max: usize) -> String {
    let mut result: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        result.push('…');
    }
    result
}
