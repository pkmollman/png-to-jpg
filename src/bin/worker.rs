//! Processing worker: decodes, resizes and JPEG-encodes on its own thread so the
//! page never freezes. Messages are handled one at a time; the page coalesces
//! requests so this never builds a backlog.

use std::{cell::RefCell, rc::Rc};

use image::RgbImage;
use js_sys::Date;
use leptos::wasm_bindgen::{JsCast, closure::Closure};
use resize_img::process::{EncodeCache, JPEG_LEVELS, PrepParams, encode, encode_to_fit, flatten, prepare};
use resize_img::protocol::{FromWorker, JobSpec, ToWorker};
use web_sys::{DedicatedWorkerGlobalScope, MessageEvent};

/// Transparent areas are flattened onto white (JPEG has no alpha).
const MATTE: [u8; 3] = [255, 255, 255];

/// The resized image for one (image, resolution), plus every encode of it.
struct Prepared {
    key: (u32, PrepParams),
    rgb: RgbImage,
    cache: EncodeCache,
}

#[derive(Default)]
struct State {
    /// (version, image flattened onto MATTE)
    source: Option<(u32, RgbImage)>,
    prepared: Option<Prepared>,
}

fn main() {
    let scope: DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();
    let state = Rc::new(RefCell::new(State::default()));
    let reply_to = scope.clone();
    let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |ev: MessageEvent| {
        let Some(msg) = ToWorker::parse(&ev.data()) else { return };
        let reply = handle(&mut state.borrow_mut(), msg);
        let _ = reply.post(&reply_to);
    });
    scope.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget(); // lives as long as the worker
}

fn handle(st: &mut State, msg: ToWorker) -> FromWorker {
    match msg {
        ToWorker::Load { version, file } => match image::load_from_memory(&file.to_vec()) {
            Ok(img) => {
                let (width, height, has_alpha) = (img.width(), img.height(), img.color().has_alpha());
                st.source = Some((version, flatten(&img, MATTE)));
                st.prepared = None;
                FromWorker::Loaded { version, width, height, has_alpha }
            }
            Err(e) => FromWorker::Failed { message: format!("Could not decode the image: {e}") },
        },
        ToWorker::Encode(job) => encode_job(st, job),
    }
}

fn encode_job(st: &mut State, job: JobSpec) -> FromWorker {
    let Some((version, flat)) = &st.source else { return FromWorker::Skipped { id: job.id } };
    if *version != job.version {
        return FromWorker::Skipped { id: job.id };
    }
    let prep = PrepParams { width: job.width.clamp(1, 16384), height: job.height.clamp(1, 16384) };
    let key = (job.version, prep);

    let mut prep_ms = None;
    if st.prepared.as_ref().map(|p| p.key) != Some(key) {
        let t = Date::now();
        let rgb = prepare(flat, &prep);
        prep_ms = Some(Date::now() - t);
        st.prepared = Some(Prepared { key, rgb, cache: EncodeCache::default() });
    }
    let p = st.prepared.as_mut().expect("just prepared");

    let t = Date::now();
    let rgb = &p.rgb;
    let enc = |q: u16| encode(rgb, q as u8);
    p.cache.passes = 0;
    let (bytes, quality, fits) = if job.auto {
        let hint = (job.hint > 0).then_some(job.hint);
        let r = encode_to_fit(&enc, JPEG_LEVELS, job.target_bytes as usize, job.quality, hint, &mut p.cache);
        (r.bytes, r.level, r.fits)
    } else {
        let q = job.quality.clamp(JPEG_LEVELS.min, JPEG_LEVELS.max);
        let b = p.cache.encode(q, &enc);
        let fits = b.len() <= job.target_bytes as usize;
        (b, q, fits)
    };

    FromWorker::Encoded {
        id: job.id,
        width: prep.width,
        height: prep.height,
        jpeg: js_sys::Uint8Array::from(bytes.as_slice()),
        quality,
        fits,
        prep_ms,
        encode_ms: Date::now() - t,
        passes: p.cache.passes,
    }
}
