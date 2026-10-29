use std::cell::RefCell;
use std::rc::Rc;

use serde::Serialize;
use serde_json::{json, Value};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use crate::dap::session::DapSession;
use crate::dap::types::ProtocolMessage;
use crate::debug::dap::DwarfBackend;
use crate::debug::Debugger;
use crate::python::dap::adapter::PythonBackend;
use crate::python::debug::Debugger as PyDebugger;
use crate::types::DebugInfo;

struct AdapterState {
    session: DapSession,
    filter_internals: bool,
}

#[wasm_bindgen]
pub struct DapAdapter {
    state: Rc<RefCell<AdapterState>>,
    attachment: RefCell<Option<(web_sys::Worker, Closure<dyn FnMut(web_sys::MessageEvent)>)>>,
}

#[wasm_bindgen]
impl DapAdapter {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self {
            state: Rc::new(RefCell::new(AdapterState {
                session: DapSession::new(),
                filter_internals: false,
            })),
            attachment: RefCell::new(None),
        }
    }

    /// Attaches a worker to the adapter.
    ///
    /// Listens for the worker's `debug` message (containing `DebugInfo`) to construct
    /// the internal debugger, `paused` messages to emit DAP `stopped` events,
    /// and `stop` messages to emit `terminated`.
    pub fn attach(&self, worker: web_sys::Worker) {
        self.detach();
        self.reset_run();

        let state = self.state.clone();

        let closure = Closure::wrap(Box::new(move |event: web_sys::MessageEvent| {
            let data = event.data();
            let msg_type = js_sys::Reflect::get(&data, &"type".into())
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_default();

            match msg_type.as_str() {
                "debug" => {
                    let info = js_sys::Reflect::get(&data, &"info".into())
                        .expect("debug message has info field");
                    let info: DebugInfo =
                        serde_wasm_bindgen::from_value(info).expect("DebugInfo deserialization");
                    state.borrow_mut().session.set_debugger(Box::new(
                        DwarfBackend::new(Debugger::new(info)),
                    ));
                    state.borrow_mut().session.try_emit_initialized();
                }
                "python_debug" => {
                    let sab = js_sys::Reflect::get(&data, &"state".into())
                        .expect("python_debug message has state field");
                    let sab = sab
                        .dyn_into::<js_sys::SharedArrayBuffer>()
                        .expect("state is a SharedArrayBuffer");
                    let filter_internals = state.borrow().filter_internals;
                    state.borrow_mut().session.set_debugger(Box::new(
                        PythonBackend::new(PyDebugger::from_sab(sab), filter_internals),
                    ));
                    state.borrow_mut().session.try_emit_initialized();
                }
                "paused" => {
                    let body = {
                        let mut s = state.borrow_mut();
                        let debugger = s.session.debugger_mut().expect("paused without debugger");
                        debugger.handle_paused(&data).expect("handle pause")
                    };
                    state.borrow_mut().session.emit_event("stopped", Some(body));
                }
                "stop" => {
                    state.borrow_mut().session.emit_event(
                        "terminated",
                        Some(json!({ "restart": false })),
                    );
                }
                _ => {}
            }
        }) as Box<dyn FnMut(web_sys::MessageEvent)>);

        worker
            .add_event_listener_with_callback("message", closure.as_ref().unchecked_ref())
            .expect("Added message listener to worker");

        *self.attachment.borrow_mut() = Some((worker, closure));
    }

    /// Retire the listener without borrowing session state (callbacks can synchronously Stop).
    pub fn detach(&self) {
        if let Some((worker, closure)) = self.attachment.borrow_mut().take() {
            let _ = worker.remove_event_listener_with_callback("message", closure.as_ref().unchecked_ref());
        }
    }

    #[wasm_bindgen(js_name = "resetRun")]
    pub fn reset_run(&self) {
        self.state.borrow_mut().session.reset_run();
    }

    #[wasm_bindgen(js_name = "sendMessage")]
    pub fn send_message(&self, msg: JsValue) -> JsValue {
        let request: ProtocolMessage = match serde_wasm_bindgen::from_value(msg) {
            Ok(r) => r,
            Err(_) => return JsValue::NULL,
        };

        let ProtocolMessage::Request {
            seq,
            command,
            arguments,
        } = request
        else {
            return JsValue::NULL;
        };

        let args = arguments.unwrap_or(Value::Null);
        let response = self
            .state
            .borrow_mut()
            .session
            .process_request(seq, &command, &args);

        let ser = serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true);
        response.serialize(&ser).unwrap_or(JsValue::NULL)
    }

    #[wasm_bindgen(js_name = "setFilterInternals")]
    pub fn set_filter_internals(&self, value: bool) {
        self.state.borrow_mut().filter_internals = value;
    }

    pub fn on(&self, callback: js_sys::Function) {
        self.state.borrow_mut().session.set_callback(callback);
    }
}
