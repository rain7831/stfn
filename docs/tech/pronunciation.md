# 英文音标与发音设计

> 状态：待审核 · 目标版本：v0.2.0
> 关联：`v0.2-plan.md`、`architecture.md`、`ui.md`、`providers.md`

## 1. 范围与决策

- 只处理**英文**：不做中文拼音。
- 只处理**单词级**：句子/短语不显示音标。
- 英译中：源英文单词的音标/发音显示在**输入框下方**。
- 中译英：每个结果卡片在**厂商标题和译文之间**显示该译文的音标/发音。
- 厂商 API 不返回英文 IPA；v0.2 通过英文词典查询做后处理，查不到就不显示。
- 音标缺失时不伪造；音频缺失时隐藏播放按钮，音标仍可显示。
- 数据/音频来源需要 attribution。

## 2. Bing 调研结论

- `ttranslatev3` 英译中返回的 `translations[].transliteration` 是**中文译文的拼音**（例如 `Nǐ hǎo`），不是英文 IPA。
- `ttranslatev3` 中译英返回的 `inputTransliteration` 是**中文输入的拼音**，不是英文结果 IPA。
- `tlookupv3` 的 `translations[].transliteration` 同样是中文拼音。
- Bing 词典网页有前端音频播放器，但没有稳定的官方 JSON API；不抓 HTML。
- 因此：**Bing/火山/阿里都不提供英文 IPA**；音标是应用侧词典服务，不是 provider 能力。

## 3. 数据源

### 3.1 主来源：dictionaryapi.dev

```http
GET https://api.dictionaryapi.dev/api/v2/entries/en/<word>
```

- 免费、无 key。
- 返回 `phonetics[]`，每项可能包含：
  - `text`: IPA，例如 `/həˈləʊ/`
  - `audio`: MP3 URL，例如 `.../hello-uk.mp3`
  - `sourceUrl` / `license`
- 仅英文单词有效；404/非单词 → 不显示。
- 数据为 CC BY-SA 3.0；音频为 CC BY / BY-SA；README 与设置页需要 attribution。

### 3.2 音频兜底：有道 -> Edge TTS

- 词典没有真人音频或下载失败时，先请求有道词典发音：
  ```http
  GET https://dict.youdao.com/dictvoice?audio=<word>&type=1   # US
  GET https://dict.youdao.com/dictvoice?audio=<word>&type=2   # UK
  ```
  返回 MP3，无 key；实测约 0.1–0.2s。
- 有道也失败时才用 Edge TTS：
  - crate：`msedge-tts`（`synthesize` 返回 `SynthesizedAudio.audio_bytes`）。
  - 非官方、无 key。
  - 默认音色：US `en-US-AriaNeural`，UK `en-GB-SoniaNeural`。
  - 音色从 `get_voices_list()` 校验；找不到回退第一个 `en-US` / `en-GB` 音色。
  - Edge TTS 调用是 blocking API，放在 `spawn_blocking` 或独立线程，不阻塞 GTK。

### 3.3 播放

- `rodio` 0.23，启用 `mp3` feature。
- 独立音频线程持有输出流，通过 mpsc 接收 `Play(bytes)` / `Stop`；同一时间只播一个音频。
- Linux 上优先选择 cpal 的 `pipewire` / `pulse` 设备，避免直接打开 ALSA 硬件 PCM 绕过系统混音器。
- 音频按 `(word, accent, source)` 缓存字节：LRU 64 条 + 8MiB 总字节 + 1h TTL，仅内存，进程退出即清空。
- 词典命中后预加载可见 accent 的音频；预加载与点击共享单飞请求。
- 播放失败保留播放按钮供重试，不弹全局错误。

## 4. 单词判定与解析

### 4.1 可查询单词

满足以下条件才查询：

- trim 后非空；
- 只包含 ASCII 字母、`'`、`-`；
- 不含空白；
- 长度 1..=64；
- 不是纯数字/符号。

不满足则返回 `None`，UI 不显示。

### 4.2 Accent 解析

```rust
enum Accent { Us, Uk }

struct AccentPronunciation {
    ipa: String,
    audio: Option<AudioSource>,
}

struct Pronunciation {
    us: Option<AccentPronunciation>,
    uk: Option<AccentPronunciation>,
    generic: Option<AccentPronunciation>, // accent 无法判定时
}

enum AudioSource {
    DictionaryUrl(String),
    EdgeTts { voice: &'static str },
}
```

解析规则：

1. 遍历 `phonetics[]`，收集有 `text` 的条目。
2. 用 audio URL 文件名判断 accent：
   - 包含 `-us` → US
   - 包含 `-uk` → UK
   - 其他（如 `-au`）→ 不强行标 US/UK
3. 同一个 accent 有多条时，优先保留有 audio 的条目。
4. 无 accent 标记的 IPA：
   - 若 US/UK 只缺一个，且与已标记 IPA 不同，归到缺失的那个（dictionaryapi 常见：US 文本无音频、UK 有音频）；
   - 否则作为 `generic`。
5. 只有 `generic` 时 UI 显示一条“音标”，不标 US/UK。
6. 只有 audio 没有 IPA 的条目不展示（避免“有读音没音标”）。

### 4.3 音频 URL 与兜底

- 有 `audio` URL：优先下载并缓存真人音频。
- 有 IPA 但无 audio：为对应 accent 生成 `Youdao` 源。
- URL 下载失败：先回退有道；再有道失败才回退 Edge TTS。
- 全部失败：保留播放按钮并提示可重试；IPA 保留。
- 词典查询失败：整个 pronunciation 为 `None`，UI 不显示音标行。

## 5. 服务与缓存

新增 `src/pronunciation.rs`：

```rust
struct PronunciationService {
    client: reqwest::Client,
    cache: Mutex<LruCache<String, Option<Pronunciation>>>,
    audio_cache: Mutex<LruCache<(String, Accent), Arc<Vec<u8>>>>,
    tts: EdgeTts,
}

impl PronunciationService {
    async fn lookup(&self, word: &str) -> Option<Pronunciation>;
    async fn audio(&self, word: &str, accent: Accent, source: &AudioSource) -> Option<Arc<Vec<u8>>>;
}
```

- 词典缓存：容量 256，TTL 24h（内存）；瞬时失败不缓存。
- 音频缓存：LRU 64 条 + 8MiB 总字节 + TTL 1h（内存，不落盘）。
- 预加载：词典结果返回后按 US -> UK 顺序后台预取；点击若仍在下载则等待同一个单飞请求。
- `lookup` 超时 5s；5xx 重试一次；404 直接缓存 `None`。
- 同一单词的并发查询做 in-flight 去重（`HashMap<String, Vec<oneshot::Sender>>` 或单飞 Future）。
- 不记录用户单词到日志；只记录 `provider=dictionary, status, latency`。

## 6. UI 接入

### 6.1 英译中

```text
输入中文或英文，自动翻译              EN → 中
┌────────────────────────────────────────┐
│ hello                                  │
└────────────────────────────────────────┘
US /həˈloʊ/  ▶      UK /həˈləʊ/  ▶
```

- 位置：输入卡片下、分割线之上。
- 组件：可复用的 `PronunciationView`；隐藏时高度 0。
- 数据返回后显示；不显示 spinner，避免结果区跳动。
- 只查输入源英文单词；输入变化时旧结果按 generation 丢弃。
- 空/非单词/词典未命中：不显示。

### 6.2 中译英

```text
翻译结果
┌────────────────────────────────────────┐
│ Bing                                   │
│ US /həˈloʊ/ ▶   UK /həˈləʊ/ ▶          │
│ Hello                                  │
└────────────────────────────────────────┘
```

- 位置：`ProviderRow` 的 header 与 body 之间，靠近厂商标题（margin-top 2–4px）。
- 每个 provider 结果单独查询自己的英文译文；不同译文各自显示；相同译文可重复。
- 只有该结果是单词且词典命中才显示。
- provider 失败/加载中不显示。

### 6.3 交互

- 每个 accent 显示 `US`/`UK` + IPA + 播放按钮。
- 播放中按钮切换为停止/加载；再次点击停止。
- 失败回到 ▶，tooltip 提示“发音加载失败，点击重试”，不隐藏。
- 只有 generic：显示“音标”+播放。
- 文本长度/卡片宽度不足时 IPA 可换行；播放按钮固定在右侧。
- 使用主题色，不使用硬编码灰色；音标次要但可读。

### 6.4 句子级朗读

- 英译中：输入是英文但不是单个单词时，输入卡片下方显示“朗读原文 ▶”。
- 中译英：译文是英文但不是单个单词时，结果卡片 header 显示“朗读译文”按钮。
- 使用 Edge TTS，默认 US；Edge 失败时用有道发音兜底。不预加载、不做句子音频缓存；点击后合成播放。
- 不产生句子级 IPA；失败保留按钮可重试。

## 7. 并发与事件

- 查询在 tokio runtime；结果通过 `UiEvent::Pronunciation { generation, target, result }` 回主线程。
- `target`：
  ```rust
  enum PronunciationTarget {
      Input,
      Provider(String),
  }
  ```
- `generation` 与翻译 generation 对齐；输入/结果变化后旧事件丢弃。
- 中译英：每个 provider 结果返回后各查一次；相同单词走缓存/单飞。
- 英译中：翻译触发时并行查询源单词；翻译失败不影响音标查询。

## 8. Attribution 与隐私

- README 增加：
  - “English phonetics and audio from dictionaryapi.dev (CC BY-SA 3.0)”
  - “Edge TTS is an unofficial Microsoft Read Aloud interface”
- 设置页“关于/数据来源”可放简短说明；v0.2 至少 README 有。
- 查询单词会发送给 dictionaryapi.dev；Edge TTS 会发送待朗读文本。不记录、不遥测。
- 可后续增加“禁用在线发音”开关；v0.2 不做。

## 9. CLI `--speak`

- v0.2 让 `--speak` 可用：
  - 翻译目标为英文时，用 Edge TTS 播放译文，默认 US；
  - 目标为中文时不朗读（当前范围只做英文发音），返回 1 并提示。
- 不新增 `--accent` 参数；默认 US，后续再加。
- `--speak` 失败不吞掉已拿到的翻译结果：先输出译文，再返回非零？v0.2 设计为提示后返回 1。

## 10. 测试

- 解析 fixture：
  - US/UK 都有 IPA/audio；
  - 只有 UK IPA + audio，US IPA 无 audio；
  - 只有一条 generic IPA；
  - 404 / 空 phonetics；
  - 非单词输入不请求。
- 音频：
  - 词典 URL 下载；
  - Edge TTS 兜底（注入 fake synthesizer）；
  - 播放器注入 fake，验证 Play/Stop 命令。
- 缓存/单飞：相同 word 只请求一次。
- UI 手工：英译中单词/句子、中译英不同译文、播放/停止、主题。
- CLI：`--speak` 英文/中文目标。

## 11. 验收

- [ ] 英译中单个英文单词：输入框下方显示 US/UK IPA；有音频可播放。
- [ ] 英译中句子/短语：不显示音标，不请求词典。
- [ ] 中译英每个结果卡片：英文单词各自显示 IPA；译文不同各显示各的。
- [ ] 中译英句子/短语/未命中：该卡片不显示。
- [ ] 词典有 IPA 无音频：有道兜底；再有道失败才 Edge TTS。
- [ ] 单词音频预加载后点击可立即播放；缓存不写磁盘。
- [ ] 非单词英文文本可整句朗读。
- [ ] 不显示中文拼音。
- [ ] 主题切换下 accent 行可读。
- [ ] 缓存命中不重复请求；错误不阻塞翻译。
- [ ] attribution 写入 README。

## 12. 风险

- dictionaryapi.dev 可用性/限流；404 与网络错误区分。
- accent 标记启发式可能误判；只有 generic 时如实显示，不强行标 US/UK。
- Edge TTS 非官方，接口可能变化；失败降级为无播放。
- `rodio` 音频后端依赖系统音频（PipeWire/ALSA）；失败不影响文本。
- 词典/ TTS 都会把文本发给第三方，需在文档说明。
