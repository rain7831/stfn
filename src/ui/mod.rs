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

const SENTINEL: &str = "******";

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
        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        root.add_css_class("card");
        root.set_vexpand(true);
        root.set_margin_bottom(10);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.set_margin_top(12);
        header.set_margin_start(12);
        header.set_margin_end(12);
        header.append(&provider_logo(id));

        let name = gtk::Label::new(Some(provider_label(id)));
        set_label_font(&name, 14, true);
        name.set_xalign(0.0);
        header.append(&name);

        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        header.append(&spacer);

        let phonetic = gtk::Label::new(None);
        set_label_font(&phonetic, 12, false);
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
        set_label_font(&body, 16, false);
        body.set_wrap(true);
        body.set_selectable(true);
        body.set_xalign(0.0);
        body.set_hexpand(true);
        body.set_vexpand(true);
        body.set_valign(gtk::Align::Start);
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

#[derive(Clone)]
enum KeyAction {
    Keep,
    Delete,
    Set(String),
}

#[derive(Clone)]
struct KeyField {
    row: adw::PasswordEntryRow,
    field: &'static str,
    had_key: bool,
    env: bool,
}

impl KeyField {
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

    fn is_new(&self) -> bool {
        matches!(self.action(), KeyAction::Set(_))
    }
}

struct UiState {
    window: adw::ApplicationWindow,
    nav: adw::NavigationView,
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
    settings_page: RefCell<Option<adw::NavigationPage>>,
    settings_dirty: Rc<Cell<bool>>,
    settings_save: RefCell<Option<Rc<dyn Fn()>>>,
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
        window.connect_realize(move |_| apply_app_id(&window_for_realize));
        let window_for_map = window.clone();
        window.connect_map(move |_| apply_app_id(&window_for_map));

        let header = adw::HeaderBar::new();
        let title = gtk::Label::new(Some("STFN"));
        title.add_css_class("heading");
        header.set_title_widget(Some(&title));
        let settings_button = gtk::Button::from_icon_name("emblem-system-symbolic");
        settings_button.set_tooltip_text(Some("设置 (Ctrl+,)"));
        header.pack_end(&settings_button);

        let input = gtk::Text::new();
        input.set_hexpand(true);
        input.set_vexpand(false);
        input.set_max_length(5000);
        input.set_input_purpose(gtk::InputPurpose::FreeForm);
        set_text_font(&input, 15);
        input.set_margin_top(12);
        input.set_margin_bottom(12);
        input.set_margin_start(12);
        input.set_margin_end(12);

        let input_scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .min_content_height(150)
            .max_content_height(260)
            .propagate_natural_height(true)
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

        let input_title = gtk::Label::new(Some("输入"));
        set_label_font(&input_title, 15, true);
        let input_header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        input_header.append(&input_title);
        let input_header_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        input_header_spacer.set_hexpand(true);
        input_header.append(&input_header_spacer);
        input_header.append(&direction);
        input_header.append(&translate_button);

        let input_card = gtk::Box::new(gtk::Orientation::Vertical, 0);
        input_card.add_css_class("card");
        input_card.append(&input_scrolled);

        let input_section = gtk::Box::new(gtk::Orientation::Vertical, 10);
        input_section.append(&input_header);
        input_section.append(&input_card);

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

        let results_box = gtk::Box::new(gtk::Orientation::Vertical, 10);
        results_box.set_homogeneous(true);
        results_box.set_vexpand(true);

        let results_section = gtk::Box::new(gtk::Orientation::Vertical, 10);
        results_section.set_vexpand(true);
        results_section.append(&results_title);
        results_section.append(&results_box);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_vexpand(true);
        content.set_margin_top(14);
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
            client,
            runtime,
            cache: Arc::new(TranslateCache::new(128)),
            generation: Cell::new(0),
            debounce: RefCell::new(None),
            event_tx,
            event_rx: RefCell::new(Some(event_rx)),
            settings_page: RefCell::new(None),
            settings_dirty: Rc::new(Cell::new(false)),
            settings_save: RefCell::new(None),
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

        let state = self.clone();
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
            }
        });

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
        if !self.config.borrow().general.auto_translate {
            self.cancel_debounce();
        }
        self.rebuild_rows();
        self.update_auto_ui();
    }

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

    fn close_settings(&self) {
        if self.settings_page.borrow_mut().take().is_some() {
            self.nav.pop();
        }
        *self.settings_save.borrow_mut() = None;
        self.settings_dirty.set(false);
    }

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

    fn save_settings(&self) {
        let save = self.settings_save.borrow().clone();
        if let Some(save) = save {
            save();
        }
    }
}

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

    let state_for_back = state.clone();
    back.connect_clicked(move |_| state_for_back.request_close_settings());

    let state_for_save = state.clone();
    let allow_file_for_save = allow_file.clone();
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

fn show_settings_error(group: &adw::PreferencesGroup, label: &gtk::Label, message: &str) {
    label.set_text(message);
    group.set_visible(true);
}

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

fn apply_app_id(window: &adw::ApplicationWindow) {
    if let Some(surface) = window.surface() {
        if let Some(toplevel) = surface.downcast_ref::<gdk4_wayland::WaylandToplevel>() {
            toplevel.set_application_id("stfn");
        }
    }
}

fn set_label_font(label: &gtk::Label, size: i32, bold: bool) {
    let attrs = gtk::pango::AttrList::new();
    attrs.insert(gtk::pango::AttrSize::new(size * gtk::pango::SCALE));
    if bold {
        attrs.insert(gtk::pango::AttrInt::new_weight(gtk::pango::Weight::Bold));
    }
    label.set_attributes(Some(&attrs));
}

fn set_text_font(text: &gtk::Text, size: i32) {
    let attrs = gtk::pango::AttrList::new();
    attrs.insert(gtk::pango::AttrSize::new(size * gtk::pango::SCALE));
    text.set_attributes(Some(&attrs));
}

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
    image.set_pixel_size(18);
    image.set_valign(gtk::Align::Center);
    image
}

fn provider_label(id: &str) -> &'static str {
    match id {
        "bing" => "Bing",
        "volcengine" => "火山引擎",
        "alibaba" => "阿里云",
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
