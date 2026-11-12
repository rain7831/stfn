//! 英文单词音标与发音查询。
//!
//! 厂商 API 都不返回英文 IPA，因此这里对英文单词做后处理：
//! - 词典：`api.dictionaryapi.dev`（免费、无 key，CC BY-SA 3.0）；
//! - 解析 `phonetics[]` 的 IPA 与音频 URL，按 `-us` / `-uk` 文件名标注 accent；
//! - 词典没有真人音频时，用 Edge TTS 兜底；
//! - 词典结果与音频字节都做进程内 LRU + TTL 缓存，并对同一单词做单飞去重。
//!
//! 查不到、非单词、网络失败都返回 `None`，由 UI 静默隐藏，不影响翻译。
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use lru::LruCache;
use serde::Deserialize;
use tokio::sync::oneshot;

use crate::tts::EdgeTts;

const DEFAULT_BASE_URL: &str = "https://api.dictionaryapi.dev/api/v2/entries/en/";
const USER_AGENT: &str = concat!("stfn/", env!("CARGO_PKG_VERSION"));
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
const AUDIO_TIMEOUT: Duration = Duration::from_secs(8);
const DICT_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const AUDIO_TTL: Duration = Duration::from_secs(60 * 60);
const DICT_CAPACITY: usize = 256;
const AUDIO_CAPACITY: usize = 64;
const AUDIO_MAX_BYTES: usize = 5 * 1024 * 1024;
const MAX_WORD_CHARS: usize = 64;

/// 发音口音；只有英文使用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Accent {
    Us,
    Uk,
}

impl Accent {
    /// Edge TTS 默认音色（短名）；音色表可用时会在合成前校验。
    pub fn default_voice(self) -> &'static str {
        match self {
            Self::Us => "en-US-AriaNeural",
            Self::Uk => "en-GB-SoniaNeural",
        }
    }
}

/// 音频来源：词典真人音频 URL，或 Edge TTS 兜底音色。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioSource {
    DictionaryUrl(String),
    EdgeTts { voice: &'static str },
}

/// 单个 accent 的音标与可播放来源。
#[derive(Debug, Clone)]
pub struct AccentPronunciation {
    pub ipa: String,
    pub audio: Option<AudioSource>,
}

/// 一个英文单词的音标结果；`generic` 用于无法判定 US/UK 的情况。
#[derive(Debug, Clone, Default)]
pub struct Pronunciation {
    pub us: Option<AccentPronunciation>,
    pub uk: Option<AccentPronunciation>,
    pub generic: Option<AccentPronunciation>,
}

impl Pronunciation {
    /// 展示/JSON 用的首选 IPA：US > UK > generic。
    pub fn preferred_ipa(&self) -> Option<&str> {
        self.us
            .as_ref()
            .or(self.uk.as_ref())
            .or(self.generic.as_ref())
            .map(|entry| entry.ipa.as_str())
    }
}

/// 只接受单个英文单词：ASCII 字母、撇号、连字符，长度 1..=64。
pub fn is_word(text: &str) -> bool {
    let text = text.trim();
    let count = text.chars().count();
    if count == 0 || count > MAX_WORD_CHARS {
        return false;
    }
    let mut has_letter = false;
    for ch in text.chars() {
        if ch.is_ascii_alphabetic() {
            has_letter = true;
        } else if ch != '\'' && ch != '-' {
            return false;
        }
    }
    has_letter
}

/// 词典 + 音频服务；由 GUI / CLI 共享同一个 HTTP client。
pub struct PronunciationService {
    client: reqwest::Client,
    base_url: String,
    dict: Mutex<LruCache<String, CacheEntry<Option<Pronunciation>>>>,
    audio: Mutex<LruCache<AudioKey, CacheEntry<Arc<Vec<u8>>>>>,
    inflight: Mutex<HashMap<String, Vec<oneshot::Sender<Option<Pronunciation>>>>>,
    tts: Arc<EdgeTts>,
}

// 音频缓存键：单词 + accent + 来源；generic 使用 US。
type AudioKey = (String, Accent, AudioSourceKind);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum AudioSourceKind {
    Dictionary,
    EdgeTts,
}

struct CacheEntry<T> {
    stored_at: Instant,
    value: T,
}

impl CacheEntry<Option<Pronunciation>> {
    fn fresh(&self, now: Instant) -> bool {
        now.duration_since(self.stored_at) < DICT_TTL
    }
}

impl CacheEntry<Arc<Vec<u8>>> {
    fn fresh(&self, now: Instant) -> bool {
        now.duration_since(self.stored_at) < AUDIO_TTL
    }
}

impl PronunciationService {
    /// 生产配置：dictionaryapi.dev + 默认 Edge TTS。
    /// `STFN_DICT_BASE_URL` 仅供集成测试/本地调试覆盖。
    pub fn new(client: reqwest::Client) -> Self {
        let base_url =
            std::env::var("STFN_DICT_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        Self::with_base_url(client, base_url)
    }

    /// 允许集成测试把词典指向本地 mock；base URL 会补齐结尾的 `/`。
    pub fn with_base_url(client: reqwest::Client, base_url: impl Into<String>) -> Self {
        let base_url = base_url.into();
        let base_url = format!("{}/", base_url.trim_end_matches('/'));
        Self {
            client,
            base_url,
            dict: Mutex::new(LruCache::new(non_zero(DICT_CAPACITY))),
            audio: Mutex::new(LruCache::new(non_zero(AUDIO_CAPACITY))),
            inflight: Mutex::new(HashMap::new()),
            tts: Arc::new(EdgeTts::new()),
        }
    }

    /// 查询单词；命中缓存直接返回，未命中做单飞合并。失败缓存为 `None`。
    pub async fn lookup(&self, word: &str) -> Option<Pronunciation> {
        let word = word.trim().to_ascii_lowercase();
        if !is_word(&word) {
            return None;
        }
        if let Some(cached) = self.cached(&word) {
            return cached;
        }

        let (tx, rx) = oneshot::channel();
        let joined_inflight = {
            let mut inflight = lock(&self.inflight);
            if let Some(waiters) = inflight.get_mut(&word) {
                waiters.push(tx);
                true
            } else {
                inflight.insert(word.clone(), vec![tx]);
                false
            }
        };
        if joined_inflight {
            return rx.await.ok().flatten();
        }

        let result = self.fetch(&word).await;
        {
            let mut cache = lock(&self.dict);
            cache.put(
                word.clone(),
                CacheEntry {
                    stored_at: Instant::now(),
                    value: result.clone(),
                },
            );
        }
        let waiters = lock(&self.inflight).remove(&word).unwrap_or_default();
        for waiter in waiters {
            let _ = waiter.send(result.clone());
        }
        result
    }

    /// 取音频字节：为真人音频下载 URL，否则调用 Edge TTS；按 (word, accent) 缓存。
    pub async fn audio(
        &self,
        word: &str,
        accent: Accent,
        source: &AudioSource,
    ) -> Option<Arc<Vec<u8>>> {
        let key = (
            word.trim().to_ascii_lowercase(),
            accent,
            match source {
                AudioSource::DictionaryUrl(_) => AudioSourceKind::Dictionary,
                AudioSource::EdgeTts { .. } => AudioSourceKind::EdgeTts,
            },
        );
        {
            let mut cache = lock(&self.audio);
            if let Some(entry) = cache.get(&key) {
                if entry.fresh(Instant::now()) {
                    return Some(entry.value.clone());
                }
                cache.pop(&key);
            }
        }

        let bytes = match source {
            AudioSource::DictionaryUrl(url) => match self.download(url).await {
                Some(bytes) => bytes,
                // 词典音频 URL 失效（404/5xx/网络错误）时同样退回 Edge TTS，
                // 不能因为第三方 CDN 临时不可用就丢掉整个发音功能。
                None => {
                    self.tts
                        .synthesize_voice(word.trim(), accent.default_voice())
                        .await?
                }
            },
            AudioSource::EdgeTts { voice } => self.tts.synthesize_voice(word.trim(), voice).await?,
        };
        if bytes.is_empty() {
            return None;
        }
        let bytes = Arc::new(bytes);
        lock(&self.audio).put(
            key,
            CacheEntry {
                stored_at: Instant::now(),
                value: bytes.clone(),
            },
        );
        Some(bytes)
    }

    fn cached(&self, word: &str) -> Option<Option<Pronunciation>> {
        let mut cache = lock(&self.dict);
        let now = Instant::now();
        match cache.get(word) {
            Some(entry) if entry.fresh(now) => Some(entry.value.clone()),
            Some(_) => {
                cache.pop(word);
                None
            }
            None => None,
        }
    }

    // 词典请求：5s 超时；5xx/网络错误重试一次；404 与非单词直接缓存 None。
    async fn fetch(&self, word: &str) -> Option<Pronunciation> {
        let url = format!("{}{}", self.base_url, encode_segment(word));
        let mut attempt = 0;
        loop {
            attempt += 1;
            let response = self.client.get(&url).timeout(LOOKUP_TIMEOUT).send().await;

            match response {
                Ok(response) if response.status().is_success() => {
                    let bytes = response.bytes().await.ok()?;
                    let entries: Vec<DictionaryEntry> = serde_json::from_slice(&bytes).ok()?;
                    return parse_entries(&entries);
                }
                Ok(response) if response.status().is_server_error() && attempt == 1 => {
                    continue;
                }
                Ok(_) => return None,
                Err(error)
                    if attempt == 1
                        && (error.is_timeout() || error.is_connect() || error.is_request()) =>
                {
                    continue;
                }
                Err(_) => return None,
            }
        }
    }

    async fn download(&self, url: &str) -> Option<Vec<u8>> {
        let response = self
            .client
            .get(url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .timeout(AUDIO_TIMEOUT)
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        if let Some(length) = response.content_length() {
            if length > AUDIO_MAX_BYTES as u64 {
                return None;
            }
        }
        let bytes = response.bytes().await.ok()?;
        if bytes.is_empty() || bytes.len() > AUDIO_MAX_BYTES {
            return None;
        }
        Some(bytes.to_vec())
    }
}

#[derive(Debug, Deserialize)]
// dictionaryapi.dev 的一个词条；`phonetic` 是部分词条才有的顶层音标兜底。
struct DictionaryEntry {
    #[serde(default)]
    phonetics: Vec<DictionaryPhonetic>,
    #[serde(default)]
    phonetic: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DictionaryPhonetic {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    audio: Option<String>,
}

#[derive(Clone)]
struct Candidate {
    ipa: String,
    audio: Option<String>,
}

// 解析规则见 docs/tech/pronunciation.md §4.2。
fn parse_entries(entries: &[DictionaryEntry]) -> Option<Pronunciation> {
    let mut candidates: Vec<Candidate> = Vec::new();
    for phonetic in entries.iter().flat_map(|entry| entry.phonetics.iter()) {
        let Some(ipa) = non_empty(phonetic.text.as_deref()) else {
            continue;
        };
        let audio = non_empty(phonetic.audio.as_deref()).map(str::to_string);
        match candidates.iter_mut().find(|candidate| candidate.ipa == ipa) {
            Some(existing) => {
                if existing.audio.is_none() {
                    existing.audio = audio;
                }
            }
            None => candidates.push(Candidate {
                ipa: ipa.to_string(),
                audio,
            }),
        }
    }

    if candidates.is_empty() {
        let ipa = entries
            .iter()
            .find_map(|entry| non_empty(entry.phonetic.as_deref()))?;
        return Some(Pronunciation {
            generic: Some(finish_entry(ipa.to_string(), None, None)),
            ..Pronunciation::default()
        });
    }

    let mut us: Option<AccentPronunciation> = None;
    let mut uk: Option<AccentPronunciation> = None;
    let mut unmarked: Vec<Candidate> = Vec::new();

    for candidate in candidates {
        match candidate.audio.as_deref().and_then(accent_from_audio) {
            Some(Accent::Us) => merge_accent(&mut us, candidate, Accent::Us),
            Some(Accent::Uk) => merge_accent(&mut uk, candidate, Accent::Uk),
            None => unmarked.push(candidate),
        }
    }

    // 只缺一个 accent 且未标注 IPA 唯一时，归到缺失的 accent（常见于美音无音频）。
    if us.is_none() {
        if let Some(ipa) = unique_unmarked(&unmarked, uk.as_ref()) {
            us = Some(finish_entry(ipa.clone(), None, Some(Accent::Us)));
            unmarked.retain(|candidate| candidate.ipa != ipa);
        }
    }
    if uk.is_none() {
        if let Some(ipa) = unique_unmarked(&unmarked, us.as_ref()) {
            uk = Some(finish_entry(ipa.clone(), None, Some(Accent::Uk)));
            unmarked.retain(|candidate| candidate.ipa != ipa);
        }
    }

    let generic = if us.is_none() && uk.is_none() {
        unmarked
            .first()
            .map(|candidate| finish_entry(candidate.ipa.clone(), candidate.audio.clone(), None))
    } else {
        None
    };

    if us.is_none() && uk.is_none() && generic.is_none() {
        return None;
    }

    Some(Pronunciation { us, uk, generic })
}

// 同一个 accent 出现多次时优先保留带真人音频的条目。
fn merge_accent(slot: &mut Option<AccentPronunciation>, candidate: Candidate, accent: Accent) {
    let incoming = finish_entry(candidate.ipa, candidate.audio, Some(accent));
    match slot {
        Some(existing) => {
            let existing_audio = matches!(existing.audio, Some(AudioSource::DictionaryUrl(_)));
            let incoming_audio = matches!(incoming.audio, Some(AudioSource::DictionaryUrl(_)));
            if incoming_audio && !existing_audio {
                *slot = Some(incoming);
            }
        }
        None => *slot = Some(incoming),
    }
}

// 无 accent 标记且没有音频时用 Edge TTS；generic 使用 US 音色。
fn finish_entry(ipa: String, audio: Option<String>, accent: Option<Accent>) -> AccentPronunciation {
    let audio = audio.map(AudioSource::DictionaryUrl).or_else(|| {
        Some(AudioSource::EdgeTts {
            voice: accent.unwrap_or(Accent::Us).default_voice(),
        })
    });
    AccentPronunciation { ipa, audio }
}

// 未标注候选去重后只剩一条、且与另一个 accent 不同，才归入缺失 accent。
fn unique_unmarked(unmarked: &[Candidate], other: Option<&AccentPronunciation>) -> Option<String> {
    let other = other?;
    let mut distinct: Vec<&str> = Vec::new();
    for candidate in unmarked {
        if !distinct.contains(&candidate.ipa.as_str()) {
            distinct.push(candidate.ipa.as_str());
        }
    }
    match distinct.as_slice() {
        [ipa] if *ipa != other.ipa => Some((*ipa).to_string()),
        _ => None,
    }
}

// 从音频文件名判断口音；-au 等其他口音不强行标记。
fn accent_from_audio(url: &str) -> Option<Accent> {
    let file = url.rsplit('/').next().unwrap_or(url).to_ascii_lowercase();
    if file.contains("-us.") || file.ends_with("-us") {
        Some(Accent::Us)
    } else if file.contains("-uk.") || file.ends_with("-uk") {
        Some(Accent::Uk)
    } else {
        None
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

// 单词只含 ASCII 字母/撇号/连字符，URL path 编码只需处理撇号。
fn encode_segment(word: &str) -> String {
    url::form_urlencoded::byte_serialize(word.as_bytes()).collect()
}

fn non_zero(capacity: usize) -> NonZeroUsize {
    NonZeroUsize::new(capacity).expect("cache capacity > 0")
}

// 锁中毒时恢复内部值；缓存没有跨线程不变量，恢复比 panic 更稳。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}
