//! Direct `<canvas data-host="page">` host bridge and `window.fogbreakFigHost` RPC handle.
#![cfg(target_arch = "wasm32")]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use figcraft_doc::ExportFormat;
use figcraft_engine::cmd::file::{export_bytes, file_bytes, fig_bytes, frames_pdf};
use figcraft_engine::{Session, all_commands, find_command};
use figcraft_ui_egui::{ControlReply, ControlRequest, FigApp, RequestGate, pending_open};
use serde_json::{Value, json};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::JsFuture;
use web_sys::{CustomEvent, CustomEventInit, HtmlCanvasElement};

use crate::theme;

pub const ENGINE_ID: &str = "fogbreak-fig";
pub const DEFAULT_HOST_KEY: &str = "fogbreakFigHost";
pub const MAX_OPEN_BYTES: u64 = 512 << 20;
const POLL_MS: i32 = 16;

#[derive(Clone)]
pub struct ExportResult {
    pub name: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

type ExportSlot = Arc<Mutex<Option<Result<ExportResult, String>>>>;

pub enum HostAction {
    SetTheme(bool),
    OpenBytes {
        name: String,
        bytes: Vec<u8>,
    },
    Export {
        format: String,
        slot: ExportSlot,
        deliver_save: bool,
    },
}

#[derive(Clone, Default)]
pub struct ActionQueue(Rc<RefCell<VecDeque<HostAction>>>);

impl ActionQueue {
    pub fn push(&self, action: HostAction) {
        self.0.borrow_mut().push_back(action);
    }

    pub fn drain(&self) -> Vec<HostAction> {
        self.0.borrow_mut().drain(..).collect()
    }
}

#[derive(Clone)]
pub struct HostContext {
    pub canvas: HtmlCanvasElement,
    pub ctx: egui::Context,
    pub control_pending: Rc<RefCell<Vec<ControlRequest>>>,
    pub actions: ActionQueue,
    pub dark_state: Arc<Mutex<bool>>,
    pub next_id: Rc<Cell<u64>>,
}

pub fn find_page_canvas() -> Option<HtmlCanvasElement> {
    let doc = web_sys::window()?.document()?;
    let sel = format!("canvas[data-host=\"page\"][data-engine=\"{ENGINE_ID}\"]");
    if let Ok(Some(el)) = doc.query_selector(&sel)
        && let Ok(c) = el.dyn_into::<HtmlCanvasElement>()
    {
        return Some(c);
    }
    doc.get_element_by_id("fogbreak_canvas")
        .and_then(|e| e.dyn_into::<HtmlCanvasElement>().ok())
}

fn event_prefix(canvas: &HtmlCanvasElement) -> String {
    canvas
        .get_attribute("data-engine")
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| ENGINE_ID.to_string())
}

fn rpc_key(canvas: &HtmlCanvasElement) -> String {
    canvas
        .get_attribute("data-rpc-key")
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_HOST_KEY.to_string())
}

fn js_from_json(value: &Value) -> JsValue {
    js_sys::JSON::parse(&value.to_string()).unwrap_or(JsValue::NULL)
}

fn json_from_js(value: &JsValue) -> Value {
    if value.is_undefined() || value.is_null() {
        return Value::Null;
    }
    js_sys::JSON::stringify(value)
        .ok()
        .and_then(|s| s.as_string())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null)
}

fn dispatch_event(canvas: &HtmlCanvasElement, suffix: &str, detail: &JsValue) {
    let name = format!("{}:{suffix}", event_prefix(canvas));
    let init = CustomEventInit::new();
    init.set_detail(detail);
    if let Ok(ev) = CustomEvent::new_with_event_init_dict(&name, &init) {
        let _ = canvas.dispatch_event(&ev);
    }
}

pub fn emit_ready(canvas: &HtmlCanvasElement) {
    let detail = js_from_json(&json!({
        "engine": ENGINE_ID,
        "version": env!("CARGO_PKG_VERSION"),
        "capabilities": {
            "execute": true,
            "commands": true,
            "listCommands": true,
            "describeCommand": true,
            "docInspect": true,
            "uiInspect": true,
            "screenshot": true,
            "open": true,
            "theme": true,
            "io": true,
        }
    }));
    dispatch_event(canvas, "ready", &detail);
}

pub fn emit_failed(canvas: &HtmlCanvasElement, error: &str) {
    dispatch_event(canvas, "failed", &js_from_json(&json!({ "error": error })));
}

pub fn emit_dirty(canvas: &HtmlCanvasElement, dirty: bool, last: &mut Option<bool>) {
    if *last == Some(dirty) {
        return;
    }
    *last = Some(dirty);
    dispatch_event(canvas, "dirty", &js_from_json(&json!({ "dirty": dirty })));
}

pub fn deliver_save(canvas: &HtmlCanvasElement, name: &str, save_as: bool, bytes: &[u8]) {
    let detail = js_sys::Object::new();
    let arr = js_sys::Uint8Array::from(bytes);
    let _ = js_sys::Reflect::set(&detail, &"name".into(), &name.into());
    let _ = js_sys::Reflect::set(&detail, &"saveAs".into(), &save_as.into());
    let _ = js_sys::Reflect::set(&detail, &"data".into(), &arr);
    let _ = js_sys::Reflect::set(&detail, &"bytes".into(), &arr);
    dispatch_event(canvas, "save", &detail.into());
}

pub fn emit_handoff(canvas: &HtmlCanvasElement, target: &str, payload: Value) {
    dispatch_event(canvas, "handoff", &js_from_json(&json!({ "target": target, "payload": payload })));
}

pub fn emit_theme(canvas: &HtmlCanvasElement, dark: bool) {
    let mode = if dark { "dark" } else { "light" };
    dispatch_event(canvas, "theme", &js_from_json(&json!({ "theme": mode, "dark": dark })));
}

fn spec_to_json(spec: &figcraft_engine::CommandSpec) -> Value {
    json!({
        "id": spec.id,
        "label": spec.label,
        "params": spec.params,
        "menu": spec.menu(),
        "shortcut": spec.shortcut(),
        "undoable": spec.history,
    })
}

pub fn all_command_descriptors() -> Vec<Value> {
    all_commands().iter().map(spec_to_json).collect()
}

pub fn describe_command(id: &str) -> Option<Value> {
    find_command(id).map(spec_to_json)
}

fn parse_bytes_arg(val: &JsValue) -> Option<Vec<u8>> {
    if let Some(u8a) = val.dyn_ref::<js_sys::Uint8Array>() {
        return Some(u8a.to_vec());
    }
    if let Some(ab) = val.dyn_ref::<js_sys::ArrayBuffer>() {
        return Some(js_sys::Uint8Array::new(ab).to_vec());
    }
    if js_sys::Array::is_array(val) {
        let arr = js_sys::Array::from(val);
        let mut out = Vec::with_capacity(arr.length() as usize);
        for v in arr.iter() {
            let n = v.as_f64()?;
            out.push(n.clamp(0.0, 255.0) as u8);
        }
        return Some(out);
    }
    None
}

fn sanitize_name(name: &str) -> String {
    let leaf = name.trim().rsplit(['/', '\\']).next().unwrap_or("").trim();
    if leaf.is_empty() {
        "design.figcraft".to_string()
    } else {
        leaf.to_string()
    }
}

fn open_from_js_detail(detail: &JsValue, actions: &ActionQueue, ctx: &egui::Context) -> Result<String, String> {
    let raw_name = js_sys::Reflect::get(detail, &"name".into())
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_default();
    let name = sanitize_name(&raw_name);
    let data = js_sys::Reflect::get(detail, &"bytes".into())
        .ok()
        .filter(|v| !v.is_undefined() && !v.is_null())
        .or_else(|| js_sys::Reflect::get(detail, &"data".into()).ok())
        .ok_or_else(|| "missing `bytes` or `data` payload".to_string())?;
    let bytes = parse_bytes_arg(&data).ok_or_else(|| "expected Uint8Array, ArrayBuffer, or byte array".to_string())?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_OPEN_BYTES {
        return Err(format!("file exceeds {} MiB host open limit", MAX_OPEN_BYTES >> 20));
    }
    actions.push(HostAction::OpenBytes {
        name: name.clone(),
        bytes,
    });
    ctx.request_repaint();
    Ok(name)
}

pub fn listen_open_and_theme(host: &HostContext) {
    let prefix = event_prefix(&host.canvas);
    let open_event = format!("{prefix}:open");
    let theme_event = format!("{prefix}:set-theme");

    let actions_open = host.actions.clone();
    let ctx_open = host.ctx.clone();
    let open_cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |ev: web_sys::Event| {
        if let Ok(cev) = ev.dyn_into::<CustomEvent>()
            && let Err(err) = open_from_js_detail(&cev.detail(), &actions_open, &ctx_open)
        {
            log::warn!("host open refused: {err}");
        }
    });
    let _ = host.canvas.add_event_listener_with_callback(&open_event, open_cb.as_ref().unchecked_ref());
    if let Some(win) = web_sys::window() {
        let _ = win.add_event_listener_with_callback(&open_event, open_cb.as_ref().unchecked_ref());
    }
    open_cb.forget();

    let actions = host.actions.clone();
    let ctx_theme = host.ctx.clone();
    let theme_cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |ev: web_sys::Event| {
        if let Ok(cev) = ev.dyn_into::<CustomEvent>() {
            let v = json_from_js(&cev.detail());
            if let Some(dark) = parse_dark_param(&v) {
                actions.push(HostAction::SetTheme(dark));
                ctx_theme.request_repaint();
            }
        }
    });
    let _ = host.canvas.add_event_listener_with_callback(&theme_event, theme_cb.as_ref().unchecked_ref());
    theme_cb.forget();
}

fn parse_dark_param(v: &Value) -> Option<bool> {
    if let Some(b) = v.get("dark").and_then(Value::as_bool) {
        return Some(b);
    }
    match v.get("theme").and_then(Value::as_str).map(str::to_ascii_lowercase).as_deref() {
        Some("dark") => Some(true),
        Some("light") => Some(false),
        _ => None,
    }
}

async fn sleep(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        if let Some(win) = web_sys::window() {
            let _ = win.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        }
    });
    let _ = JsFuture::from(promise).await;
}

struct WaitingReply {
    rx: Receiver<ControlReply>,
    gate: RequestGate,
    done: bool,
}

impl Drop for WaitingReply {
    fn drop(&mut self) {
        if !self.done {
            self.gate.cancel_if_open();
        }
    }
}

async fn send_control(host: &HostContext, method: &str, params: Value) -> Value {
    let (reply, rx) = std::sync::mpsc::channel();
    let id = host.next_id.get().wrapping_add(1);
    host.next_id.set(id);
    let gate = RequestGate::new();
    host.control_pending.borrow_mut().push(ControlRequest {
        id: json!(id),
        method: method.to_string(),
        params,
        reply,
        state: gate.clone(),
    });
    host.ctx.request_repaint();
    let mut waiting = WaitingReply { rx, gate, done: false };
    for _ in 0..600 {
        match waiting.rx.try_recv() {
            Ok(ControlReply { result, .. }) => {
                waiting.done = true;
                return match result {
                    Ok(v) => json!({ "ok": true, "result": v }),
                    Err(e) => json!({ "ok": false, "error": e }),
                };
            }
            Err(TryRecvError::Empty) => sleep(POLL_MS).await,
            Err(TryRecvError::Disconnected) => {
                waiting.done = true;
                return json!({ "ok": false, "error": "app control channel disconnected" });
            }
        }
    }
    json!({ "ok": false, "error": "control request timed out" })
}

async fn run_request(host: HostContext, method: String, params: Value) -> Value {
    match method.as_str() {
        "theme.get" => {
            let dark = *host.dark_state.lock().unwrap_or_else(|e| e.into_inner());
            json!({ "ok": true, "result": { "theme": if dark { "dark" } else { "light" }, "dark": dark } })
        }
        "theme.set" => {
            let Some(dark) = parse_dark_param(&params) else {
                return json!({ "ok": false, "error": "expected { theme: 'light'|'dark' } or { dark: bool }" });
            };
            host.actions.push(HostAction::SetTheme(dark));
            host.ctx.request_repaint();
            json!({ "ok": true, "result": { "theme": if dark { "dark" } else { "light" }, "dark": dark } })
        }
        "ui.commands" | "engine.commands" | "command.list" => {
            json!({ "ok": true, "result": { "commands": all_command_descriptors() } })
        }
        "command.describe" => {
            let id = params.get("id").or_else(|| params.get("command")).and_then(Value::as_str).unwrap_or("");
            match describe_command(id) {
                Some(spec) => json!({ "ok": true, "result": spec }),
                None => json!({ "ok": false, "error": format!("unknown command {id:?}") }),
            }
        }
        "ui.state" => send_control(&host, "document.inspect", params).await,
        _ => send_control(&host, &method, params).await,
    }
}

fn install_async_fn(
    target: &js_sys::Object,
    name: &str,
    handler: impl Fn(JsValue, JsValue) -> js_sys::Promise + 'static,
) {
    let cb = Closure::<dyn FnMut(JsValue, JsValue) -> js_sys::Promise>::new(handler);
    let _ = js_sys::Reflect::set(target, &name.into(), cb.as_ref().unchecked_ref());
    cb.forget();
}

fn install_sync_fn(
    target: &js_sys::Object,
    name: &str,
    handler: impl Fn(JsValue, JsValue) -> JsValue + 'static,
) {
    let cb = Closure::<dyn FnMut(JsValue, JsValue) -> JsValue>::new(handler);
    let _ = js_sys::Reflect::set(target, &name.into(), cb.as_ref().unchecked_ref());
    cb.forget();
}

fn install_core_methods(obj: &js_sys::Object, host: &HostContext) {
    let h = host.clone();
    install_async_fn(obj, "request", move |m, p| {
        let h = h.clone();
        let method = m.as_string().unwrap_or_default();
        let params = json_from_js(&p);
        wasm_bindgen_futures::future_to_promise(async move { Ok(js_from_json(&run_request(h, method, params).await)) })
    });

    for alias in ["execute", "executeCommand"] {
        let h = host.clone();
        install_async_fn(obj, alias, move |c, p| {
            let h = h.clone();
            let cmd = c.as_string().unwrap_or_default();
            let params = json_from_js(&p);
            wasm_bindgen_futures::future_to_promise(async move {
                let res = send_control(&h, "engine.execute", json!({ "command": cmd, "params": params })).await;
                Ok(js_from_json(&res))
            })
        });
    }

    install_sync_fn(obj, "commands", |_, _| js_from_json(&Value::Array(all_command_descriptors())));

    install_sync_fn(obj, "listCommands", |q, _| {
        let filter = q.as_string().map(|s| s.to_ascii_lowercase());
        let list: Vec<Value> = all_command_descriptors()
            .into_iter()
            .filter(|v| {
                let Some(f) = &filter else { return true };
                let id = v.get("id").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
                let label = v.get("label").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
                id.contains(f) || label.contains(f)
            })
            .collect();
        js_from_json(&Value::Array(list))
    });

    install_sync_fn(obj, "describeCommand", |id, _| {
        let key = id.as_string().unwrap_or_default();
        describe_command(&key).map_or(JsValue::NULL, |v| js_from_json(&v))
    });
}

fn install_inspect_methods(obj: &js_sys::Object, host: &HostContext) {
    for alias in ["docInspect", "inspect"] {
        let h = host.clone();
        install_async_fn(obj, alias, move |p, _| {
            let h = h.clone();
            let params = json_from_js(&p);
            wasm_bindgen_futures::future_to_promise(async move {
                Ok(js_from_json(&send_control(&h, "document.inspect", params).await))
            })
        });
    }

    let h = host.clone();
    install_async_fn(obj, "uiInspect", move |p, _| {
        let h = h.clone();
        let params = json_from_js(&p);
        wasm_bindgen_futures::future_to_promise(async move {
            Ok(js_from_json(&send_control(&h, "ui.inspect", params).await))
        })
    });

    let h = host.clone();
    install_async_fn(obj, "screenshot", move |p, _| {
        let h = h.clone();
        let params = json_from_js(&p);
        wasm_bindgen_futures::future_to_promise(async move {
            Ok(js_from_json(&send_control(&h, "ui.screenshot", params).await))
        })
    });

    let h = host.clone();
    install_sync_fn(obj, "open", move |name_val, bytes_val| {
        let name = sanitize_name(&name_val.as_string().unwrap_or_default());
        let Some(bytes) = parse_bytes_arg(&bytes_val) else {
            return js_from_json(&json!({ "ok": false, "error": "expected byte buffer" }));
        };
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_OPEN_BYTES {
            return js_from_json(&json!({ "ok": false, "error": "file exceeds size limit" }));
        }
        h.actions.push(HostAction::OpenBytes {
            name: name.clone(),
            bytes,
        });
        h.ctx.request_repaint();
        js_from_json(&json!({ "ok": true, "name": name }))
    });

    let c = host.canvas.clone();
    install_sync_fn(obj, "emitHandoff", move |target, payload| {
        let t = target.as_string().unwrap_or_default();
        emit_handoff(&c, &t, json_from_js(&payload));
        JsValue::TRUE
    });

    let h = host.clone();
    install_sync_fn(obj, "setTheme", move |mode, _| {
        let dark = mode
            .as_bool()
            .or_else(|| mode.as_string().map(|s| s.eq_ignore_ascii_case("dark")))
            .unwrap_or(false);
        h.actions.push(HostAction::SetTheme(dark));
        h.ctx.request_repaint();
        js_from_json(&json!({ "theme": if dark { "dark" } else { "light" }, "dark": dark }))
    });
}

fn build_io_object(host: &HostContext) -> js_sys::Object {
    let io_obj = js_sys::Object::new();
    install_sync_fn(&io_obj, "formats", |_, _| {
        js_from_json(&json!({
            "open": ["figcraft", "fig", "json", "penpot", "sketch", "svg", "png", "jpg", "jpeg", "webp", "gif", "bmp", "tiff"],
            "export": ["figcraft", "fig", "json", "svg", "pdf", "png", "jpg", "webp"]
        }))
    });

    let h = host.clone();
    install_async_fn(&io_obj, "import", move |name_val, bytes_val| {
        let h = h.clone();
        let name = sanitize_name(&name_val.as_string().unwrap_or_default());
        let parsed = parse_bytes_arg(&bytes_val);
        wasm_bindgen_futures::future_to_promise(async move {
            let Some(bytes) = parsed else {
                return Ok(js_from_json(&json!({ "ok": false, "error": "expected Uint8Array or ArrayBuffer" })));
            };
            if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_OPEN_BYTES {
                return Ok(js_from_json(&json!({ "ok": false, "error": "file exceeds size limit" })));
            }
            let data = base64::engine::general_purpose::STANDARD.encode(bytes);
            let res = send_control(&h, "engine.execute", json!({ "command": "file.open", "params": { "name": name, "data": data } })).await;
            Ok(js_from_json(&res))
        })
    });

    let h = host.clone();
    install_async_fn(&io_obj, "export", move |opts_val, _| {
        let h = h.clone();
        let opts = json_from_js(&opts_val);
        let format = opts.get("format").and_then(Value::as_str).unwrap_or("figcraft").to_ascii_lowercase();
        let deliver_save_flag = opts.get("deliverSave").and_then(Value::as_bool).unwrap_or(false);
        let slot: ExportSlot = Arc::new(Mutex::new(None));
        h.actions.push(HostAction::Export {
            format,
            slot: slot.clone(),
            deliver_save: deliver_save_flag,
        });
        h.ctx.request_repaint();
        wasm_bindgen_futures::future_to_promise(async move {
            for _ in 0..600 {
                if let Some(res) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    return match res {
                        Ok(exp) => {
                            let out = js_sys::Object::new();
                            let arr = js_sys::Uint8Array::from(exp.bytes.as_slice());
                            let _ = js_sys::Reflect::set(&out, &"ok".into(), &true.into());
                            let _ = js_sys::Reflect::set(&out, &"name".into(), &exp.name.into());
                            let _ = js_sys::Reflect::set(&out, &"mime".into(), &exp.mime.into());
                            let _ = js_sys::Reflect::set(&out, &"bytes".into(), &arr);
                            let _ = js_sys::Reflect::set(&out, &"data".into(), &arr);
                            Ok(out.into())
                        }
                        Err(err) => Ok(js_from_json(&json!({ "ok": false, "error": err }))),
                    };
                }
                sleep(POLL_MS).await;
            }
            Ok(js_from_json(&json!({ "ok": false, "error": "export timed out" })))
        })
    });
    io_obj
}

pub fn install_host_rpc(host: &HostContext) {
    let Some(win) = web_sys::window() else { return };
    let key = rpc_key(&host.canvas);
    let obj = js_sys::Object::new();
    install_core_methods(&obj, host);
    install_inspect_methods(&obj, host);
    let io_obj = build_io_object(host);
    let _ = js_sys::Reflect::set(&obj, &"io".into(), &io_obj);
    let _ = js_sys::Reflect::set(&obj, &"engine".into(), &ENGINE_ID.into());
    let _ = js_sys::Reflect::set(&win, &key.into(), &obj);
}

fn export_targets(session: &Session) -> Result<Vec<figcraft_doc::NodeId>, String> {
    let ds = session.ds().map_err(|e| e.to_string())?;
    if !ds.selection.is_empty() {
        return Ok(ds.selection.clone());
    }
    let children = ds
        .doc
        .node(ds.page)
        .map(|n| n.children.clone())
        .unwrap_or_default();
    if children.is_empty() {
        return Err("no layers on active page to export".to_string());
    }
    Ok(children)
}

fn export_active_document(session: &Session, format: &str) -> Result<ExportResult, String> {
    let ds = session.ds().map_err(|e| e.to_string())?;
    let base = sanitize_name(&ds.doc.name);
    let stem = base
        .strip_suffix(".figcraft")
        .or_else(|| base.strip_suffix(".fig"))
        .unwrap_or(&base);
    match format {
        "json" => {
            let v = figcraft_format::to_json(&ds.doc).map_err(|e| e.to_string())?;
            let bytes = serde_json::to_vec_pretty(&v).map_err(|e| e.to_string())?;
            Ok(ExportResult { name: format!("{stem}.json"), mime: "application/json".into(), bytes })
        }
        "fig" => {
            let (bytes, _) = fig_bytes(&ds.doc).map_err(|e| e.to_string())?;
            Ok(ExportResult { name: format!("{stem}.fig"), mime: "application/octet-stream".into(), bytes })
        }
        "svg" => {
            let ids = export_targets(session)?;
            let bytes = export_bytes(&ds.doc, &ids, ExportFormat::Svg, 1.0).map_err(|e| e.to_string())?;
            Ok(ExportResult { name: format!("{stem}.svg"), mime: "image/svg+xml".into(), bytes })
        }
        "pdf" => {
            if let Ok((bytes, _)) = frames_pdf(&ds.doc, ds.page, &Default::default()) {
                return Ok(ExportResult { name: format!("{stem}.pdf"), mime: "application/pdf".into(), bytes });
            }
            let ids = export_targets(session)?;
            let bytes = export_bytes(&ds.doc, &ids, ExportFormat::Pdf, 1.0).map_err(|e| e.to_string())?;
            Ok(ExportResult { name: format!("{stem}.pdf"), mime: "application/pdf".into(), bytes })
        }
        "png" => {
            let ids = export_targets(session)?;
            let bytes = export_bytes(&ds.doc, &ids, ExportFormat::Png, 2.0).map_err(|e| e.to_string())?;
            Ok(ExportResult { name: format!("{stem}.png"), mime: "image/png".into(), bytes })
        }
        "jpg" | "jpeg" => {
            let ids = export_targets(session)?;
            let bytes = export_bytes(&ds.doc, &ids, ExportFormat::Jpg, 2.0).map_err(|e| e.to_string())?;
            Ok(ExportResult { name: format!("{stem}.jpg"), mime: "image/jpeg".into(), bytes })
        }
        "webp" => {
            let ids = export_targets(session)?;
            let bytes = export_bytes(&ds.doc, &ids, ExportFormat::Webp, 2.0).map_err(|e| e.to_string())?;
            Ok(ExportResult { name: format!("{stem}.webp"), mime: "image/webp".into(), bytes })
        }
        _ => {
            let bytes = file_bytes(ds).map_err(|e| e.to_string())?;
            Ok(ExportResult { name: format!("{stem}.figcraft"), mime: "application/zip".into(), bytes })
        }
    }
}

pub fn process_actions(
    fig: &mut FigApp,
    ctx: &egui::Context,
    canvas: &HtmlCanvasElement,
    actions: &ActionQueue,
    dark_state: &Arc<Mutex<bool>>,
) {
    for action in actions.drain() {
        match action {
            HostAction::SetTheme(dark) => {
                *dark_state.lock().unwrap_or_else(|e| e.into_inner()) = dark;
                fig.session.prefs.theme = if dark { "dark" } else { "light" }.to_string();
                theme::sync_document_theme(dark);
                theme::apply_palette(ctx, dark);
                emit_theme(canvas, dark);
            }
            HostAction::OpenBytes { name, bytes } => {
                let read: figcraft_ui_egui::ReadFuture = Box::pin(std::future::ready(Ok(bytes)));
                fig.ui.pending_opens.push(pending_open::PendingOpen {
                    label: name.clone(),
                    params: json!({ "name": name }),
                    command: "file.open",
                    target: None,
                    read: Some(read),
                });
            }
            HostAction::Export { format, slot, deliver_save: do_save } => {
                let res = export_active_document(&fig.session, &format);
                if do_save
                    && let Ok(exp) = &res
                {
                    deliver_save(canvas, &exp.name, false, &exp.bytes);
                }
                *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(res);
            }
        }
    }
}
