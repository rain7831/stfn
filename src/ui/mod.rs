//! GTK4 / libadwaita 界面。
//!
//! 主窗口是一个 `AdwNavigationView`：主页面 + 应用内设置页。
//! 每次启动都是独立进程/窗口（`NON_UNIQUE`），没有单实例和命令转发。
//! 线程模型：
//! - GTK 主线程只做 UI；
//! - tokio runtime 在后台执行 provider / 词典请求；
//! - 结果经 async_channel 回到 glib 主循环；
//! - generation 过滤过期输入，避免旧结果覆盖新输入。
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::cli::GuiIntent;
use crate::config::{self, Config};
use crate::lang::{self, Detection, Lang};
use crate::pronunciation::{
    self, Accent, AccentPronunciation, AudioSource, Pronunciation, PronunciationService,
};
use crate::providers;
use crate::secrets::{self, Credentials};
use crate::selection;
use crate::translate::{self, ProviderError, TranslateCache, TranslateOutput, TranslateRequest};
use crate::tts::{AudioEvent, AudioPlayer};

// 已保存密钥的占位符；密码框显示为圆点，保存时据此判断“未修改”。
const SENTINEL: &str = "******";

/// 后台任务回到 UI 线程的事件。
enum UiEvent {
    Result {
        generation: u64,
        provider: String,
        result: Result<TranslateOutput, ProviderError>,
    },
    /// 词典查询结果；`target` 决定更新输入区还是某个厂商卡片。
    Pronunciation {
        generation: u64,
        target: PronunciationTarget,
        word: String,
        result: Option<Pronunciation>,
    },
    /// 音频字节已取到；epoch/seq 用于丢弃旧单词或旧播放请求的结果。
    AudioReady {
        token: u64,
        epoch: u64,
        seq: u64,
        result: Option<Arc<Vec<u8>>>,
    },
    /// 音频线程的播放状态。
    Audio(AudioEvent),
}

#[derive(Debug, Clone)]
/// 音标查询的落点：输入区源单词，或某个 provider 的译文。
enum PronunciationTarget {
    Input,
    Provider(String),
}

/// 音标行中的三个位置；generic 用于词典无法判定 US/UK 的情况。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotAccent {
    Us,
    Uk,
    Generic,
}

impl SlotAccent {
    fn label(self) -> &'static str {
        match self {
            Self::Us => "US",
            Self::Uk => "UK",
            Self::Generic => "音标",
        }
    }

    // generic 无法确定口音，用 US 音色/缓存键。
    fn audio_accent(self) -> Accent {
        match self {
            Self::Us | Self::Generic => Accent::Us,
            Self::Uk => Accent::Uk,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 单个 accent 的播放按钮状态。
enum AudioState {
    Idle,
    Loading,
    Playing,
    Unavailable,
}

/// 播放请求：token/epoch 把异步结果映射回当前按钮，避免旧单词覆盖新状态。
#[derive(Debug, Clone)]
struct PlayRequest {
    token: u64,
    epoch: u64,
    word: String,
    accent: Accent,
    source: AudioSource,
}

#[derive(Clone, Default)]
/// accent 行共享给按钮/异步事件的可变状态。
struct SlotData {
    word: Rc<RefCell<String>>,
    source: Rc<RefCell<Option<AudioSource>>>,
    epoch: Rc<Cell<u64>>,
    // 同一按钮的播放序号：旧 Stop/Finished/AudioReady 不覆盖新播放。
    play_seq: Rc<Cell<u64>>,
}

/// 一个 accent 的“标签 + IPA + 播放/停止/加载”控件。
struct AccentSlot {
    token: u64,
    accent: SlotAccent,
    root: gtk::Box,
    tag: gtk::Label,
    ipa: gtk::Label,
    button: gtk::Button,
    spinner: gtk::Spinner,
    data: SlotData,
    state: Cell<AudioState>,
}

impl AccentSlot {
    fn new(token: u64, accent: SlotAccent, on_play: Rc<dyn Fn(PlayRequest)>) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        root.set_valign(gtk::Align::Center);
        root.set_visible(false);

        let tag = gtk::Label::new(Some(accent.label()));
        set_label_font(&tag, 12, true);
        tag.add_css_class("dim-label");
        root.append(&tag);

        let ipa = gtk::Label::new(None);
        set_label_font(&ipa, 12, false);
        ipa.set_xalign(0.0);
        ipa.set_wrap(true);
        root.append(&ipa);

        let button = gtk::Button::from_icon_name("media-playback-start-symbolic");
        button.set_has_frame(false);
        button.set_valign(gtk::Align::Center);
        button.set_tooltip_text(Some(&format!("播放 {} 发音", accent.label())));
        button.set_visible(false);
        root.append(&button);

        let spinner = gtk::Spinner::new();
        spinner.set_size_request(14, 14);
        spinner.set_valign(gtk::Align::Center);
        spinner.set_visible(false);
        root.append(&spinner);

        let data = SlotData::default();
        let click_data = data.clone();
        let on_play_click = on_play.clone();
        button.connect_clicked(move |_| {
            let Some(source) = click_data.source.borrow().clone() else {
                return;
            };
            let word = click_data.word.borrow().clone();
            if word.is_empty() {
                return;
            }
            on_play_click(PlayRequest {
                token,
                epoch: click_data.epoch.get(),
                word,
                accent: accent.audio_accent(),
                source,
            });
        });

        Rc::new(Self {
            token,
            accent,
            root,
            tag,
            ipa,
            button,
            spinner,
            data,
            state: Cell::new(AudioState::Idle),
        })
    }

    fn set_pronunciation(&self, word: &str, entry: &AccentPronunciation) {
        self.tag.set_text(self.accent.label());
        self.ipa.set_text(&entry.ipa);
        *self.data.word.borrow_mut() = word.to_string();
        *self.data.source.borrow_mut() = entry.audio.clone();
        self.data.epoch.set(self.data.epoch.get() + 1);
        self.data.play_seq.set(self.data.play_seq.get() + 1);
        self.root.set_visible(true);
        self.set_state(AudioState::Idle);
    }

    fn set_state(&self, state: AudioState) {
        self.state.set(state);
        self.spinner.stop();
        match state {
            AudioState::Idle => {
                self.spinner.set_visible(false);
                self.button.set_visible(self.data.source.borrow().is_some());
                self.button.set_icon_name("media-playback-start-symbolic");
                self.button
                    .set_tooltip_text(Some(&format!("播放 {} 发音", self.accent.label())));
                self.button.set_sensitive(true);
            }
            AudioState::Loading => {
                self.button.set_visible(false);
                self.spinner.set_visible(true);
                self.spinner.start();
            }
            AudioState::Playing => {
                self.spinner.set_visible(false);
                self.button.set_visible(true);
                self.button.set_icon_name("media-playback-stop-symbolic");
                self.button
                    .set_tooltip_text(Some(&format!("停止 {} 发音", self.accent.label())));
                self.button.set_sensitive(true);
            }
            AudioState::Unavailable => {
                self.spinner.set_visible(false);
                self.button.set_visible(false);
            }
        }
    }

    fn clear(&self) {
        self.root.set_visible(false);
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.button.set_visible(false);
        self.data.epoch.set(self.data.epoch.get() + 1);
        self.data.play_seq.set(self.data.play_seq.get() + 1);
        self.data.word.borrow_mut().clear();
        *self.data.source.borrow_mut() = None;
        self.state.set(AudioState::Idle);
    }

    fn state(&self) -> AudioState {
        self.state.get()
    }
}

/// 一组 US/UK/generic 音标行；隐藏时不占布局空间。
struct PronunciationView {
    root: gtk::Box,
    slots: Vec<Rc<AccentSlot>>,
}

impl PronunciationView {
    fn new(
        registry: &Rc<RefCell<HashMap<u64, Rc<AccentSlot>>>>,
        next_token: &Rc<Cell<u64>>,
        on_play: Rc<dyn Fn(PlayRequest)>,
    ) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 18);
        root.set_valign(gtk::Align::Center);
        root.set_margin_start(12);
        root.set_margin_end(12);
        root.set_visible(false);

        let mut slots = Vec::new();
        for accent in [SlotAccent::Us, SlotAccent::Uk, SlotAccent::Generic] {
            let token = next_token.get() + 1;
            next_token.set(token);
            let slot = AccentSlot::new(token, accent, on_play.clone());
            root.append(&slot.root);
            registry.borrow_mut().insert(token, slot.clone());
            slots.push(slot);
        }

        Rc::new(Self { root, slots })
    }

    /// 有 US/UK 时只显示 US/UK；只有 generic 时显示单条“音标”。
    fn set(&self, word: &str, pronunciation: &Pronunciation) {
        let us = pronunciation.us.as_ref();
        let uk = pronunciation.uk.as_ref();
        let generic = pronunciation.generic.as_ref();
        let use_generic = us.is_none() && uk.is_none();
        // 注意：GtkWidget::is_visible() 会连带检查父级；父级此刻仍是 hidden，
        // 不能用它判断 slot 是否被标记为可见，必须自己记录。
        let mut has_visible = false;

        for slot in &self.slots {
            let entry = match slot.accent {
                SlotAccent::Us => us,
                SlotAccent::Uk => uk,
                SlotAccent::Generic => {
                    if use_generic {
                        generic
                    } else {
                        None
                    }
                }
            };
            match entry {
                Some(entry) => {
                    slot.set_pronunciation(word, entry);
                    has_visible = true;
                }
                None => slot.clear(),
            }
        }
        self.root.set_visible(has_visible);
    }

    fn clear(&self) {
        for slot in &self.slots {
            slot.clear();
        }
        self.root.set_visible(false);
    }

    fn unregister(&self, registry: &Rc<RefCell<HashMap<u64, Rc<AccentSlot>>>>) {
        self.clear();
        let mut registry = registry.borrow_mut();
        for slot in &self.slots {
            registry.remove(&slot.token);
        }
    }
}

/// 单个平台结果卡片：持有控件和当前译文，更新时只改内容不重建行。
struct ProviderRow {
    id: &'static str,
    root: gtk::Box,
    spinner: gtk::Spinner,
    pronunciation: Rc<PronunciationView>,
    body: gtk::Label,
    error: gtk::Label,
    copy: gtk::Button,
    retry: gtk::Button,
    text: Rc<RefCell<String>>,
}

// 结果卡片构建与状态切换（loading/result/error/clear）。
impl ProviderRow {
    /// 构建卡片；retry 回调由外层注入，复制按钮自带 1.2s 勾选反馈。
    fn new(id: &'static str, on_retry: Rc<dyn Fn()>, pronunciation: Rc<PronunciationView>) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        root.add_css_class("card");
        root.set_size_request(-1, 100);
        root.set_margin_bottom(10);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.set_margin_top(6);
        header.set_margin_start(12);
        header.set_margin_end(12);
        header.append(&provider_logo(id));

        let name = gtk::Label::new(Some(provider_label(id)));
        set_label_font(&name, 13, true);
        name.set_xalign(0.0);
        header.append(&name);

        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        header.append(&spacer);

        let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
        copy.set_has_frame(false);
        copy.set_tooltip_text(Some("复制译文"));
        copy.set_visible(false);
        header.append(&copy);

        let retry = gtk::Button::from_icon_name("view-refresh-symbolic");
        retry.set_has_frame(false);
        retry.set_tooltip_text(Some("重试"));
        retry.set_visible(false);
        header.append(&retry);

        let spinner = gtk::Spinner::new();
        spinner.set_size_request(16, 16);
        spinner.set_visible(false);
        header.append(&spinner);
        root.append(&header);

        // 音标行紧贴厂商标题，只有查到单词音标时才占位。
        pronunciation.root.set_margin_top(2);
        pronunciation.root.set_margin_bottom(2);
        root.append(&pronunciation.root);

        let body = gtk::Label::new(None);
        set_label_font(&body, 16, false);
        body.set_wrap(true);
        body.set_selectable(true);
        body.set_xalign(0.0);
        body.set_hexpand(true);
        body.set_margin_start(12);
        body.set_margin_end(12);
        body.set_margin_bottom(12);
        root.append(&body);

        let error = gtk::Label::new(None);
        set_label_font(&error, 12, false);
        error.set_wrap(true);
        error.set_xalign(0.0);
        error.set_margin_start(12);
        error.set_margin_end(12);
        error.set_margin_bottom(10);
        error.add_css_class("error");
        error.set_visible(false);
        root.append(&error);

        let text = Rc::new(RefCell::new(String::new()));
        let retry_callback = on_retry.clone();
        retry.connect_clicked(move |_| retry_callback());

        let copy_text = text.clone();
        let copy_button = copy.clone();
        copy.connect_clicked(move |_| {
            let value = copy_text.borrow().clone();
            if value.is_empty() {
                return;
            }
            if let Some(display) = gtk::gdk::Display::default() {
                display.clipboard().set_text(&value);
            }
            copy_button.set_icon_name("object-select-symbolic");
            let reset = copy_button.clone();
            glib::timeout_add_seconds_local_once(1, move || {
                reset.set_icon_name("edit-copy-symbolic");
            });
        });

        Self {
            id,
            root,
            spinner,
            pronunciation,
            body,
            error,
            copy,
            retry,
            text,
        }
    }

    /// 加载态：保留旧译文并降低透明度，避免闪烁。
    fn set_loading(&self) {
        self.spinner.set_visible(true);
        self.spinner.start();
        self.retry.set_visible(false);
        self.error.set_visible(false);
        self.pronunciation.clear();
        if self.text.borrow().is_empty() {
            self.body.set_text("翻译中…");
        }
        self.body.set_opacity(0.6);
    }

    /// 成功态：写入译文、显示复制按钮；音标由词典后处理单独更新。
    fn set_result(&self, output: &TranslateOutput) {
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.error.set_visible(false);
        self.retry.set_visible(false);
        self.body.set_text(&output.text);
        self.body.set_opacity(1.0);
        *self.text.borrow_mut() = output.text.clone();
        self.copy.set_visible(true);
    }

    /// 失败态：显示脱敏错误并露出重试按钮。
    fn set_error(&self, error: &ProviderError) {
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.error.set_text(&error.message);
        self.error.set_visible(true);
        self.retry.set_visible(true);
        self.pronunciation.clear();
        self.body.set_opacity(0.6);
    }

    /// 清空输入时重置卡片。
    fn clear(&self) {
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.error.set_visible(false);
        self.retry.set_visible(false);
        self.body.set_text("");
        self.body.set_opacity(1.0);
        self.copy.set_visible(false);
        self.pronunciation.clear();
        self.text.borrow_mut().clear();
    }
}

#[derive(Clone)]
/// 密钥字段的保存动作：保留、删除、替换。
enum KeyAction {
    Keep,
    Delete,
    Set(String),
}

#[derive(Clone)]
/// 单个密钥输入行；记录是否原本已有密钥，以及是否来自环境变量。
struct KeyField {
    row: adw::PasswordEntryRow,
    field: &'static str,
    had_key: bool,
    env: bool,
}

// 密钥输入状态机：sentinel 表示已保存，空字符串表示删除。
impl KeyField {
    /// 初始化输入行：env 只读；已有密钥显示 sentinel；聚焦时全选便于替换。
    fn new(title: &str, field: &'static str, had_key: bool, dirty: &Rc<Cell<bool>>) -> Self {
        let row = adw::PasswordEntryRow::new();
        let env = secrets::env_is_set(field);
        let has_key = env || had_key;

        if env {
            row.set_title(&format!("{title}（环境变量）"));
            row.set_text(SENTINEL);
            row.set_editable(false);
            row.set_tooltip_text(Some("由环境变量提供，设置中不可修改"));
        } else {
            row.set_title(&if has_key {
                format!("{title} · 已保存")
            } else {
                title.to_string()
            });
            row.set_text(if has_key { SENTINEL } else { "" });
            row.set_tooltip_text(Some("显示 ****** 表示已保存；全选后输入新密钥，清空则删除"));
        }

        if !env {
            let dirty = dirty.clone();
            let base_title = title.to_string();
            let row_for_title = row.clone();
            row.connect_changed(move |row| {
                dirty.set(true);
                let text = row.text().to_string();
                let suffix = if text == SENTINEL {
                    Some("已保存")
                } else if text.is_empty() {
                    if has_key {
                        Some("将删除")
                    } else {
                        None
                    }
                } else {
                    Some("将更新")
                };
                let title = match suffix {
                    Some(suffix) => format!("{base_title} · {suffix}"),
                    None => base_title.clone(),
                };
                row_for_title.set_title(&title);
            });

            let focus = gtk::EventControllerFocus::new();
            let row_for_focus = row.clone();
            focus.connect_enter(move |_| {
                if row_for_focus.text() == SENTINEL {
                    row_for_focus.select_region(0, -1);
                }
            });
            row.add_controller(focus);
        }

        Self {
            row,
            field,
            had_key: has_key,
            env,
        }
    }

    /// 根据当前输入决定保存动作；容忍用户在 sentinel 后追加内容。
    fn action(&self) -> KeyAction {
        if self.env {
            return KeyAction::Keep;
        }
        let text = self.row.text().to_string();
        if text == SENTINEL {
            return KeyAction::Keep;
        }
        let value = text.strip_prefix(SENTINEL).unwrap_or(&text).to_string();
        if value.is_empty() {
            if self.had_key {
                KeyAction::Delete
            } else {
                KeyAction::Keep
            }
        } else {
            KeyAction::Set(value)
        }
    }

    /// 厂商开启时，判断该字段是否已有或本次填写了有效密钥。
    fn satisfied(&self) -> bool {
        if self.env {
            return true;
        }
        match self.action() {
            KeyAction::Keep => self.had_key,
            KeyAction::Delete => false,
            KeyAction::Set(value) => !value.is_empty(),
        }
    }

    /// 本次是否输入了新密钥（用于判断是否需要本地文件回退授权）。
    fn is_new(&self) -> bool {
        matches!(self.action(), KeyAction::Set(_))
    }
}

/// UI 状态与后台 runtime 的桥接；所有控件和配置都集中在主线程。
struct UiState {
    window: adw::ApplicationWindow,
    nav: adw::NavigationView,
    input: gtk::TextView,
    direction: gtk::Label,
    translate_button: gtk::Button,
    settings_button: gtk::Button,
    status: gtk::Label,
    results_box: gtk::Box,
    rows: RefCell<Vec<ProviderRow>>,
    config: RefCell<Config>,
    credentials: RefCell<Credentials>,
    client: reqwest::Client,
    runtime: tokio::runtime::Runtime,
    cache: Arc<TranslateCache>,
    generation: Cell<u64>,
    debounce: RefCell<Option<glib::SourceId>>,
    event_tx: async_channel::Sender<UiEvent>,
    event_rx: RefCell<Option<async_channel::Receiver<UiEvent>>>,
    settings_page: RefCell<Option<adw::NavigationPage>>,
    settings_dirty: Rc<Cell<bool>>,
    settings_save: RefCell<Option<Rc<dyn Fn()>>>,
    // 英译中源单词音标行（输入卡片下方）。
    input_pronunciation: Rc<PronunciationView>,
    pronunciation: Arc<PronunciationService>,
    // accent token -> 控件；异步播放状态按 token 回写。
    slots: Rc<RefCell<HashMap<u64, Rc<AccentSlot>>>>,
    next_slot: Rc<Cell<u64>>,
    on_play: Rc<dyn Fn(PlayRequest)>,
    play_rx: RefCell<Option<async_channel::Receiver<PlayRequest>>>,
    audio_player: RefCell<Option<AudioPlayer>>,
    // 选区注入文本时抑制 buffer changed 的 debounce。
    injecting: Cell<bool>,
}

// 主窗口生命周期、翻译编排、设置页导航。
impl UiState {
    /// 初始化配置/密钥、主页面、输入区、结果区和后台 runtime。
    fn new(app: &adw::Application) -> Rc<Self> {
        install_css();
        let loaded = config::load();
        let (credentials, _) = secrets::load();

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(12))
            .cookie_store(true)
            .build()
            .expect("创建 HTTP 客户端");

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("STFN")
            .default_width(420)
            .default_height(640)
            .build();
        window.set_size_request(320, 400);

        let window_for_realize = window.clone();
        window.connect_realize(move |_| apply_app_id(&window_for_realize));
        let window_for_map = window.clone();
        window.connect_map(move |_| apply_app_id(&window_for_map));

        // 顶栏：标题 + 设置入口；设置页复用同一个窗口。
        let header = adw::HeaderBar::new();
        let title = gtk::Label::new(Some("STFN"));
        title.add_css_class("heading");
        header.set_title_widget(Some(&title));
        let settings_button = gtk::Button::from_icon_name("emblem-system-symbolic");
        settings_button.set_tooltip_text(Some("设置 (Ctrl+,)"));
        header.pack_end(&settings_button);

        // 输入区：GtkTextView 保证多行文字从左上角开始，而不是垂直居中。
        let input = gtk::TextView::new();
        input.set_hexpand(true);
        input.set_vexpand(true);
        input.set_valign(gtk::Align::Fill);
        input.set_wrap_mode(gtk::WrapMode::WordChar);
        input.set_accepts_tab(false);
        input.set_top_margin(12);
        input.set_bottom_margin(12);
        input.set_left_margin(12);
        input.set_right_margin(12);
        input.add_css_class("input-text");

        let input_scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .min_content_height(150)
            .max_content_height(260)
            .propagate_natural_height(false)
            .has_frame(false)
            .child(&input)
            .build();

        let direction = gtk::Label::new(None);
        set_label_font(&direction, 12, true);
        direction.add_css_class("accent");
        direction.set_xalign(0.0);
        direction.set_visible(false);

        let translate_button = gtk::Button::with_label("翻译");
        translate_button.add_css_class("suggested-action");
        translate_button.set_visible(false);

        // 区块标题 + 方向/手动翻译按钮；输入卡片在标题下方留 10px。
        let input_title = gtk::Label::new(Some("输入中文或英文，自动翻译"));
        set_label_font(&input_title, 15, true);
        input_title.set_xalign(0.0);
        let input_header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        input_header.append(&input_title);
        let input_header_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        input_header_spacer.set_hexpand(true);
        input_header.append(&input_header_spacer);
        input_header.append(&direction);
        input_header.append(&translate_button);

        input_scrolled.set_vexpand(false);
        // 输入卡片：只包含文本区，方向/按钮已经在标题行。
        let input_card = gtk::Box::new(gtk::Orientation::Vertical, 0);
        input_card.set_vexpand(false);
        input_card.add_css_class("card");
        input_card.append(&input_scrolled);

        // 音标行：播放请求先进通道，attach 后在主线程消费。
        let (play_tx, play_rx) = async_channel::unbounded::<PlayRequest>();
        let on_play: Rc<dyn Fn(PlayRequest)> = Rc::new(move |request| {
            let _ = play_tx.try_send(request);
        });
        let slots: Rc<RefCell<HashMap<u64, Rc<AccentSlot>>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let next_slot = Rc::new(Cell::new(0));
        let input_pronunciation = PronunciationView::new(&slots, &next_slot, on_play.clone());

        let input_section = gtk::Box::new(gtk::Orientation::Vertical, 10);
        input_section.set_vexpand(false);
        input_section.append(&input_header);
        input_section.append(&input_card);
        input_section.append(&input_pronunciation.root);

        let divider = gtk::Separator::new(gtk::Orientation::Horizontal);
        divider.set_margin_top(2);
        divider.set_margin_bottom(2);

        let status = gtk::Label::new(None);
        set_label_font(&status, 13, false);
        status.set_wrap(true);
        status.set_xalign(0.0);
        status.set_margin_start(12);
        status.set_margin_end(12);
        status.add_css_class("error");
        status.set_visible(false);

        let results_title = gtk::Label::new(Some("翻译结果"));
        set_label_font(&results_title, 15, true);
        results_title.set_xalign(0.0);

        // 结果区：每个 provider 一张卡片，按内容高度展示，底部留白。
        let results_box = gtk::Box::new(gtk::Orientation::Vertical, 10);

        let results_section = gtk::Box::new(gtk::Orientation::Vertical, 10);
        results_section.append(&results_title);
        results_section.append(&results_box);

        // 主内容：输入区 -> 分割线 -> 翻译结果；最大宽度 640px 防止宽屏拉长。
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_vexpand(true);
        content.set_margin_top(24);
        content.set_margin_bottom(14);
        content.set_margin_start(24);
        content.set_margin_end(24);
        content.append(&input_section);
        content.append(&divider);
        content.append(&status);
        content.append(&results_section);

        let clamp = adw::Clamp::new();
        clamp.set_vexpand(true);
        clamp.set_maximum_size(640);
        clamp.set_tightening_threshold(480);
        clamp.set_child(Some(&content));

        let main_toolbar = adw::ToolbarView::new();
        main_toolbar.add_top_bar(&header);
        main_toolbar.set_content(Some(&clamp));
        let main_page = adw::NavigationPage::new(&main_toolbar, "STFN");

        let nav = adw::NavigationView::new();
        nav.set_pop_on_escape(false);
        nav.push(&main_page);
        window.set_content(Some(&nav));

        // 后台 runtime 与 UI 事件通道：provider / 词典请求在线程池里跑，结果回主线程。
        let (event_tx, event_rx) = async_channel::unbounded();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("创建异步运行时");

        let state = Rc::new(Self {
            window,
            nav,
            input,
            direction,
            translate_button,
            settings_button,
            status,
            results_box,
            rows: RefCell::new(Vec::new()),
            config: RefCell::new(loaded.config),
            credentials: RefCell::new(credentials),
            client: client.clone(),
            runtime,
            cache: Arc::new(TranslateCache::new(128)),
            generation: Cell::new(0),
            debounce: RefCell::new(None),
            event_tx,
            event_rx: RefCell::new(Some(event_rx)),
            settings_page: RefCell::new(None),
            settings_dirty: Rc::new(Cell::new(false)),
            settings_save: RefCell::new(None),
            input_pronunciation,
            pronunciation: Arc::new(PronunciationService::new(client)),
            slots,
            next_slot,
            on_play,
            play_rx: RefCell::new(Some(play_rx)),
            audio_player: RefCell::new(None),
            injecting: Cell::new(false),
        });

        state.attach();
        state.rebuild_rows();
        state.update_auto_ui();
        state
    }

    /// 连接输入、按钮、快捷键、关闭确认和后台结果通道。
    fn attach(self: &Rc<Self>) {
        let state = self.clone();
        self.input.buffer().connect_changed(move |_| {
            state.on_input_changed();
        });

        let state = self.clone();
        self.settings_button.connect_clicked(move |_| {
            state.open_settings();
        });

        let state = self.clone();
        self.translate_button.connect_clicked(move |_| {
            state.translate_now();
        });

        // 快捷键在捕获阶段处理：设置页 Ctrl+S/Esc 优先于输入控件。
        let key_controller = gtk::EventControllerKey::new();
        key_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        let state = self.clone();
        key_controller.connect_key_pressed(move |_, keyval, _, modifier| {
            let ctrl = modifier.contains(gtk::gdk::ModifierType::CONTROL_MASK);
            let settings_open = state.settings_page.borrow().is_some();

            if settings_open {
                if ctrl && keyval.to_unicode() == Some('s') {
                    state.save_settings();
                    return glib::Propagation::Stop;
                }
                if keyval == gtk::gdk::Key::Escape {
                    state.request_close_settings();
                    return glib::Propagation::Stop;
                }
                return glib::Propagation::Proceed;
            }

            if ctrl && (keyval == gtk::gdk::Key::Return || keyval == gtk::gdk::Key::KP_Enter) {
                state.translate_now();
                return glib::Propagation::Stop;
            }
            if ctrl && keyval.to_unicode() == Some('l') {
                state.input.grab_focus();
                return glib::Propagation::Stop;
            }
            if ctrl && keyval.to_unicode() == Some(',') {
                state.open_settings();
                return glib::Propagation::Stop;
            }
            if keyval == gtk::gdk::Key::Escape {
                if text_view_text(&state.input).is_empty() {
                    state.window.close();
                } else {
                    state.input.buffer().set_text("");
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.window.add_controller(key_controller);

        let state = self.clone();
        // 关闭窗口时若设置页有未保存修改，先弹确认。
        self.window.connect_close_request(move |window| {
            if state.settings_page.borrow().is_some() && state.settings_dirty.get() {
                let state = state.clone();
                let window = window.clone();
                let window_for_dialog = window.clone();
                show_discard_dialog(&window, move || {
                    state.settings_dirty.set(false);
                    window_for_dialog.close();
                });
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });

        let state_for_popped = self.clone();
        self.nav.connect_popped(move |_, page| {
            let is_settings = state_for_popped
                .settings_page
                .borrow()
                .as_ref()
                .is_some_and(|settings| settings == page);
            if is_settings {
                *state_for_popped.settings_page.borrow_mut() = None;
                *state_for_popped.settings_save.borrow_mut() = None;
                state_for_popped.settings_dirty.set(false);
                state_for_popped.focus_input_later();
            }
        });

        // async_channel -> glib 主循环；UI 只在这里更新控件。
        if let Some(rx) = self.event_rx.borrow_mut().take() {
            let state = self.clone();
            glib::spawn_future_local(async move {
                while let Ok(event) = rx.recv().await {
                    state.handle_event(event);
                }
            });
        }

        // 播放请求通道：按钮点击在主线程入队，避免在控件回调里直接做网络/解码。
        if let Some(rx) = self.play_rx.borrow_mut().take() {
            let state = self.clone();
            glib::spawn_future_local(async move {
                while let Ok(request) = rx.recv().await {
                    state.play_audio(request);
                }
            });
        }
    }

    /// 输入变化：更新方向 badge，空/不支持时清空；自动翻译开启时重置去抖。
    fn on_input_changed(self: &Rc<Self>) {
        let text = text_view_text(&self.input);
        let detection = lang::detect(&text);
        self.update_direction(detection);
        self.input_pronunciation.clear();
        // 新输入立即让旧 generation 的翻译/音标结果失效，不必等下一次翻译。
        self.generation.set(self.generation.get() + 1);

        if self.injecting.get() {
            return;
        }

        if text.trim().is_empty() {
            self.cancel_debounce();
            self.clear_results();
            self.hide_status();
            return;
        }
        if matches!(detection, Detection::Unsupported) {
            self.cancel_debounce();
            self.clear_results();
            self.show_status("v1 仅支持中英互译");
            return;
        }

        self.hide_status();
        if self.config.borrow().general.auto_translate {
            self.schedule_debounce();
        } else {
            self.cancel_debounce();
        }
    }

    /// 根据本地检测和强制 target 更新 EN/中 方向提示。
    fn update_direction(&self, detection: Detection) {
        let from = match detection {
            Detection::Zh => Some(Lang::Zh),
            Detection::En => Some(Lang::En),
            _ => None,
        };
        let target = self.config.borrow().general.target.resolve(detection);
        if let (Some(from), Some(target)) = (from, target) {
            self.direction
                .set_text(&format!("{} → {}", from.label(), target.label()));
            self.direction.set_visible(true);
        } else {
            self.direction.set_visible(false);
        }
    }

    /// 重置 debounce 计时器；只有最后一次输入会触发翻译。
    fn schedule_debounce(self: &Rc<Self>) {
        self.cancel_debounce();
        let delay = self.config.borrow().general.debounce_ms;
        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(Duration::from_millis(delay), move || {
            if let Some(state) = weak.upgrade() {
                state.translate_now();
            }
        });
        *self.debounce.borrow_mut() = Some(source);
    }

    /// 取消尚未触发的 debounce。
    fn cancel_debounce(&self) {
        if let Some(source) = self.debounce.borrow_mut().take() {
            source.remove();
        }
    }

    /// 翻译入口：检测方向、选源、generation++、构建 provider，再按平台并发请求。
    fn translate_now(self: &Rc<Self>) {
        self.cancel_debounce();
        let text = text_view_text(&self.input).trim().to_string();
        if text.is_empty() {
            self.clear_results();
            return;
        }

        let detection = lang::detect(&text);
        let from = match detection {
            Detection::Zh => Lang::Zh,
            Detection::En => Lang::En,
            Detection::Empty => {
                self.show_status("输入为空，或只包含数字 / 符号");
                return;
            }
            Detection::Unsupported => {
                self.show_status("v1 仅支持中英互译");
                return;
            }
        };
        let target = match self.config.borrow().general.target.resolve(detection) {
            Some(target) => target,
            None => {
                self.show_status("无法判断翻译方向");
                return;
            }
        };

        let ids = self.config.borrow().enabled_provider_ids();
        if ids.is_empty() {
            self.ensure_rows(&[]);
            self.show_status("未启用任何翻译源，请在设置中启用");
            return;
        }

        self.hide_status();
        self.ensure_rows(&ids);
        let generation = self.generation.get() + 1;
        self.generation.set(generation);

        // 英译中：源英文单词的音标与翻译并行查询；新输入先清掉旧音标。
        self.input_pronunciation.clear();
        if from == Lang::En && pronunciation::is_word(&text) {
            self.lookup_input_pronunciation(generation, &text);
        }

        // 先构造 provider；缺凭据的平台直接转成该行的错误状态。
        let built = providers::build_providers(
            &ids,
            &self.config.borrow(),
            &self.credentials.borrow(),
            &self.client,
        );

        for item in built {
            let id = item.id;
            let request = TranslateRequest {
                text: text.clone(),
                from,
                to: target,
            };

            match item.result {
                Ok(provider) => {
                    if let Some(row) = self.rows.borrow().iter().find(|row| row.id == id) {
                        row.set_loading();
                    }

                    let key = translate::cache_key(id, &request);
                    if let Some(output) = self.cache.get(&key) {
                        self.apply_result(generation, id, Ok(output));
                        continue;
                    }

                    let tx = self.event_tx.clone();
                    let cache = self.cache.clone();
                    // 每个 provider 独立任务；结果带 generation，旧结果会被 UI 丢弃。
                    self.runtime.spawn(async move {
                        let (_, result) = translate::run_one(provider, request).await;
                        if let Ok(output) = &result {
                            cache.put(key, output.clone());
                        }
                        let _ = tx
                            .send(UiEvent::Result {
                                generation,
                                provider: id.to_string(),
                                result,
                            })
                            .await;
                    });
                }
                Err(error) => self.apply_result(generation, id, Err(error)),
            }
        }
    }

    /// 只在 provider 集合变化时重建卡片，避免每次输入闪烁。
    fn ensure_rows(self: &Rc<Self>, ids: &[&'static str]) {
        let current: Vec<&'static str> = self.rows.borrow().iter().map(|row| row.id).collect();
        if current.as_slice() == ids {
            return;
        }

        while let Some(child) = self.results_box.first_child() {
            self.results_box.remove(&child);
        }
        {
            let rows = self.rows.borrow();
            for row in rows.iter() {
                row.pronunciation.unregister(&self.slots);
            }
        }
        self.rows.borrow_mut().clear();

        for id in ids {
            let state = self.clone();
            let pronunciation =
                PronunciationView::new(&self.slots, &self.next_slot, self.on_play.clone());
            let row = ProviderRow::new(id, Rc::new(move || state.translate_now()), pronunciation);
            self.results_box.append(&row.root);
            self.rows.borrow_mut().push(row);
        }
    }

    /// 设置保存后按新配置重建结果行。
    fn rebuild_rows(self: &Rc<Self>) {
        let ids = self.config.borrow().enabled_provider_ids();
        self.ensure_rows(&ids);
    }

    /// 校验 generation 后，把结果写到对应平台卡片。
    fn apply_result(
        &self,
        generation: u64,
        provider: &str,
        result: Result<TranslateOutput, ProviderError>,
    ) {
        if generation != self.generation.get() {
            return;
        }
        if let Ok(output) = &result {
            // 中译英：每个厂商的英文译文单独查音标，各自显示。
            if pronunciation::is_word(&output.text) {
                self.lookup_provider_pronunciation(generation, provider, &output.text);
            }
        }
        let rows = self.rows.borrow();
        if let Some(row) = rows.iter().find(|row| row.id == provider) {
            match result {
                Ok(output) => row.set_result(&output),
                Err(error) => row.set_error(&error),
            }
        }
    }

    /// 英译中：查询源单词音标；失败只隐藏，不影响翻译。
    fn lookup_input_pronunciation(&self, generation: u64, word: &str) {
        let service = self.pronunciation.clone();
        let tx = self.event_tx.clone();
        let word = word.to_string();
        self.runtime.spawn(async move {
            let result = service.lookup(&word).await;
            let _ = tx
                .send(UiEvent::Pronunciation {
                    generation,
                    target: PronunciationTarget::Input,
                    word,
                    result,
                })
                .await;
        });
    }

    /// 中译英：按 provider 译文查询音标；不同译文各自显示。
    fn lookup_provider_pronunciation(&self, generation: u64, provider: &str, word: &str) {
        let service = self.pronunciation.clone();
        let tx = self.event_tx.clone();
        let word = word.to_string();
        let provider = provider.to_string();
        self.runtime.spawn(async move {
            let result = service.lookup(&word).await;
            let _ = tx
                .send(UiEvent::Pronunciation {
                    generation,
                    target: PronunciationTarget::Provider(provider),
                    word,
                    result,
                })
                .await;
        });
    }

    /// 把词典结果写到输入区或对应结果卡片；过期 generation 直接丢弃。
    fn apply_pronunciation(
        &self,
        generation: u64,
        target: PronunciationTarget,
        word: &str,
        result: Option<Pronunciation>,
    ) {
        if generation != self.generation.get() {
            return;
        }
        let Some(pronunciation) = result else {
            return;
        };
        match target {
            PronunciationTarget::Input => self.input_pronunciation.set(word, &pronunciation),
            PronunciationTarget::Provider(id) => {
                let rows = self.rows.borrow();
                if let Some(row) = rows.iter().find(|row| row.id == id) {
                    row.pronunciation.set(word, &pronunciation);
                }
            }
        }
    }

    /// 播放按钮点击：正在播放则停止，否则取音频字节；失败隐藏按钮。
    fn play_audio(&self, request: PlayRequest) {
        let Some(slot) = self.slots.borrow().get(&request.token).cloned() else {
            return;
        };
        match slot.state() {
            AudioState::Playing => {
                // 先递增 seq 让在途事件失效，再让音频线程停止当前音频。
                slot.data.play_seq.set(slot.data.play_seq.get() + 1);
                if let Some(player) = self.audio_player.borrow().as_ref() {
                    player.stop(request.token);
                }
                slot.set_state(AudioState::Idle);
            }
            AudioState::Idle => {
                slot.set_state(AudioState::Loading);
                let seq = slot.data.play_seq.get() + 1;
                slot.data.play_seq.set(seq);
                let service = self.pronunciation.clone();
                let tx = self.event_tx.clone();
                let token = request.token;
                let epoch = request.epoch;
                self.runtime.spawn(async move {
                    let audio = service
                        .audio(&request.word, request.accent, &request.source)
                        .await;
                    let _ = tx
                        .send(UiEvent::AudioReady {
                            token,
                            epoch,
                            seq,
                            result: audio,
                        })
                        .await;
                });
            }
            AudioState::Loading | AudioState::Unavailable => {}
        }
    }

    /// 音频字节就绪：epoch/seq 已变化说明单词或播放请求已换，丢弃旧结果。
    fn apply_audio_ready(&self, token: u64, epoch: u64, seq: u64, result: Option<Arc<Vec<u8>>>) {
        let Some(slot) = self.slots.borrow().get(&token).cloned() else {
            return;
        };
        if slot.data.epoch.get() != epoch || slot.data.play_seq.get() != seq {
            return;
        }
        match result {
            Some(bytes) => {
                slot.set_state(AudioState::Playing);
                self.audio_player().play(token, seq, bytes.as_ref().clone());
            }
            None => slot.set_state(AudioState::Unavailable),
        }
    }

    /// 懒启动音频线程；事件通过同一 UI 通道回主线程。
    fn audio_player(&self) -> AudioPlayer {
        let mut player = self.audio_player.borrow_mut();
        if player.is_none() {
            let tx = self.event_tx.clone();
            *player = Some(AudioPlayer::start(move |event| {
                let _ = tx.try_send(UiEvent::Audio(event));
            }));
        }
        player
            .as_ref()
            .expect("audio player initialized above")
            .clone()
    }

    /// 播放线程状态回写；过期 seq 忽略，失败时只隐藏播放按钮，不影响 IPA。
    fn apply_audio_event(&self, event: AudioEvent) {
        let (token, seq) = match event {
            AudioEvent::Finished { token, seq }
            | AudioEvent::Stopped { token, seq }
            | AudioEvent::Failed { token, seq } => (token, seq),
        };
        let Some(slot) = self.slots.borrow().get(&token).cloned() else {
            return;
        };
        if slot.data.play_seq.get() != seq {
            return;
        }
        match event {
            AudioEvent::Finished { .. } | AudioEvent::Stopped { .. } => {
                if slot.state() != AudioState::Unavailable {
                    slot.set_state(AudioState::Idle);
                }
            }
            AudioEvent::Failed { .. } => slot.set_state(AudioState::Unavailable),
        }
    }

    /// `stfn selection`：present 后读取 primary selection，填入后立即翻译。
    ///
    /// GDK 的 primary clipboard 在窗口获得键盘焦点前可能还没有选区数据，
    /// 因此先等 is-active（最多 1s），再按 2s 超时读取。
    fn translate_selection(self: &Rc<Self>) {
        let state = self.clone();
        glib::spawn_future_local(async move {
            for _ in 0..40 {
                if state.window.is_active() {
                    break;
                }
                glib::timeout_future(Duration::from_millis(25)).await;
            }
            let raw = read_primary_text().await;
            state.apply_selection(raw.as_deref());
        });
    }

    fn apply_selection(self: &Rc<Self>, raw: Option<&str>) {
        let Some(raw) = raw else {
            self.show_status(selection::SelectionError::Empty.message());
            return;
        };
        match selection::sanitize(raw) {
            Ok(text) => {
                self.hide_status();
                self.set_input_text(&text);
                self.input.grab_focus();
                self.translate_now();
            }
            Err(error) => self.show_status(error.message()),
        }
    }

    /// 写入注入文本并抑制 buffer changed 的 debounce；光标移到文末。
    fn set_input_text(self: &Rc<Self>, text: &str) {
        self.injecting.set(true);
        let buffer = self.input.buffer();
        buffer.set_text(text);
        let end = buffer.end_iter();
        buffer.place_cursor(&end);
        self.injecting.set(false);
    }

    /// 启动时/返回主页面/选区注入后聚焦输入框，并补一次 idle 聚焦覆盖首次 map。
    /// 如果 idle 触发时已进入设置页，则不再抢焦点。
    fn focus_input_later(self: &Rc<Self>) {
        self.input.grab_focus();
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(state) = weak.upgrade() {
                if state.settings_page.borrow().is_none() {
                    state.input.grab_focus();
                }
            }
        });
    }

    /// 后台结果事件入口。
    fn handle_event(&self, event: UiEvent) {
        match event {
            UiEvent::Result {
                generation,
                provider,
                result,
            } => self.apply_result(generation, &provider, result),
            UiEvent::Pronunciation {
                generation,
                target,
                word,
                result,
            } => self.apply_pronunciation(generation, target, &word, result),
            UiEvent::AudioReady {
                token,
                epoch,
                seq,
                result,
            } => self.apply_audio_ready(token, epoch, seq, result),
            UiEvent::Audio(event) => self.apply_audio_event(event),
        }
    }

    /// 清空所有平台卡片内容。
    fn clear_results(&self) {
        for row in self.rows.borrow().iter() {
            row.clear();
        }
    }

    /// 全局错误/提示（例如未启用翻译源）。
    fn show_status(&self, message: &str) {
        self.status.set_text(message);
        self.status.set_visible(true);
    }

    /// 隐藏全局提示。
    fn hide_status(&self) {
        self.status.set_visible(false);
    }

    /// 自动翻译关闭时显示“翻译”按钮。
    fn update_auto_ui(&self) {
        self.translate_button
            .set_visible(!self.config.borrow().general.auto_translate);
    }

    /// 设置保存后重新加载配置/密钥，刷新 provider 卡片和延迟行为。
    fn reload(self: &Rc<Self>) {
        let loaded = config::load();
        *self.config.borrow_mut() = loaded.config;
        let (credentials, _) = secrets::load();
        *self.credentials.borrow_mut() = credentials;
        if !self.config.borrow().general.auto_translate {
            self.cancel_debounce();
        }
        self.rebuild_rows();
        self.update_auto_ui();
    }

    /// 打开设置页：push 到 NavigationView，不创建浮动窗口。
    fn open_settings(self: &Rc<Self>) {
        if self.settings_page.borrow().is_some() {
            return;
        }

        self.settings_dirty.set(false);
        let (page, save) = build_settings(self);
        *self.settings_save.borrow_mut() = Some(save);
        *self.settings_page.borrow_mut() = Some(page.clone());
        self.nav.push(&page);
    }

    /// 返回主页面并清理 settings_save 回调。
    fn close_settings(self: &Rc<Self>) {
        if self.settings_page.borrow_mut().take().is_some() {
            self.nav.pop();
        }
        *self.settings_save.borrow_mut() = None;
        self.settings_dirty.set(false);
        self.focus_input_later();
    }

    /// 返回/关闭设置页；dirty 时先确认。
    fn request_close_settings(self: &Rc<Self>) {
        if !self.settings_dirty.get() {
            self.close_settings();
            return;
        }

        let state = self.clone();
        show_discard_dialog(self.window.upcast_ref::<gtk::Window>(), move || {
            state.settings_dirty.set(false);
            state.close_settings();
        });
    }

    /// 快捷键 Ctrl+S 调用当前设置页保存回调（先克隆 Rc，避免 RefCell 重入）。
    fn save_settings(&self) {
        let save = self.settings_save.borrow().clone();
        if let Some(save) = save {
            save();
        }
    }
}

/// 构建应用内设置页：通用设置、provider expander、密钥行、保存/脏状态。
fn build_settings(state: &Rc<UiState>) -> (adw::NavigationPage, Rc<dyn Fn()>) {
    state.settings_dirty.set(false);
    let dirty = state.settings_dirty.clone();

    let header = adw::HeaderBar::new();
    header.set_show_back_button(false);
    let title = gtk::Label::new(Some("设置"));
    title.add_css_class("heading");
    header.set_title_widget(Some(&title));

    let back = gtk::Button::from_icon_name("go-previous-symbolic");
    back.set_tooltip_text(Some("返回"));
    header.pack_start(&back);

    let save_button = gtk::Button::with_label("保存");
    save_button.add_css_class("suggested-action");
    header.pack_end(&save_button);

    let page = adw::PreferencesPage::new();

    // 通用设置：自动翻译 + 延迟。
    let general_group = adw::PreferencesGroup::new();
    general_group.set_title("翻译");

    let auto = adw::SwitchRow::new();
    auto.set_title("自动翻译");
    auto.set_subtitle("输入停止后自动翻译");
    auto.set_active(state.config.borrow().general.auto_translate);
    general_group.add(&auto);

    let spin = adw::SpinRow::with_range(200.0, 2000.0, 50.0);
    spin.set_title("延迟");
    spin.set_subtitle("毫秒");
    spin.set_value(state.config.borrow().general.debounce_ms as f64);
    spin.set_sensitive(auto.is_active());
    general_group.add(&spin);
    page.add(&general_group);

    // 翻译源：Bing 开关；火山/阿里用 ExpanderRow 把密钥收在厂商开关下面。
    let providers_group = adw::PreferencesGroup::new();
    providers_group.set_title("翻译源");
    providers_group.set_description(Some("显示 ****** 表示密钥已保存；留空并保存表示删除"));

    let bing = adw::SwitchRow::new();
    bing.set_title("Bing");
    bing.set_subtitle("免密钥，非官方接口");
    bing.set_active(state.config.borrow().providers.bing.enabled);
    bing.add_prefix(&provider_logo("bing"));
    providers_group.add(&bing);

    let credentials = state.credentials.borrow();
    // 密钥行：已有值显示 sentinel；环境变量字段只读。
    let volc_ak = KeyField::new(
        "Access Key",
        "volcengine.access_key",
        credentials.volcengine.access_key.is_some(),
        &dirty,
    );
    let volc_sk = KeyField::new(
        "Secret Key",
        "volcengine.secret_key",
        credentials.volcengine.secret_key.is_some(),
        &dirty,
    );
    let ali_id = KeyField::new(
        "AccessKey ID",
        "alibaba.access_key_id",
        credentials.alibaba.access_key_id.is_some(),
        &dirty,
    );
    let ali_secret = KeyField::new(
        "AccessKey Secret",
        "alibaba.access_key_secret",
        credentials.alibaba.access_key_secret.is_some(),
        &dirty,
    );
    drop(credentials);

    let volc = adw::ExpanderRow::new();
    volc.set_title("火山引擎");
    volc.set_subtitle("机器翻译");
    volc.set_show_enable_switch(true);
    volc.set_enable_expansion(state.config.borrow().providers.volcengine.enabled);
    volc.set_expanded(state.config.borrow().providers.volcengine.enabled);
    volc.add_prefix(&provider_logo("volcengine"));
    volc.add_row(&volc_ak.row);
    volc.add_row(&volc_sk.row);
    providers_group.add(&volc);

    let alibaba = adw::ExpanderRow::new();
    alibaba.set_title("阿里云");
    alibaba.set_subtitle("机器翻译通用版");
    alibaba.set_show_enable_switch(true);
    alibaba.set_enable_expansion(state.config.borrow().providers.alibaba.enabled);
    alibaba.set_expanded(state.config.borrow().providers.alibaba.enabled);
    alibaba.add_prefix(&provider_logo("alibaba"));
    alibaba.add_row(&ali_id.row);
    alibaba.add_row(&ali_secret.row);
    providers_group.add(&alibaba);

    page.add(&providers_group);

    let allow_file = if !secrets::keyring_available() {
        let security = adw::PreferencesGroup::new();
        security.set_title("密钥存储");
        let warning = gtk::Label::new(Some(
            "系统密钥环不可用。如需保存新密钥，请明确允许写入 0600 权限的本地文件。",
        ));
        warning.set_wrap(true);
        warning.set_xalign(0.0);
        warning.add_css_class("dim-label");
        security.add(&warning);

        let allow = adw::SwitchRow::new();
        allow.set_title("允许保存到本地文件");
        allow.set_subtitle("权限 0600；仅在密钥环不可用时使用");
        allow.set_active(false);
        security.add(&allow);
        page.add(&security);
        Some(allow)
    } else {
        None
    };

    let status_group = adw::PreferencesGroup::new();
    status_group.set_title("状态");
    let status_label = gtk::Label::new(None);
    status_label.set_wrap(true);
    status_label.set_xalign(0.0);
    status_label.add_css_class("error");
    status_group.add(&status_label);
    status_group.set_visible(false);
    page.add(&status_group);

    // v0.2 attribution：词典数据与 TTS 兜底都要在 UI 中声明来源。
    let about_group = adw::PreferencesGroup::new();
    about_group.set_title("数据来源");
    let attribution = gtk::Label::new(Some(
        "英文音标与真人发音来自 dictionaryapi.dev（CC BY-SA 3.0）。\n\
         Edge TTS 为非官方 Microsoft Read Aloud 接口，仅在词典无音频时兜底。",
    ));
    attribution.set_wrap(true);
    attribution.set_xalign(0.0);
    attribution.add_css_class("dim-label");
    about_group.add(&attribution);
    page.add(&about_group);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&page));
    let nav_page = adw::NavigationPage::new(&toolbar, "设置");

    let auto_for_spin = spin.clone();
    let dirty_for_auto = dirty.clone();
    auto.connect_active_notify(move |row| {
        auto_for_spin.set_sensitive(row.is_active());
        dirty_for_auto.set(true);
    });

    let dirty_for_spin = dirty.clone();
    spin.connect_value_notify(move |_| dirty_for_spin.set(true));

    let dirty_for_bing = dirty.clone();
    bing.connect_active_notify(move |_| dirty_for_bing.set(true));

    let volc_for_toggle = volc.clone();
    let dirty_for_volc = dirty.clone();
    volc.connect_enable_expansion_notify(move |row| {
        let enabled = row.enables_expansion();
        volc_for_toggle.set_expanded(enabled);
        dirty_for_volc.set(true);
    });

    let alibaba_for_toggle = alibaba.clone();
    let dirty_for_alibaba = dirty.clone();
    alibaba.connect_enable_expansion_notify(move |row| {
        let enabled = row.enables_expansion();
        alibaba_for_toggle.set_expanded(enabled);
        dirty_for_alibaba.set(true);
    });

    if let Some(allow) = allow_file.as_ref() {
        let dirty_for_allow = dirty.clone();
        allow.connect_active_notify(move |_| dirty_for_allow.set(true));
    }

    // 返回按钮：有未保存修改时由 UiState 弹确认。
    let state_for_back = state.clone();
    back.connect_clicked(move |_| state_for_back.request_close_settings());

    let state_for_save = state.clone();
    let allow_file_for_save = allow_file.clone();
    // 保存流程：校验 -> 密钥 keep/delete/replace -> 写 config -> reload -> 返回主页面。
    let save: Rc<dyn Fn()> = Rc::new(move || {
        let mut config = state_for_save.config.borrow().clone();
        config.general.auto_translate = auto.is_active();
        config.general.debounce_ms = spin.value().round() as u64;
        config.providers.bing.enabled = bing.is_active();
        config.providers.volcengine.enabled = volc.enables_expansion();
        config.providers.alibaba.enabled = alibaba.enables_expansion();

        if !(200..=2000).contains(&config.general.debounce_ms) {
            show_settings_error(&status_group, &status_label, "延迟必须在 200–2000ms 之间");
            return;
        }

        if config.providers.volcengine.enabled {
            if !volc_ak.satisfied() {
                show_settings_error(&status_group, &status_label, "请填写火山引擎 Access Key");
                return;
            }
            if !volc_sk.satisfied() {
                show_settings_error(&status_group, &status_label, "请填写火山引擎 Secret Key");
                return;
            }
        }
        if config.providers.alibaba.enabled {
            if !ali_id.satisfied() {
                show_settings_error(&status_group, &status_label, "请填写阿里云 AccessKey ID");
                return;
            }
            if !ali_secret.satisfied() {
                show_settings_error(
                    &status_group,
                    &status_label,
                    "请填写阿里云 AccessKey Secret",
                );
                return;
            }
        }

        let has_new_key = [&volc_ak, &volc_sk, &ali_id, &ali_secret]
            .iter()
            .any(|key| key.is_new());
        if has_new_key && !secrets::keyring_available() {
            let allowed = allow_file_for_save
                .as_ref()
                .is_some_and(|row| row.is_active());
            if !allowed {
                show_settings_error(
                    &status_group,
                    &status_label,
                    "密钥环不可用，请先勾选“允许保存到本地文件”",
                );
                return;
            }
        }

        for key in [&volc_ak, &volc_sk, &ali_id, &ali_secret] {
            match key.action() {
                KeyAction::Keep => {}
                KeyAction::Delete => {
                    if let Err(error) = secrets::delete_secret(key.field) {
                        show_settings_error(&status_group, &status_label, &error);
                        return;
                    }
                }
                KeyAction::Set(value) => {
                    if let Err(error) = secrets::store_secret(key.field, &value) {
                        show_settings_error(&status_group, &status_label, &error);
                        return;
                    }
                }
            }
        }

        if let Err(error) = config::save(&config) {
            show_settings_error(
                &status_group,
                &status_label,
                &format!("保存配置失败：{error}"),
            );
            return;
        }

        state_for_save.reload();
        state_for_save.settings_dirty.set(false);
        state_for_save.close_settings();
    });

    let save_for_button = save.clone();
    save_button.connect_clicked(move |_| save_for_button());

    (nav_page, save)
}

/// 设置页就地错误展示。
fn show_settings_error(group: &adw::PreferencesGroup, label: &gtk::Label, message: &str) {
    label.set_text(message);
    group.set_visible(true);
}

/// 放弃未保存修改的确认对话框。
fn show_discard_dialog<W: IsA<gtk::Window>>(parent: &W, on_discard: impl FnOnce() + 'static) {
    let parent = parent.upcast_ref::<gtk::Window>();
    let dialog = adw::AlertDialog::new(Some("放弃未保存的修改？"), Some("设置尚未保存。"));
    dialog.add_response("cancel", "取消");
    dialog.add_response("discard", "放弃");
    dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.choose(Some(parent), None::<&gio::Cancellable>, move |response| {
        if response == "discard" {
            on_discard();
        }
    });
}

/// 把 Wayland app-id 固定为 stfn，供 niri 的 app-id 规则匹配。
fn apply_app_id(window: &adw::ApplicationWindow) {
    if let Some(surface) = window.surface() {
        if let Some(toplevel) = surface.downcast_ref::<gdk4_wayland::WaylandToplevel>() {
            toplevel.set_application_id("stfn");
        }
    }
}

/// 读取 GtkTextView 全文；缓冲区 start/end iter 用于取整个 buffer。
fn text_view_text(view: &gtk::TextView) -> String {
    let buffer = view.buffer();
    buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), true)
        .to_string()
}

/// 给 GtkTextView 设置 16px 字号和透明背景，避免和结果字号不一致。
fn install_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        "textview.input-text { font-size: 16px; background: transparent; }\n\
         textview.input-text text { font-size: 16px; }",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

/// 用 Pango 属性统一标签字号/粗细，避免依赖零散 CSS class。
fn set_label_font(label: &gtk::Label, size: i32, bold: bool) {
    let attrs = gtk::pango::AttrList::new();
    attrs.insert(gtk::pango::AttrSize::new(size * gtk::pango::SCALE));
    if bold {
        attrs.insert(gtk::pango::AttrInt::new_weight(gtk::pango::Weight::Bold));
    }
    label.set_attributes(Some(&attrs));
}

/// 加载内嵌的官方厂商 logo；找不到映射时退回通用图标。
fn provider_logo(id: &str) -> gtk::Image {
    let data: Option<&'static [u8]> = match id {
        "bing" => Some(&include_bytes!("../../assets/logos/bing.png")[..]),
        "volcengine" => Some(&include_bytes!("../../assets/logos/volcengine.png")[..]),
        "alibaba" => Some(&include_bytes!("../../assets/logos/alibaba.png")[..]),
        _ => None,
    };

    let image = gtk::Image::new();
    let mut loaded = false;
    if let Some(data) = data {
        let bytes = gtk::glib::Bytes::from_static(data);
        if let Ok(texture) = gtk::gdk::Texture::from_bytes(&bytes) {
            image.set_paintable(Some(&texture));
            loaded = true;
        }
    }
    if !loaded {
        image.set_icon_name(Some("application-x-executable-symbolic"));
    }
    image.set_pixel_size(16);
    image.set_valign(gtk::Align::Center);
    image
}

/// provider id -> 中文显示名。
fn provider_label(id: &str) -> &'static str {
    match id {
        "bing" => "Bing",
        "volcengine" => "火山引擎",
        "alibaba" => "阿里云",
        _ => "翻译源",
    }
}

/// 读取 primary selection；2s 超时或失败返回 None。
///
/// primary selection 由合成器持有，窗口获得焦点不会清空它；
/// 不读普通剪贴板、不模拟 Ctrl+C。
async fn read_primary_text() -> Option<String> {
    let display = gtk::gdk::Display::default()?;
    let clipboard = display.primary_clipboard();
    let read = clipboard.read_text_future();
    let timeout = glib::timeout_future(Duration::from_secs(2));
    futures::pin_mut!(read, timeout);
    match futures::future::select(read, timeout).await {
        futures::future::Either::Left((Ok(Some(text)), _)) => Some(text.to_string()),
        _ => None,
    }
}

/// GUI 入口：每次启动独立进程/窗口（`NON_UNIQUE`），按意图进入设置页或选区翻译。
pub fn run(intent: GuiIntent) -> anyhow::Result<()> {
    let app = adw::Application::builder()
        .application_id("dev.stfn.Stfn")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    // 进程内只建一个窗口；重复 activate（非单实例下极少发生）直接忽略。
    let built = Rc::new(Cell::new(false));
    let intent = Rc::new(Cell::new(Some(intent)));
    app.connect_activate(move |app| {
        if built.replace(true) {
            return;
        }

        // ApplicationWindow / HeaderBar / NavigationView / 输入区 / 结果卡片都在 UiState::new 里。
        let state = UiState::new(app);
        state.window.present();
        apply_app_id(&state.window);

        // 意图：Settings push 设置页；Selection present 后读取 primary selection。
        match intent.take().unwrap_or(GuiIntent::Open) {
            GuiIntent::Open => state.focus_input_later(),
            GuiIntent::Settings => state.open_settings(),
            GuiIntent::Selection => {
                state.focus_input_later();
                state.translate_selection();
            }
        }
    });

    // 只把程序名交给 GApplication，避免它把 clap 的 settings/selection 当成待打开文件。
    app.run_with_args(&["stfn"]);
    // 进入 GTK 主循环，直到窗口关闭；返回后整个 GUI 进程结束。
    Ok(())
}
