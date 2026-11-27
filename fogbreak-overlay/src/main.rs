//! Fogbreak Design in the browser (wasm32): `<canvas data-host="page">` host over full `FigApp`.
#![cfg_attr(target_arch = "wasm32", allow(dead_code))]

#[cfg(target_arch = "wasm32")]
pub mod host_bridge;
#[cfg(target_arch = "wasm32")]
pub mod kept;
#[cfg(target_arch = "wasm32")]
pub mod local_fonts;
#[cfg(target_arch = "wasm32")]
pub mod pasted;
#[cfg(target_arch = "wasm32")]
pub mod recovery;
pub mod theme;

#[cfg(target_arch = "wasm32")]
pub(crate) mod web {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    use figcraft_engine::Session;
    use figcraft_ui_egui::{ContentsFuture, ControlRequest, FigApp, Services};
    use wasm_bindgen::JsCast as _;

    use crate::{host_bridge, kept, local_fonts, pasted, recovery, theme};

    const LOADING_ID: &str = "fogbreak_loading";
    const STOPPED_ID: &str = "fogbreak_stopped";
    const NAME_KEY: &str = "fogbreak-fig.userName";
    const RECOVERY_KEY: &str = "fogbreak-fig.recovery";
    const REVOKE_DOWNLOAD_MS: i32 = 40_000;
    const PICKER_FOCUS_SETTLE_MS: i32 = 1_000;

    thread_local! {
        static UNSAVED: RefCell<recovery::Unsaved> = RefCell::default();
    }

    pub struct WebShell {
        fig: FigApp,
        canvas: web_sys::HtmlCanvasElement,
        actions: host_bridge::ActionQueue,
        control_pending: Rc<RefCell<Vec<ControlRequest>>>,
        dark_state: Arc<Mutex<bool>>,
        last_dirty: Option<bool>,
    }

    impl eframe::App for WebShell {
        fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            UNSAVED.with(|u| u.try_borrow_mut().map(|mut u| u.follow(&self.fig.session)).ok());
            host_bridge::process_actions(&mut self.fig, ctx, &self.canvas, &self.actions, &self.dark_state);
            let queued = std::mem::take(&mut *self.control_pending.borrow_mut());
            self.fig.control_queue.extend(queued);
            figcraft_ui_egui::windows::guard(&mut self.fig, ctx, |fig| fig.background_tick(ctx));
            let dark = *self.dark_state.lock().unwrap_or_else(|e| e.into_inner());
            theme::apply_palette(ctx, dark);
            let dirty = self.fig.session.docs.iter().any(|d| d.is_dirty());
            host_bridge::emit_dirty(&self.canvas, dirty, &mut self.last_dirty);
            self.fig.close_path(ctx);
        }

        fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
            self.fig.before_input(ctx);
            let events = figcraft_ui_egui::windows::take_input(&mut self.fig, raw_input.viewport_id);
            raw_input.events.extend(events);
            pasted::begin_frame(&mut self.fig, raw_input, ctx);
        }

        fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
            let queued = std::mem::take(&mut *self.control_pending.borrow_mut());
            self.fig.control_queue.extend(queued);
            self.fig.ui(ui);
            let dark = *self.dark_state.lock().unwrap_or_else(|e| e.into_inner());
            theme::apply_palette(ui.ctx(), dark);
            local_fonts::load_wanted(ui.ctx());
            if local_fonts::take_loaded() {
                self.fig.run("text.refit", serde_json::json!({}));
            }
        }
    }

    struct WebServices {
        reduce_motion: Option<web_sys::MediaQueryList>,
        ctx: egui::Context,
        canvas: web_sys::HtmlCanvasElement,
    }

    impl Services for WebServices {
        fn pick_open_contents(&mut self, _title: &str, exts: &[&str]) -> Option<ContentsFuture> {
            let accept = exts.iter().map(|e| format!(".{e}")).collect::<Vec<_>>().join(",");
            Some(Box::pin(async move {
                pick_file(&accept).await.unwrap_or_else(|e| {
                    log::error!("couldn't open a file: {e:?}");
                    None
                })
            }))
        }

        fn downloads(&self) -> bool {
            true
        }

        fn download(&mut self, name: &str, bytes: &[u8]) -> bool {
            host_bridge::deliver_save(&self.canvas, name, false, bytes);
            true
        }

        fn clipboard_set_text(&mut self, s: &str) {
            figcraft_ui_egui::egui_copy_text(&self.ctx, s);
        }

        fn clipboard_set_png(&mut self, w: u32, h: u32, rgba: &[u8]) {
            figcraft_ui_egui::egui_copy_png(&self.ctx, w, h, rgba);
        }

        fn clipboard_get_png(&mut self) -> Option<Vec<u8>> {
            pasted::take_png()
        }

        fn open_url(&mut self, url: &str) {
            if let Some(window) = web_sys::window() {
                let _ = window.open_with_url_and_target_and_features(url, "_blank", "noopener,noreferrer");
            }
        }

        fn open_url_here(&mut self, url: &str) {
            if let Some(window) = web_sys::window() {
                let _ = window.location().set_href(url);
            }
        }

        fn reduce_motion(&self) -> bool {
            self.reduce_motion.as_ref().is_some_and(web_sys::MediaQueryList::matches)
        }

        fn save_prefs(&mut self, prefs: &figcraft_engine::Prefs) {
            if let Some(Err(e)) = local_storage().map(|s| s.set_item(NAME_KEY, &prefs.user_name)) {
                log::warn!("couldn't keep your name: {e:?}");
            }
        }

        fn system_language(&self) -> Option<String> {
            web_sys::window().and_then(|w| w.navigator().language())
        }

        fn gamepad_held(&mut self) -> u32 {
            let Some(pads) = web_sys::window().and_then(|w| w.navigator().get_gamepads().ok()) else { return 0 };
            let mut held = 0;
            for pad in pads.iter().filter_map(|p| p.dyn_into::<web_sys::Gamepad>().ok()) {
                if pad.mapping() != web_sys::GamepadMappingType::Standard {
                    continue;
                }
                for (i, b) in pad.buttons().iter().take(32).enumerate() {
                    if b.dyn_into::<web_sys::GamepadButton>().is_ok_and(|b| b.pressed()) {
                        held |= 1 << i;
                    }
                }
            }
            held
        }

        fn can_ask_for_local_fonts(&self) -> bool {
            local_fonts::can_ask()
        }

        fn ask_for_local_fonts(&mut self) {
            local_fonts::ask(self.ctx.clone());
        }
    }

    fn build_shell(cc: &eframe::CreationContext<'_>, canvas: web_sys::HtmlCanvasElement, dark: bool) -> WebShell {
        figcraft_ui_egui::framework::install_headless(&cc.egui_ctx);
        let reduce_motion = web_sys::window().and_then(|w| w.match_media("(prefers-reduced-motion: reduce)").ok().flatten());
        let mut session = Session::new();
        session.prefs.theme = if dark { "dark" } else { "light" }.to_string();
        kept::restore_name(&mut session, local_storage().and_then(|s| s.get_item(NAME_KEY).ok().flatten()));
        let recovered = restore_unsaved(&mut session);
        let net_ctx = cc.egui_ctx.clone();
        session.live.install(Box::new(figcraft_collab_net::NetLink::new(Arc::new(move || net_ctx.request_repaint()))));

        let mut fig = FigApp::new(
            session,
            Box::new(WebServices {
                reduce_motion,
                ctx: cc.egui_ctx.clone(),
                canvas: canvas.clone(),
            }),
        );
        fig.note_recovered(recovered);
        if let Err(e) = pasted::install(cc.egui_ctx.clone()) {
            log::error!("couldn't listen for pasted images: {e:?}");
        }
        if let Some(author) = query_param("author").map(|a| a.trim().to_string()).filter(|a| !a.is_empty()) {
            let _ = fig.session.execute("pref.userName", &serde_json::json!({ "name": author }));
        }
        if let Some(link) = join_invite_from_hash() {
            figcraft_ui_egui::live::open_invite(&mut fig, &link);
        }
        theme::apply_palette(&cc.egui_ctx, dark);

        let control_pending: Rc<RefCell<Vec<ControlRequest>>> = Rc::new(RefCell::new(Vec::new()));
        let actions = host_bridge::ActionQueue::default();
        let dark_state = Arc::new(Mutex::new(dark));
        let host_ctx = host_bridge::HostContext {
            canvas: canvas.clone(),
            ctx: cc.egui_ctx.clone(),
            control_pending: control_pending.clone(),
            actions: actions.clone(),
            dark_state: dark_state.clone(),
            next_id: Rc::new(Cell::new(0)),
        };
        host_bridge::listen_open_and_theme(&host_ctx);
        host_bridge::install_host_rpc(&host_ctx);
        host_bridge::emit_ready(&canvas);

        WebShell {
            fig,
            canvas,
            actions,
            control_pending,
            dark_state,
            last_dirty: None,
        }
    }

    pub fn run() {
        eframe::WebLogger::init(log::LevelFilter::Info).ok();
        keep_unsaved_on_panic();
        let dark = theme::initial_dark_mode();
        theme::sync_document_theme(dark);

        wasm_bindgen_futures::spawn_local(async move {
            let Some(canvas) = host_bridge::find_page_canvas() else {
                log::error!("missing <canvas data-host=\"page\" data-engine=\"fogbreak-fig\">");
                return;
            };
            let canvas_for_err = canvas.clone();
            let mut options = eframe::WebOptions::default();
            if query_has("webgl")
                && let eframe::egui_wgpu::WgpuSetup::CreateNew(create) = &mut options.wgpu_options.wgpu_setup
            {
                create.instance_descriptor.backends = eframe::wgpu::Backends::GL;
            }
            let started = eframe::WebRunner::new()
                .start(
                    canvas.clone(),
                    options,
                    Box::new(move |cc| Ok(Box::new(build_shell(cc, canvas, dark)))),
                )
                .await;
            finish_boot(&canvas_for_err, started);
        });
    }

    fn finish_boot(canvas: &web_sys::HtmlCanvasElement, started: Result<(), wasm_bindgen::JsValue>) {
        let msg = started.as_ref().err().map(|e| format!("{e:?}"));
        if let Some(el) = web_sys::window().and_then(|w| w.document()).and_then(|d| d.get_element_by_id(LOADING_ID)) {
            match &msg {
                None => el.remove(),
                Some(err) => el.set_text_content(Some(&format!("Failed to start: {err}"))),
            }
        }
        if let Some(err) = msg {
            host_bridge::emit_failed(canvas, &err);
        }
    }

    fn keep_unsaved_on_panic() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            previous(info);
            ONCE.call_once(|| {
                let (unsaved, files) = UNSAVED.with(|u| u.try_borrow().map(|u| (u.count(), u.files())).unwrap_or_default());
                let kept = local_storage().map_or(0, |s| recovery::store(files, |v| s.set_item(RECOVERY_KEY, v).is_ok()));
                show_stopped(&figcraft_ui_egui::browser_stopped(unsaved, kept));
            });
        }));
    }

    fn restore_unsaved(session: &mut Session) -> usize {
        let Some(storage) = local_storage() else { return 0 };
        let kept = storage.get_item(RECOVERY_KEY).ok().flatten();
        let _ = storage.remove_item(RECOVERY_KEY);
        recovery::restore(session, kept)
    }

    fn show_stopped(text: &str) {
        let Some(document) = web_sys::window().and_then(|w| w.document()) else { return };
        let (Ok(el), Some(body)) = (document.create_element("div"), document.body()) else { return };
        el.set_id(STOPPED_ID);
        el.set_text_content(Some(text));
        let _ = body.append_child(&el);
    }

    fn local_storage() -> Option<web_sys::Storage> {
        web_sys::window().and_then(|w| w.local_storage().ok().flatten())
    }

    fn query() -> String {
        web_sys::window().and_then(|w| w.location().search().ok()).unwrap_or_default()
    }

    fn query_param(key: &str) -> Option<String> {
        web_sys::UrlSearchParams::new_with_str(&query()).ok()?.get(key)
    }

    fn query_has(key: &str) -> bool {
        web_sys::UrlSearchParams::new_with_str(&query()).is_ok_and(|p| p.has(key))
    }

    fn join_invite_from_hash() -> Option<String> {
        let hash = web_sys::window()?.location().hash().ok()?;
        let rest = hash.strip_prefix("#join=")?;
        let decoded = js_sys::decode_uri_component(rest).ok().and_then(|s| s.as_string()).unwrap_or_else(|| rest.to_string());
        (!decoded.is_empty()).then_some(decoded)
    }

    async fn pick_file(accept: &str) -> Result<Option<Result<(String, Vec<u8>), String>>, wasm_bindgen::JsValue> {
        let window = web_sys::window().ok_or("no window")?;
        let document = window.document().ok_or("no document")?;
        let body = document.body().ok_or("no body")?;
        let input: web_sys::HtmlInputElement = document.create_element("input")?.dyn_into()?;
        input.set_type("file");
        input.set_accept(accept);
        input.set_hidden(true);
        body.append_child(&input)?;
        let (closed, picker) = PickerClosed::new(&window, input.clone());
        input.click();
        let result = wasm_bindgen_futures::JsFuture::from(closed).await;
        let file = input.files().and_then(|files| files.get(0));
        drop(picker);
        result?;
        let Some(file) = file else { return Ok(None) };
        Ok(Some(read_file(&file).await.map(|bytes| (file.name(), bytes))))
    }

    pub(crate) async fn read_file(file: &web_sys::File) -> Result<Vec<u8>, String> {
        let name = file.name();
        figcraft_ui_egui::browser_can_read(&name, file.size())?;
        match wasm_bindgen_futures::JsFuture::from(file.array_buffer()).await {
            Ok(buffer) => Ok(js_sys::Uint8Array::new(&buffer).to_vec()),
            Err(e) => {
                log::error!("couldn't read {name}: {e:?}");
                Err(figcraft_ui_egui::unreadable(&name))
            }
        }
    }

    struct PickerClosed {
        window: web_sys::Window,
        input: web_sys::HtmlInputElement,
        _settle: wasm_bindgen::closure::Closure<dyn FnMut()>,
        on_focus: wasm_bindgen::closure::Closure<dyn FnMut()>,
        timer: Rc<Cell<Option<i32>>>,
    }

    impl PickerClosed {
        fn new(window: &web_sys::Window, input: web_sys::HtmlInputElement) -> (js_sys::Promise, Self) {
            let resolve = Rc::new(RefCell::new(None::<js_sys::Function>));
            let slot = resolve.clone();
            let closed = js_sys::Promise::new(&mut |res, _rej| *slot.borrow_mut() = Some(res));
            let settle = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
                if let Some(res) = resolve.borrow_mut().take() {
                    let _ = res.call0(&wasm_bindgen::JsValue::NULL);
                }
            });
            let _ = input.add_event_listener_with_callback("change", settle.as_ref().unchecked_ref());
            let _ = input.add_event_listener_with_callback("cancel", settle.as_ref().unchecked_ref());
            let timer = Rc::new(Cell::new(None));
            let (w, t, f) = (window.clone(), timer.clone(), settle.as_ref().unchecked_ref::<js_sys::Function>().clone());
            let on_focus = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
                if t.get().is_none() {
                    t.set(w.set_timeout_with_callback_and_timeout_and_arguments_0(&f, PICKER_FOCUS_SETTLE_MS).ok());
                }
            });
            let _ = window.add_event_listener_with_callback("focus", on_focus.as_ref().unchecked_ref());
            let _ = REVOKE_DOWNLOAD_MS;
            (closed, Self { window: window.clone(), input, _settle: settle, on_focus, timer })
        }
    }

    impl Drop for PickerClosed {
        fn drop(&mut self) {
            if let Some(t) = self.timer.take() {
                self.window.clear_timeout_with_handle(t);
            }
            let _ = self.window.remove_event_listener_with_callback("focus", self.on_focus.as_ref().unchecked_ref());
            self.input.remove();
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {
    web::run();
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("fogbreak-fig targets wasm32; build with `trunk build --release` inside fogbreak-overlay/");
}
