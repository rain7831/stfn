//! Edge TTS 合成与音频播放。
//!
//! - `EdgeTts`：调用非官方 Edge Read Aloud 接口，把文本合成为 MP3 字节；
//!   首次使用时向 Edge 拉取音色表并缓存，用于校验/回退音色。
//! - `AudioPlayer`：独立音频线程持有 `rodio` 输出流，UI 通过命令通道控制
//!   播放/停止，通过回调回传结束状态；同一时间只播放一条音频。
//! - `play_blocking`：CLI `--speak` 使用，播放到结束再返回。
//!
//! Edge TTS 是 blocking API，所有网络合成都放在 `spawn_blocking` 中执行，
//! 不阻塞 GTK 主线程。
use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use msedge_tts::tts::SpeechConfig;
use msedge_tts::voice::{get_voices_list, Voice};

use crate::pronunciation::Accent;

// Edge Read Aloud 默认音频格式；与音色建议格式一致。
const AUDIO_FORMAT: &str = "audio-24khz-48kbitrate-mono-mp3";
const SYNTH_TIMEOUT: Duration = Duration::from_secs(15);

/// Edge TTS 客户端；音色表按进程缓存，失败时不做负缓存。
pub struct EdgeTts {
    voices: Mutex<Option<VoiceIndex>>,
}

impl Default for EdgeTts {
    fn default() -> Self {
        Self::new()
    }
}

// 合成入口：blocking API 放进 spawn_blocking，并加超时保护。
impl EdgeTts {
    pub fn new() -> Self {
        // reqwest 启用 rustls/ring，msedge-tts 启用 rustls/aws-lc-rs；
        // 两个 provider 同时存在时必须显式选择进程级默认值，否则首次 TLS 会 panic。
        let _ = rustls::crypto::ring::default_provider().install_default();
        Self {
            voices: Mutex::new(None),
        }
    }

    /// 用 accent 的默认音色合成；供 `--speak` 与词典兜底使用。
    pub async fn synthesize(self: &Arc<Self>, text: &str, accent: Accent) -> Option<Vec<u8>> {
        self.synthesize_voice(text, accent.default_voice()).await
    }

    /// 用指定短音色名合成（如 `en-US-AriaNeural`）。
    pub async fn synthesize_voice(self: &Arc<Self>, text: &str, voice: &str) -> Option<Vec<u8>> {
        if text.trim().is_empty() {
            return None;
        }
        let text = text.to_string();
        let voice = voice.to_string();
        for attempt in 1..=2 {
            let this = self.clone();
            let text_attempt = text.clone();
            let voice_attempt = voice.clone();
            let task = tokio::task::spawn_blocking(move || {
                this.synthesize_blocking(&text_attempt, &voice_attempt)
            });
            match tokio::time::timeout(SYNTH_TIMEOUT, task).await {
                Ok(Ok(Ok(bytes))) => {
                    tracing::debug!(bytes = bytes.len(), "Edge TTS synthesized");
                    return Some(bytes);
                }
                Ok(Ok(Err(error))) => {
                    tracing::debug!(attempt, error = %error, "Edge TTS synthesize failed");
                    if attempt == 1 {
                        // 403 通常是 Edge 端的瞬时限流；换一条新连接再试一次。
                        tokio::time::sleep(Duration::from_millis(700)).await;
                    }
                }
                Ok(Err(error)) => {
                    tracing::debug!(error = %error, "Edge TTS task failed");
                    break;
                }
                Err(_) => {
                    tracing::debug!("Edge TTS synthesis timed out");
                    break;
                }
            }
        }
        None
    }

    fn synthesize_blocking(&self, text: &str, short_voice: &str) -> Result<Vec<u8>, String> {
        let config = SpeechConfig {
            voice_name: self.full_voice_name(short_voice),
            audio_format: AUDIO_FORMAT.to_string(),
            pitch: 0,
            rate: 0,
            volume: 0,
        };
        let mut client = msedge_tts::tts::client::connect().map_err(|error| {
            tracing::debug!(error = %error, "Edge TTS connect failed");
            error.to_string()
        })?;
        let audio = client.synthesize(text, &config).map_err(|error| {
            tracing::debug!(error = %error, "Edge TTS synthesize failed");
            error.to_string()
        })?;
        if audio.audio_bytes.is_empty() {
            Err("Edge TTS returned empty audio".to_string())
        } else {
            Ok(audio.audio_bytes)
        }
    }

    // 短音色名 -> 完整 Voice.Name；音色表不可用时按命名规则拼出完整名。
    fn full_voice_name(&self, short_voice: &str) -> String {
        let mut guard = lock(&self.voices);
        if guard.is_none() {
            *guard = get_voices_list().ok().map(VoiceIndex::new);
        }
        if let Some(index) = guard.as_ref() {
            if let Some(full) = index.by_short.get(short_voice) {
                return full.clone();
            }
            if let Some(full) = index.first_for_locale(short_locale(short_voice)) {
                return full.clone();
            }
        }
        synth_full_name(short_voice)
    }
}

/// 音色表索引：短名 -> 完整名，并保留每个 locale 的第一个音色用于回退。
struct VoiceIndex {
    by_short: HashMap<String, String>,
    first_by_locale: HashMap<String, String>,
}

impl VoiceIndex {
    fn new(voices: Vec<Voice>) -> Self {
        let mut by_short = HashMap::new();
        let mut first_by_locale: HashMap<String, String> = HashMap::new();
        for voice in voices {
            if let Some(short) = voice.short_name.as_ref() {
                by_short.insert(short.clone(), voice.name.clone());
            }
            if let Some(locale) = voice.locale.as_ref() {
                first_by_locale
                    .entry(locale.clone())
                    .or_insert_with(|| voice.name.clone());
            }
        }
        Self {
            by_short,
            first_by_locale,
        }
    }

    fn first_for_locale(&self, locale: &str) -> Option<&String> {
        self.first_by_locale.get(locale)
    }
}

// `en-US-AriaNeural` -> `en-US`；不符合命名规则时原样返回。
fn short_locale(short_voice: &str) -> &str {
    match short_voice.match_indices('-').nth(1) {
        Some((index, _)) => &short_voice[..index],
        None => short_voice,
    }
}

// 音色表不可用时的时间兜底：按 Edge 的完整命名规则拼接。
fn synth_full_name(short_voice: &str) -> String {
    let mut parts = short_voice.splitn(3, '-');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(lang), Some(region), Some(name)) => {
            format!("Microsoft Server Speech Text to Speech Voice ({lang}-{region}, {name})")
        }
        _ => short_voice.to_string(),
    }
}

/// 音频播放命令；token 定位按钮，seq 用于丢弃同一按钮的旧播放事件。
#[derive(Debug)]
pub enum AudioCommand {
    Play {
        token: u64,
        seq: u64,
        bytes: Vec<u8>,
    },
    Stop {
        token: u64,
    },
}

/// 播放线程回传的状态；seq 由 UI 递增，旧事件直接忽略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioEvent {
    Finished { token: u64, seq: u64 },
    Stopped { token: u64, seq: u64 },
    Failed { token: u64, seq: u64 },
}

/// 音频线程句柄；克隆只共享命令发送端。
#[derive(Clone)]
pub struct AudioPlayer {
    tx: std::sync::mpsc::Sender<AudioCommand>,
}

impl AudioPlayer {
    /// 启动独立音频线程；`on_event` 在音频线程里调用，必须自行切回主线程。
    pub fn start<F>(on_event: F) -> Self
    where
        F: Fn(AudioEvent) + Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::channel();
        thread::Builder::new()
            .name("stfn-audio".to_string())
            .spawn(move || run_audio_thread(rx, on_event))
            .expect("创建音频线程");
        Self { tx }
    }

    pub fn play(&self, token: u64, seq: u64, bytes: Vec<u8>) {
        let _ = self.tx.send(AudioCommand::Play { token, seq, bytes });
    }

    pub fn stop(&self, token: u64) {
        let _ = self.tx.send(AudioCommand::Stop { token });
    }
}

// 音频线程主循环：50ms 轮询命令与播放结束，避免阻塞在 recv 上无法感知结束。
fn run_audio_thread<F>(rx: std::sync::mpsc::Receiver<AudioCommand>, on_event: F)
where
    F: Fn(AudioEvent),
{
    let mut sink: Option<rodio::MixerDeviceSink> = None;
    let mut player: Option<rodio::Player> = None;
    let mut current: Option<(u64, u64)> = None;

    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(AudioCommand::Play { token, seq, bytes }) => {
                // 新播放打断旧播放；旧 token/seq 视作结束，UI 按 seq 丢弃过期事件。
                if let Some((prev_token, prev_seq)) = current.take() {
                    if let Some(player) = player.take() {
                        player.stop();
                    }
                    on_event(AudioEvent::Finished {
                        token: prev_token,
                        seq: prev_seq,
                    });
                }
                if sink.is_none() {
                    match rodio::DeviceSinkBuilder::open_default_sink() {
                        Ok(mut device) => {
                            device.log_on_drop(false);
                            sink = Some(device);
                        }
                        Err(error) => {
                            tracing::debug!(error = %error, "audio sink open failed");
                            on_event(AudioEvent::Failed { token, seq });
                            continue;
                        }
                    }
                }
                let mixer = sink.as_ref().expect("audio sink opened above").mixer();
                match rodio::stream::play(mixer, Cursor::new(bytes)) {
                    Ok(handle) => {
                        tracing::debug!(token, seq, "audio playback started");
                        current = Some((token, seq));
                        player = Some(handle);
                    }
                    Err(error) => {
                        tracing::debug!(error = %error, "audio decode/play failed");
                        on_event(AudioEvent::Failed { token, seq })
                    }
                }
            }
            Ok(AudioCommand::Stop { token }) => {
                if let Some((current_token, seq)) = current {
                    if current_token == token {
                        if let Some(player) = player.take() {
                            player.stop();
                        }
                        current = None;
                        on_event(AudioEvent::Stopped { token, seq });
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }

        if let Some(handle) = player.as_ref() {
            if handle.empty() {
                player = None;
                if let Some((token, seq)) = current.take() {
                    on_event(AudioEvent::Finished { token, seq });
                }
            }
        }
    }
}

/// 阻塞播放一段 MP3，直到播完；供 CLI `--speak` 使用。
pub fn play_blocking(bytes: Vec<u8>) -> Result<(), String> {
    let mut sink = rodio::DeviceSinkBuilder::open_default_sink()
        .map_err(|_| "无法打开音频输出设备".to_string())?;
    sink.log_on_drop(false);
    let player = rodio::stream::play(sink.mixer(), Cursor::new(bytes))
        .map_err(|_| "无法解码朗读音频".to_string())?;
    player.sleep_until_end();
    Ok(())
}

// 锁中毒时取回内部值；缓存/音色表不存在跨线程不变量，恢复比 panic 更稳。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}
