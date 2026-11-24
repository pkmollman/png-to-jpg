//! Messages between the page and the processing worker.
//!
//! Plain JS objects, so image bytes can be *transferred* between threads (moved,
//! not copied) via `postMessage`'s transfer list.

use crate::process::{Filter, Sharpen};
use js_sys::{Array, Object, Reflect, Uint8Array};
use leptos::wasm_bindgen::{JsCast, JsValue};
use web_sys::{DedicatedWorkerGlobalScope, Worker};

fn set(o: &Object, key: &str, value: impl Into<JsValue>) {
    let _ = Reflect::set(o, &key.into(), &value.into());
}
fn get(o: &JsValue, key: &str) -> JsValue {
    Reflect::get(o, &key.into()).unwrap_or(JsValue::UNDEFINED)
}
fn num(o: &JsValue, key: &str) -> f64 {
    get(o, key).as_f64().unwrap_or(0.0)
}
fn flag(o: &JsValue, key: &str) -> bool {
    get(o, key).as_bool().unwrap_or(false)
}
fn bytes(o: &JsValue, key: &str) -> Option<Uint8Array> {
    get(o, key).dyn_into::<Uint8Array>().ok()
}
fn transfer_list(b: &Uint8Array) -> Array {
    Array::of1(&b.buffer())
}

/// One encode request. `id` lets the page ignore results it no longer wants.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct JobSpec {
    pub id: u32,
    /// Which loaded image this is for.
    pub version: u32,
    pub width: u32,
    pub height: u32,
    pub filter: Filter,
    pub sharpen: Sharpen,
    pub auto: bool,
    pub target_bytes: u32,
    /// Exact quality in manual mode, the cap in auto mode.
    pub quality: u16,
    /// Auto mode's previous answer, a good first guess (0 = none).
    pub hint: u16,
}

pub enum ToWorker {
    /// Decode a new source image (PNG/JPEG file bytes).
    Load { version: u32, file: Uint8Array },
    Encode(JobSpec),
}

pub enum FromWorker {
    Loaded { version: u32, width: u32, height: u32, has_alpha: bool },
    Encoded {
        id: u32,
        width: u32,
        height: u32,
        jpeg: Uint8Array,
        quality: u16,
        fits: bool,
        /// None when the resized image came from the worker's cache.
        prep_ms: Option<f64>,
        encode_ms: f64,
        passes: u32,
    },
    /// The job was for an image that has since been replaced.
    Skipped { id: u32 },
    Failed { message: String },
}

impl ToWorker {
    pub fn post(self, worker: &Worker) -> Result<(), JsValue> {
        let o = Object::new();
        match self {
            ToWorker::Load { version, file } => {
                set(&o, "kind", "load");
                set(&o, "version", version);
                set(&o, "file", &file);
                worker.post_message_with_transfer(&o, &transfer_list(&file))
            }
            ToWorker::Encode(j) => {
                set(&o, "kind", "encode");
                set(&o, "id", j.id);
                set(&o, "version", j.version);
                set(&o, "width", j.width);
                set(&o, "height", j.height);
                set(&o, "filter", j.filter.key());
                set(&o, "sharpenAmount", j.sharpen.amount);
                set(&o, "sharpenRadius", j.sharpen.radius);
                set(&o, "sharpenThreshold", j.sharpen.threshold);
                set(&o, "auto", j.auto);
                set(&o, "target", j.target_bytes);
                set(&o, "quality", j.quality);
                set(&o, "hint", j.hint);
                worker.post_message(&o)
            }
        }
    }

    pub fn parse(v: &JsValue) -> Option<ToWorker> {
        match get(v, "kind").as_string()?.as_str() {
            "load" => Some(ToWorker::Load { version: num(v, "version") as u32, file: bytes(v, "file")? }),
            "encode" => Some(ToWorker::Encode(JobSpec {
                id: num(v, "id") as u32,
                version: num(v, "version") as u32,
                width: num(v, "width") as u32,
                height: num(v, "height") as u32,
                filter: Filter::from_key(&get(v, "filter").as_string().unwrap_or_default()),
                sharpen: Sharpen {
                    amount: num(v, "sharpenAmount") as f32,
                    radius: num(v, "sharpenRadius") as f32,
                    threshold: num(v, "sharpenThreshold") as u8,
                },
                auto: flag(v, "auto"),
                target_bytes: num(v, "target") as u32,
                quality: num(v, "quality") as u16,
                hint: num(v, "hint") as u16,
            })),
            _ => None,
        }
    }
}

impl FromWorker {
    pub fn post(self, scope: &DedicatedWorkerGlobalScope) -> Result<(), JsValue> {
        let o = Object::new();
        match self {
            FromWorker::Loaded { version, width, height, has_alpha } => {
                set(&o, "kind", "loaded");
                set(&o, "version", version);
                set(&o, "width", width);
                set(&o, "height", height);
                set(&o, "hasAlpha", has_alpha);
                scope.post_message(&o)
            }
            FromWorker::Encoded { id, width, height, jpeg, quality, fits, prep_ms, encode_ms, passes } => {
                set(&o, "kind", "encoded");
                set(&o, "id", id);
                set(&o, "width", width);
                set(&o, "height", height);
                set(&o, "jpeg", &jpeg);
                set(&o, "quality", quality);
                set(&o, "fits", fits);
                set(&o, "prepMs", prep_ms.map_or(JsValue::NULL, JsValue::from));
                set(&o, "encodeMs", encode_ms);
                set(&o, "passes", passes);
                scope.post_message_with_transfer(&o, &transfer_list(&jpeg))
            }
            FromWorker::Skipped { id } => {
                set(&o, "kind", "skipped");
                set(&o, "id", id);
                scope.post_message(&o)
            }
            FromWorker::Failed { message } => {
                set(&o, "kind", "failed");
                set(&o, "message", message);
                scope.post_message(&o)
            }
        }
    }

    pub fn parse(v: &JsValue) -> Option<FromWorker> {
        match get(v, "kind").as_string()?.as_str() {
            "loaded" => Some(FromWorker::Loaded {
                version: num(v, "version") as u32,
                width: num(v, "width") as u32,
                height: num(v, "height") as u32,
                has_alpha: flag(v, "hasAlpha"),
            }),
            "encoded" => Some(FromWorker::Encoded {
                id: num(v, "id") as u32,
                width: num(v, "width") as u32,
                height: num(v, "height") as u32,
                jpeg: bytes(v, "jpeg")?,
                quality: num(v, "quality") as u16,
                fits: flag(v, "fits"),
                prep_ms: get(v, "prepMs").as_f64(),
                encode_ms: num(v, "encodeMs"),
                passes: num(v, "passes") as u32,
            }),
            "skipped" => Some(FromWorker::Skipped { id: num(v, "id") as u32 }),
            "failed" => Some(FromWorker::Failed { message: get(v, "message").as_string().unwrap_or_default() }),
            _ => None,
        }
    }
}
