use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::config::{self, Config};
use crate::lang::{self, Detection, Lang};
use crate::providers;
use crate::secrets::{self, Credentials};
use crate::translate::{self, ProviderError, TranslateCache, TranslateOutput, TranslateRequest};

enum UiEvent {
    Result {
        generation: u64,
        provider: String,
        result: Result<TranslateOutput, ProviderError>,
    },
}

struct ProviderRow {
    id: &'static str,
    root: gtk::Box,
    spinner: gtk::Spinner,
    phonetic: gtk::Label,
    body: gtk::Label,
    error: gtk::Label,
    copy: gtk::Button,
    retry: gtk::Button,
    text: Rc<RefCell<String>>,
}

impl ProviderRow {
    fn new(id: &'static str, on_retry: Rc<dyn Fn()>) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 4);
        root.set_margin_top(10);
        root.set_margin_bottom(10);
        root.set_margin_start(12);
        root.set_margin_end(12);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.set_visible(true);
        let name = gtk::Label::new(Some(provider_label(id)));
        name.set_visible(true);
        name.add_css_class("caption");
        name.add_css_class("dim-label");
        name.set_xalign(0.0);
        header.append(&name);

        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        header.append(&spacer);

        let phonetic = gtk::Label::new(None);
        phonetic.add_css_class("dim-label");
        phonetic.set_visible(false);
        header.append(&phonetic);

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

        let body = gtk::Label::new(None);
        body.set_wrap(true);
        body.set_selectable(true);
        body.set_xalign(0.0);
        body.set_hexpand(true);
        root.append(&body);

        let error = gtk::Label::new(None);
        error.set_wrap(true);
        error.set_xalign(0.0);
        error.add_css_class("error");
        error.set_visible(false);
        root.append(&error);

        root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

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
            phonetic,
            body,
            error,
            copy,
            retry,
            text,
        }
    }

    fn set_loading(&self) {
        self.spinner.set_visible(true);
        self.spinner.start();
        self.retry.set_visible(false);
        self.error.set_visible(false);
        if self.text.borrow().is_empty() {
            self.body.set_text("翻译中…");
        }
        self.body.set_opacity(0.6);
    }

    fn set_result(&self, output: &TranslateOutput) {
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.error.set_visible(false);
        self.retry.set_visible(false);
        self.body.set_text(&output.text);
        self.body.set_opacity(1.0);
        *self.text.borrow_mut() = output.text.clone();
        self.copy.set_visible(true);
        if let Some(phonetic) = &output.phonetics {
            self.phonetic.set_text(phonetic);
            self.phonetic.set_visible(true);
        } else {
            self.phonetic.set_visible(false);
        }
    }

    fn set_error(&self, error: &ProviderError) {
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.error.set_text(&error.message);
        self.error.set_visible(true);
        self.retry.set_visible(true);
        self.body.set_opacity(0.6);
    }

    fn clear(&self) {
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.error.set_visible(false);
        self.retry.set_visible(false);
        self.body.set_text("");
        self.body.set_opacity(1.0);
        self.copy.set_visible(false);
        self.phonetic.set_visible(false);
        self.text.borrow_mut().clear();
    }
}

struct UiState {
    app: adw::Application,
    window: adw::ApplicationWindow,
    input: gtk::Text,
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
    settings_slot: Rc<RefCell<Option<adw::ApplicationWindow>>>,
}

impl UiState {
    fn new(app: &adw::Application) -> Rc<Self> {
        let loaded = config::load();
        let (credentials, _) = secrets::load();

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("STFN")
            .default_width(420)
            .default_height(640)
            .build();
        window.set_size_request(320, 400);

        let window_for_realize = window.clone();
        window.connect_realize(move |_| {
            apply_app_id(&window_for_realize);
        });
        let window_for_map = window.clone();
        window.connect_map(move |_| {
            apply_app_id(&window_for_map);
        });

        let header = adw::HeaderBar::new();
        let settings_button = gtk::Button::from_icon_name("emblem-system-symbolic");
        settings_button.set_tooltip_text(Some("设置 (Ctrl+,)"));
        header.pack_end(&settings_button);

        let input = gtk::Text::new();
        input.set_hexpand(true);
        input.set_vexpand(false);
        input.set_max_length(5000);
        input.set_placeholder_text(Some("输入中文或英文，自动翻译"));
        input.set_input_purpose(gtk::InputPurpose::FreeForm);
        input.set_margin_top(10);
        input.set_margin_bottom(10);
        input.set_margin_start(10);
        input.set_margin_end(10);

        let input_scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .max_content_height(180)
            .propagate_natural_height(true)
            .has_frame(false)
            .child(&input)
            .build();

        let direction = gtk::Label::new(None);
        direction.add_css_class("dim-label");
        direction.set_xalign(0.0);
        direction.set_visible(false);

        let translate_button = gtk::Button::with_label("翻译");
        translate_button.add_css_class("suggested-action");
        translate_button.set_visible(false);

        let input_card = gtk::Box::new(gtk::Orientation::Vertical, 6);
        input_card.add_css_class("card");
        input_card.append(&input_scrolled);

        let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        bottom.append(&direction);
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        bottom.append(&spacer);
        bottom.append(&translate_button);
        input_card.append(&bottom);

        let status = gtk::Label::new(None);
        status.set_wrap(true);
        status.set_xalign(0.0);
        status.add_css_class("error");
        status.set_visible(false);

        let results_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let results_scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .vexpand(true)
            .child(&results_box)
            .build();

        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.set_margin_top(12);
        content.set_margin_bottom(12);
        content.set_margin_start(12);
        content.set_margin_end(12);
        content.append(&input_card);
        content.append(&status);
        content.append(&results_scrolled);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&content));
        window.set_content(Some(&toolbar));

        let (event_tx, event_rx) = async_channel::unbounded();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(12))
            .cookie_store(true)
            .build()
            .expect("创建 HTTP 客户端");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("创建异步运行时");

        let state = Rc::new(Self {
            app: app.clone(),
            window,
            input,
            direction,
            translate_button,
            settings_button,
            status,
            results_box,
            rows: RefCell::new(Vec::new()),
            config: RefCell::new(loaded.config),
            credentials: RefCell::new(credentials),
            client,
            runtime,
            cache: Arc::new(TranslateCache::new(128)),
            generation: Cell::new(0),
            debounce: RefCell::new(None),
            event_tx,
            event_rx: RefCell::new(Some(event_rx)),
            settings_slot: Rc::new(RefCell::new(None)),
        });

        state.attach();
        state.rebuild_rows();
        state.update_auto_ui();

        state
    }

    fn attach(self: &Rc<Self>) {
        let state = self.clone();
        self.input.connect_changed(move |input| {
            state.on_input_changed(input);
        });

        let state = self.clone();
        self.settings_button.connect_clicked(move |_| {
            state.open_settings();
        });

        let state = self.clone();
        self.translate_button.connect_clicked(move |_| {
            state.translate_now();
        });

        let key_controller = gtk::EventControllerKey::new();
        let state = self.clone();
        key_controller.connect_key_pressed(move |_, keyval, _, modifier| {
            let ctrl = modifier.contains(gtk::gdk::ModifierType::CONTROL_MASK);
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
                if state.input.text().is_empty() {
                    state.window.close();
                } else {
                    state.input.set_text("");
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.window.add_controller(key_controller);

        if let Some(rx) = self.event_rx.borrow_mut().take() {
            let state = self.clone();
            glib::spawn_future_local(async move {
                while let Ok(event) = rx.recv().await {
                    state.handle_event(event);
                }
            });
        }
    }

    fn on_input_changed(self: &Rc<Self>, input: &gtk::Text) {
        let text = input.text().to_string();
        let detection = lang::detect(&text);
        self.update_direction(detection);

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

    fn cancel_debounce(&self) {
        if let Some(source) = self.debounce.borrow_mut().take() {
            source.remove();
        }
    }

    fn translate_now(self: &Rc<Self>) {
        self.cancel_debounce();
        let text = self.input.text().trim().to_string();
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

    fn ensure_rows(self: &Rc<Self>, ids: &[&'static str]) {
        let current: Vec<&'static str> = self.rows.borrow().iter().map(|row| row.id).collect();
        if current.as_slice() == ids {
            return;
        }

        while let Some(child) = self.results_box.first_child() {
            self.results_box.remove(&child);
        }
        self.rows.borrow_mut().clear();

        for id in ids {
            let state = self.clone();
            let row = ProviderRow::new(id, Rc::new(move || state.translate_now()));
            self.results_box.append(&row.root);
            self.rows.borrow_mut().push(row);
        }
    }

    fn rebuild_rows(self: &Rc<Self>) {
        let ids = self.config.borrow().enabled_provider_ids();
        self.ensure_rows(&ids);
    }

    fn apply_result(
        &self,
        generation: u64,
        provider: &str,
        result: Result<TranslateOutput, ProviderError>,
    ) {
        if generation != self.generation.get() {
            return;
        }
        let rows = self.rows.borrow();
        if let Some(row) = rows.iter().find(|row| row.id == provider) {
            match result {
                Ok(output) => row.set_result(&output),
                Err(error) => row.set_error(&error),
            }
        }
    }

    fn handle_event(&self, event: UiEvent) {
        match event {
            UiEvent::Result {
                generation,
                provider,
                result,
            } => self.apply_result(generation, &provider, result),
        }
    }

    fn clear_results(&self) {
        for row in self.rows.borrow().iter() {
            row.clear();
        }
    }

    fn show_status(&self, message: &str) {
        self.status.set_text(message);
        self.status.set_visible(true);
    }

    fn hide_status(&self) {
        self.status.set_visible(false);
    }

    fn update_auto_ui(&self) {
        self.translate_button
            .set_visible(!self.config.borrow().general.auto_translate);
    }

    fn reload(self: &Rc<Self>) {
        let loaded = config::load();
        *self.config.borrow_mut() = loaded.config;
        let (credentials, _) = secrets::load();
        *self.credentials.borrow_mut() = credentials;
        self.rebuild_rows();
        self.update_auto_ui();
    }

    fn open_settings(self: &Rc<Self>) {
        if let Some(window) = self.settings_slot.borrow().as_ref() {
            window.present();
            return;
        }

        let window = build_settings(self);
        *self.settings_slot.borrow_mut() = Some(window.clone());
        let slot = self.settings_slot.clone();
        window.connect_close_request(move |_| {
            *slot.borrow_mut() = None;
            glib::Propagation::Proceed
        });
        window.present();
    }
}

fn build_settings(state: &Rc<UiState>) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(&state.app)
        .title("设置")
        .default_width(520)
        .default_height(680)
        .transient_for(&state.window)
        .build();
    window.set_size_request(420, 480);

    let header = adw::HeaderBar::new();
    let cancel = gtk::Button::with_label("取消");
    let save = gtk::Button::with_label("保存");
    save.add_css_class("suggested-action");
    header.pack_start(&cancel);
    header.pack_end(&save);

    let page = adw::PreferencesPage::new();

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
    let spin_for_auto = spin.clone();
    auto.connect_active_notify(move |row| spin_for_auto.set_sensitive(row.is_active()));
    general_group.add(&spin);
    page.add(&general_group);

    let providers_group = adw::PreferencesGroup::new();
    providers_group.set_title("翻译源");

    let bing = adw::SwitchRow::new();
    bing.set_title("Bing");
    bing.set_subtitle("免密钥，非官方接口");
    bing.set_active(state.config.borrow().providers.bing.enabled);
    providers_group.add(&bing);

    let volc = adw::SwitchRow::new();
    volc.set_title("火山引擎");
    volc.set_subtitle("机器翻译");
    volc.set_active(state.config.borrow().providers.volcengine.enabled);
    providers_group.add(&volc);

    let volc_ak = password_row("Access Key", "volcengine.access_key");
    let volc_sk = password_row("Secret Key", "volcengine.secret_key");
    let volc_region = adw::EntryRow::new();
    volc_region.set_title("区域");
    volc_region.set_text(&state.config.borrow().providers.volcengine.region);
    providers_group.add(&volc_ak);
    providers_group.add(&volc_sk);
    providers_group.add(&volc_region);

    let tencent = adw::SwitchRow::new();
    tencent.set_title("腾讯云");
    tencent.set_subtitle("TMT 机器翻译");
    tencent.set_active(state.config.borrow().providers.tencent.enabled);
    providers_group.add(&tencent);

    let tencent_id = password_row("SecretId", "tencent.secret_id");
    let tencent_key = password_row("SecretKey", "tencent.secret_key");
    let tencent_region = adw::EntryRow::new();
    tencent_region.set_title("区域");
    tencent_region.set_text(&state.config.borrow().providers.tencent.region);
    providers_group.add(&tencent_id);
    providers_group.add(&tencent_key);
    providers_group.add(&tencent_region);
    page.add(&providers_group);

    let allow_file = if !secrets::keyring_available() {
        let security = adw::PreferencesGroup::new();
        security.set_title("密钥存储");
        let warning = gtk::Label::new(Some(
            "系统密钥环不可用。如需保存新密钥，请在下方明确允许写入 0600 权限的本地文件。",
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

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&page));
    window.set_content(Some(&toolbar));

    let key_controller = gtk::EventControllerKey::new();
    let save_for_key = save.clone();
    let window_for_key = window.clone();
    key_controller.connect_key_pressed(move |_, keyval, _, modifier| {
        let ctrl = modifier.contains(gtk::gdk::ModifierType::CONTROL_MASK);
        if ctrl && keyval.to_unicode() == Some('s') {
            save_for_key.emit_clicked();
            return glib::Propagation::Stop;
        }
        if keyval == gtk::gdk::Key::Escape {
            window_for_key.close();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(key_controller);

    let window_for_cancel = window.clone();
    cancel.connect_clicked(move |_| window_for_cancel.close());

    let state_for_save = state.clone();
    let window_for_save = window.clone();
    let allow_file_for_save = allow_file.clone();
    let status_group_for_save = status_group.clone();
    let status_label_for_save = status_label.clone();
    save.connect_clicked(move |_| {
        let mut config = state_for_save.config.borrow().clone();
        config.general.auto_translate = auto.is_active();
        config.general.debounce_ms = spin.value().round() as u64;
        config.providers.bing.enabled = bing.is_active();
        config.providers.volcengine.enabled = volc.is_active();
        config.providers.volcengine.region = volc_region.text().trim().to_string();
        config.providers.tencent.enabled = tencent.is_active();
        config.providers.tencent.region = tencent_region.text().trim().to_string();

        if !(200..=2000).contains(&config.general.debounce_ms) {
            show_settings_error(
                &status_group_for_save,
                &status_label_for_save,
                "延迟必须在 200–2000ms 之间",
            );
            return;
        }
        if config.providers.volcengine.enabled && config.providers.volcengine.region.is_empty() {
            show_settings_error(
                &status_group_for_save,
                &status_label_for_save,
                "火山引擎区域不能为空",
            );
            return;
        }
        if config.providers.tencent.enabled && config.providers.tencent.region.is_empty() {
            show_settings_error(
                &status_group_for_save,
                &status_label_for_save,
                "腾讯云区域不能为空",
            );
            return;
        }

        let volc_ak_value = volc_ak.text().to_string();
        let volc_sk_value = volc_sk.text().to_string();
        let tencent_id_value = tencent_id.text().to_string();
        let tencent_key_value = tencent_key.text().to_string();
        let credentials = state_for_save.credentials.borrow();

        if config.providers.volcengine.enabled {
            if !secrets::env_is_set("volcengine.access_key")
                && volc_ak_value.is_empty()
                && credentials.volcengine.access_key.is_none()
            {
                show_settings_error(
                    &status_group_for_save,
                    &status_label_for_save,
                    "请填写火山引擎 Access Key",
                );
                return;
            }
            if !secrets::env_is_set("volcengine.secret_key")
                && volc_sk_value.is_empty()
                && credentials.volcengine.secret_key.is_none()
            {
                show_settings_error(
                    &status_group_for_save,
                    &status_label_for_save,
                    "请填写火山引擎 Secret Key",
                );
                return;
            }
        }
        if config.providers.tencent.enabled {
            if !secrets::env_is_set("tencent.secret_id")
                && tencent_id_value.is_empty()
                && credentials.tencent.secret_id.is_none()
            {
                show_settings_error(
                    &status_group_for_save,
                    &status_label_for_save,
                    "请填写腾讯云 SecretId",
                );
                return;
            }
            if !secrets::env_is_set("tencent.secret_key")
                && tencent_key_value.is_empty()
                && credentials.tencent.secret_key.is_none()
            {
                show_settings_error(
                    &status_group_for_save,
                    &status_label_for_save,
                    "请填写腾讯云 SecretKey",
                );
                return;
            }
        }
        drop(credentials);

        let has_new_secret = [
            &volc_ak_value,
            &volc_sk_value,
            &tencent_id_value,
            &tencent_key_value,
        ]
        .iter()
        .any(|value| !value.is_empty());
        if has_new_secret && !secrets::keyring_available() {
            let allowed = allow_file_for_save
                .as_ref()
                .is_some_and(|row| row.is_active());
            if !allowed {
                show_settings_error(
                    &status_group_for_save,
                    &status_label_for_save,
                    "密钥环不可用，请先勾选“允许保存到本地文件”",
                );
                return;
            }
        }

        for (field, value) in [
            ("volcengine.access_key", volc_ak_value),
            ("volcengine.secret_key", volc_sk_value),
            ("tencent.secret_id", tencent_id_value),
            ("tencent.secret_key", tencent_key_value),
        ] {
            if value.is_empty() {
                continue;
            }
            if let Err(error) = secrets::store_secret(field, &value) {
                show_settings_error(&status_group_for_save, &status_label_for_save, &error);
                return;
            }
        }

        if let Err(error) = config::save(&config) {
            show_settings_error(
                &status_group_for_save,
                &status_label_for_save,
                &format!("保存配置失败：{error}"),
            );
            return;
        }

        state_for_save.reload();
        window_for_save.close();
    });

    window
}

fn password_row(title: &str, field: &str) -> adw::PasswordEntryRow {
    let row = adw::PasswordEntryRow::new();
    row.set_text("");
    if secrets::env_is_set(field) {
        row.set_title(&format!("{title}（环境变量）"));
        row.set_editable(false);
        row.set_tooltip_text(Some("由环境变量提供，设置中不可修改"));
    } else {
        row.set_title(title);
        row.set_tooltip_text(Some("已保存的密钥不会回显；留空表示不修改"));
    }
    row
}

fn show_settings_error(group: &adw::PreferencesGroup, label: &gtk::Label, message: &str) {
    label.set_text(message);
    group.set_visible(true);
}

fn apply_app_id(window: &adw::ApplicationWindow) {
    if let Some(surface) = window.surface() {
        if let Some(toplevel) = surface.downcast_ref::<gdk4_wayland::WaylandToplevel>() {
            toplevel.set_application_id("stfn");
        }
    }
}

fn provider_label(id: &str) -> &'static str {
    match id {
        "bing" => "Bing",
        "volcengine" => "火山引擎",
        "tencent" => "腾讯云",
        _ => "翻译源",
    }
}

pub fn run(settings: bool) -> anyhow::Result<()> {
    let app = adw::Application::builder()
        .application_id("dev.stfn.Stfn")
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();

    let open_settings = Rc::new(Cell::new(settings));
    {
        let open_settings = open_settings.clone();
        app.connect_command_line(move |app, cmdline| {
            if cmdline
                .arguments()
                .iter()
                .any(|arg| arg.to_string_lossy() == "settings")
            {
                open_settings.set(true);
            }
            app.activate();
            glib::ExitCode::SUCCESS
        });
    }

    let state_slot: Rc<RefCell<Option<Rc<UiState>>>> = Rc::new(RefCell::new(None));
    {
        let state_slot = state_slot.clone();
        let open_settings = open_settings.clone();
        app.connect_activate(move |app| {
            let existing = state_slot.borrow().clone();
            if let Some(state) = existing {
                state.window.present();
                apply_app_id(&state.window);
                state.input.grab_focus();
                if open_settings.get() {
                    state.open_settings();
                    open_settings.set(false);
                }
                return;
            }

            let state = UiState::new(app);
            *state_slot.borrow_mut() = Some(state.clone());
            state.window.present();
            apply_app_id(&state.window);
            state.input.grab_focus();
            if open_settings.get() {
                state.open_settings();
                open_settings.set(false);
            }
        });
    }

    app.run();
    Ok(())
}
