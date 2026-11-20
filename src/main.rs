use std::time::Duration;

use leptos::__reexports::wasm_bindgen_futures::JsFuture;
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos::wasm_bindgen::{JsCast, JsValue, closure::Closure};
use resize_img::protocol::{FromWorker, JobSpec, ToWorker};

/// The worker's JS glue and wasm, emitted by Trunk next to the page. Relative,
/// so they also resolve when the site is served from a subpath.
const WORKER_JS: &str = "./worker.js";
const WORKER_WASM: &str = "./worker_bg.wasm";
/// Resolution buttons: 100%, 50%, 25%, 12.5%, 6.25%.
const HALVING_STEPS: u32 = 5;
/// A halving step is offered only while the short side stays at least this big.
const MIN_HALVED_SIDE: u32 = 16;
/// Deepest wheel zoom, in screen pixels per original pixel.
const MAX_ZOOM: f64 = 16.0;

#[derive(Clone, PartialEq)]
struct SrcInfo {
    name: String,
    width: u32,
    height: u32,
    bytes: usize,
    has_alpha: bool,
}

#[derive(Clone, PartialEq)]
struct OutInfo {
    width: u32,
    height: u32,
    bytes: usize,
    quality: u16,
    fits: bool,
    auto: bool,
}

/// Everything that affects the output; a change sends a new job to the worker.
#[derive(Clone, Copy, PartialEq)]
struct Job {
    version: u32,
    width: u32,
    height: u32,
    auto: bool,
    target_bytes: u32,
    /// Exact quality in manual mode, the cap in auto mode.
    quality: u16,
}

fn main() {
    leptos::mount::mount_to_body(App)
}

fn fmt_bytes(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.2} MB", n as f64 / 1_000_000.0)
    } else {
        format!("{:.1} KB", n as f64 / 1000.0)
    }
}

/// Start the processing worker from a `blob:` URL holding a two-line loader.
///
/// Security: the page's CSP comes from a `<meta>` tag, and a worker loaded from
/// its own file would ignore it (it takes its policy from that file's HTTP
/// headers, which GitHub Pages can't set). A worker started from a `blob:` URL
/// inherits the page's policy instead, so `connect-src 'self'` etc. also bind
/// everything that runs inside the worker.
fn start_worker() -> Result<web_sys::Worker, JsValue> {
    let base = web_sys::window().ok_or("no window")?.location().href()?;
    let abs = |rel: &str| web_sys::Url::new_with_base(rel, &base).map(|u| u.href());
    // `{:?}` gives a double-quoted, escaped string literal, valid in JS too.
    let loader = format!("importScripts({:?});wasm_bindgen({:?});", abs(WORKER_JS)?, abs(WORKER_WASM)?);
    let opts = web_sys::BlobPropertyBag::new();
    opts.set_type("text/javascript");
    let blob = web_sys::Blob::new_with_str_sequence_and_options(&js_sys::Array::of1(&loader.into()), &opts)?;
    let url = web_sys::Url::create_object_url_with_blob(&blob)?;
    web_sys::Worker::new(&url)
}

/// Blob URL for bytes that already live in JS memory (no copy through wasm).
fn js_bytes_to_url(bytes: &js_sys::Uint8Array) -> Option<String> {
    let blob = web_sys::Blob::new_with_u8_array_sequence(&js_sys::Array::of1(bytes)).ok()?;
    web_sys::Url::create_object_url_with_blob(&blob).ok()
}

fn revoke(url: Option<String>) {
    if let Some(u) = url {
        let _ = web_sys::Url::revoke_object_url(&u);
    }
}

/// Take the first `File` out of a JS `FileList`. Must be called synchronously in
/// the change/drop handler: the list can be emptied right after (iOS Safari
/// empties it when the input is cleared; browsers empty a drop's list once the
/// drop event ends), but the `File` itself stays readable.
fn first_file(files: &JsValue) -> Option<JsValue> {
    js_sys::Reflect::get(files, &JsValue::from(0))
        .ok()
        .filter(|f| !f.is_undefined() && !f.is_null())
}

/// Read a `File`: (name, bytes, the File as a Blob).
async fn read_file(file: JsValue) -> Result<(String, js_sys::Uint8Array, web_sys::Blob), String> {
    let name = js_sys::Reflect::get(&file, &"name".into())
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_else(|| "image".into());
    let blob: web_sys::Blob = file.unchecked_into();
    let buf = JsFuture::from(blob.array_buffer())
        .await
        .map_err(|_| "Could not read file")?;
    Ok((name, js_sys::Uint8Array::new(&buf), blob))
}

#[component]
fn App() -> impl IntoView {
    let version = RwSignal::new(0u32);
    let src_info = RwSignal::new(None::<SrcInfo>);
    let src_url = RwSignal::new(None::<String>);
    let out_url = RwSignal::new(None::<String>);
    let out_info = RwSignal::new(None::<OutInfo>);
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);

    let width = RwSignal::new(1920u32);
    let height = RwSignal::new(1080u32);
    let auto = RwSignal::new(true);
    let target_kb = RwSignal::new(500.0f64);
    // Manual quality; in auto mode, the quality the search last chose.
    let quality = RwSignal::new(100u16);
    // Auto mode's ceiling. 100 = no ceiling: an image that fits comes out at full quality.
    let max_quality = RwSignal::new(100u16);

    let split = RwSignal::new(50.0f64);
    // Screen pixels per original pixel; 0 = fit the window.
    let zoom = RwSignal::new(0.0f64);
    let dragging = RwSignal::new(false);
    // While zoomed, dragging the image (away from the divider) pans:
    // (pointer x, pointer y, scroll left, scroll top) at the start of the drag.
    let pan_start = StoredValue::new(None::<(f64, f64, f64, f64)>);
    let cursor = RwSignal::new("ew-resize");
    let collapsed = RwSignal::new(false);
    // Visible part of the preview as fractions (left, top, width, height), for the minimap.
    let view_rect = RwSignal::new((0.0f64, 0.0f64, 1.0f64, 1.0f64));
    // Point of the image at the centre of the window, as a fraction of the
    // preview size. Captured right before zoom or output size change (while the
    // old layout is still on screen) and restored after, so the view stays put.
    let view_center = StoredValue::new(None::<(f64, f64)>);

    let aspect = move || {
        src_info
            .with(|s| s.as_ref().map(|s| s.width as f64 / s.height as f64))
            .unwrap_or(1.0)
    };

    // ---- Processing worker -------------------------------------------------------
    // Decoding, resizing and encoding run in a Web Worker, so the page stays
    // responsive. At most one job is in flight; while it runs, newer requests
    // replace each other in `queued`, so the worker never works through a backlog.
    let worker = StoredValue::new_local(None::<web_sys::Worker>);
    let in_flight = StoredValue::new(false);
    let queued = StoredValue::new(None::<JobSpec>);
    let last_job_id = StoredValue::new(0u32);
    let load_seq = StoredValue::new(0u32);
    // Name and size of the file being decoded, until the worker reports back.
    let pending_file = StoredValue::new(None::<(String, usize)>);

    let send_job = move |spec: JobSpec| {
        if in_flight.get_value() {
            queued.set_value(Some(spec));
            return;
        }
        let sent = worker.with_value(|w| w.as_ref().map(|w| ToWorker::Encode(spec).post(w).is_ok()));
        if sent == Some(true) {
            in_flight.set_value(true);
            busy.set(true);
        }
    };
    let job_finished = move || {
        in_flight.set_value(false);
        match queued.try_update_value(|q| q.take()).flatten() {
            Some(next) => send_job(next),
            None => busy.set(false),
        }
    };

    let on_reply = move |msg: FromWorker| match msg {
        FromWorker::Loaded { version: v, width: w, height: h, has_alpha } => {
            if v != load_seq.get_value() {
                return; // a newer file was opened meanwhile
            }
            let (name, bytes) = pending_file.try_update_value(|p| p.take()).flatten().unwrap_or_default();
            error.set(None);
            src_info.set(Some(SrcInfo { name, width: w, height: h, bytes, has_alpha }));
            view_center.set_value(Some((0.5, 0.5)));
            width.set(w);
            height.set(h);
            version.set(v);
            busy.set(in_flight.get_value());
        }
        FromWorker::Encoded { id, width: w, height: h, jpeg, quality: q, fits, .. } => {
            // Only show the newest request's result; older ones finished late.
            if id == last_job_id.get_value() {
                revoke(out_url.get_untracked());
                out_url.set(js_bytes_to_url(&jpeg));
                let was_auto = auto.get_untracked();
                if was_auto {
                    quality.set(q);
                }
                out_info.set(Some(OutInfo {
                    width: w,
                    height: h,
                    bytes: jpeg.length() as usize,
                    quality: q,
                    fits,
                    auto: was_auto,
                }));
            }
            job_finished();
        }
        FromWorker::Skipped { .. } => job_finished(),
        FromWorker::Failed { message } => {
            error.set(Some(message));
            job_finished();
        }
    };

    match start_worker() {
        Ok(w) => {
            let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |ev: web_sys::MessageEvent| {
                if let Some(msg) = FromWorker::parse(&ev.data()) {
                    on_reply(msg);
                }
            });
            w.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
            on_message.forget();
            // A crash inside the worker: report it and unblock the queue.
            let on_error = Closure::<dyn FnMut(web_sys::ErrorEvent)>::new(move |ev: web_sys::ErrorEvent| {
                error.set(Some(format!("Image processing failed: {}", ev.message())));
                job_finished();
            });
            w.set_onerror(Some(on_error.as_ref().unchecked_ref()));
            on_error.forget();
            worker.set_value(Some(w));
        }
        Err(_) => error.set(Some("Could not start the image-processing worker.".into())),
    }

    let load = move |files: JsValue| {
        // Grab the File now, before the list can be emptied (see `first_file`).
        // Nothing picked (e.g. the picker was closed): nothing to do.
        let Some(picked) = first_file(&files) else { return };
        spawn_local(async move {
            let (name, file, blob) = match read_file(picked).await {
                Ok(r) => r,
                Err(e) => return error.set(Some(e)),
            };
            let v = load_seq.get_value() + 1;
            load_seq.set_value(v);
            pending_file.set_value(Some((name, file.length() as usize)));
            revoke(src_url.get_untracked());
            src_url.set(web_sys::Url::create_object_url_with_blob(&blob).ok());
            // Drop the previous image's result so stale numbers/preview never show.
            revoke(out_url.get_untracked());
            out_url.set(None);
            out_info.set(None);
            busy.set(true);
            let posted = worker.with_value(|w| w.as_ref().map(|w| ToWorker::Load { version: v, file }.post(w).is_ok()));
            if posted != Some(true) {
                error.set(Some("Could not send the image to the worker.".into()));
                busy.set(false);
            }
        });
    };

    let job = Memo::new(move |_| {
        if version.get() == 0 {
            return None;
        }
        Some(Job {
            version: version.get(),
            width: width.get().clamp(1, 16384),
            height: height.get().clamp(1, 16384),
            auto: auto.get(),
            target_bytes: (target_kb.get() * 1000.0).clamp(1.0, u32::MAX as f64) as u32,
            // Auto: the cap. Manual: the exact quality (auto writes its answer into
            // `quality`, so it's only a dependency in manual mode).
            quality: if auto.get() { max_quality.get() } else { quality.get() },
        })
    });

    // Light debounce while sliders are dragged; the worker keeps the page responsive.
    let generation = StoredValue::new(0u64);
    Effect::new(move |_| {
        let Some(job) = job.get() else { return };
        generation.update_value(|g| *g += 1);
        let mine = generation.get_value();
        busy.set(true);
        set_timeout(
            move || {
                if generation.get_value() != mine {
                    return;
                }
                let id = last_job_id.get_value() + 1;
                last_job_id.set_value(id);
                send_job(JobSpec {
                    id,
                    version: job.version,
                    width: job.width,
                    height: job.height,
                    auto: job.auto,
                    target_bytes: job.target_bytes,
                    quality: job.quality,
                    // In auto mode the last answer is a good first guess.
                    hint: if job.auto { quality.get_untracked() } else { 0 },
                });
            },
            Duration::from_millis(60),
        );
    });

    let stage_el = || {
        web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.query_selector(".stage").ok().flatten())
    };
    let capture_center = move || {
        if let Some(s) = stage_el() {
            let (sw, sh) = (s.scroll_width().max(1) as f64, s.scroll_height().max(1) as f64);
            view_center.set_value(Some((
                (s.scroll_left() as f64 + s.client_width() as f64 / 2.0) / sw,
                (s.scroll_top() as f64 + s.client_height() as f64 / 2.0) / sh,
            )));
        }
    };
    let update_view_rect = move || {
        if let Some(s) = stage_el() {
            let (sw, sh) = (s.scroll_width().max(1) as f64, s.scroll_height().max(1) as f64);
            view_rect.set((
                s.scroll_left() as f64 / sw,
                s.scroll_top() as f64 / sh,
                (s.client_width() as f64 / sw).min(1.0),
                (s.client_height() as f64 / sh).min(1.0),
            ));
        }
    };
    // After the preview box changes size, scroll a captured centre point (if any)
    // back to the middle of the window. Wheel zoom handles its own scrolling.
    Effect::new(move |_| {
        (width.get(), height.get(), zoom.get());
        set_timeout(
            move || {
                if let Some(s) = stage_el() {
                    if let Some((fx, fy)) = view_center.get_value() {
                        view_center.set_value(None);
                        s.set_scroll_left((fx * s.scroll_width() as f64 - s.client_width() as f64 / 2.0).round() as i32);
                        s.set_scroll_top((fy * s.scroll_height() as f64 - s.client_height() as f64 / 2.0).round() as i32);
                    }
                }
                update_view_rect();
            },
            Duration::ZERO,
        );
    });

    // Zooming keeps an anchor point fixed on screen. The box is resized and
    // re-scrolled in the same step, before the browser paints, so no frame shows
    // the new size at the old scroll position (which looked like a ghost image jumping).
    let src_dims = move || src_info.with_untracked(|s| s.as_ref().map(|s| (s.width as f64, s.height as f64)));
    let compare_el = || {
        web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.query_selector(".compare").ok().flatten())
            .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
    };
    // Fit scale and window size: (fit, window width, window height, image w, image h).
    let geometry = move || {
        let (s, (sw, sh)) = (stage_el()?, src_dims()?);
        let (vw, vh) = (s.client_width() as f64, s.client_height() as f64);
        Some(((vw / sw).min(vh / sh), vw, vh, sw, sh))
    };
    // Zoom to `next` (screen px per original px), keeping the image point under the
    // window position (cx, cy) where it is.
    let zoom_to = move |next: f64, cx: f64, cy: f64| {
        let (Some(s), Some(cmp), Some((fit, vw, vh, sw, sh))) = (stage_el(), compare_el(), geometry()) else { return };
        let z = zoom.get_untracked();
        let cur = if z == 0.0 { fit } else { z };
        let next = next.clamp(fit, MAX_ZOOM.max(fit));
        let new_zoom = if next <= fit * 1.001 { 0.0 } else { next };
        let (cw, ch) = (sw * cur, sh * cur);
        let (nw, nh) = (sw * next, sh * next);
        // A box smaller than the window is centred in it.
        let offset = |view: f64, size: f64| ((view - size) / 2.0).max(0.0);
        let (sl, st) = (s.scroll_left() as f64, s.scroll_top() as f64);
        let (fx, fy) = ((cx + sl - offset(vw, cw)) / cw, (cy + st - offset(vh, ch)) / ch);

        // Same width rule as `compare_style`, applied now; setting the scroll right
        // after forces layout at the new size, all before the next paint.
        let width_css = if new_zoom == 0.0 {
            format!("min(100vw, calc(100vh * {sw} / {sh}))")
        } else {
            format!("{}px", sw * new_zoom)
        };
        let _ = cmp.style().set_property("width", &width_css);
        let clamp = |v: f64, size: f64, view: f64| v.clamp(0.0, (size - view).max(0.0));
        s.set_scroll_left(clamp(offset(vw, nw) + fx * nw - cx, nw, vw).round() as i32);
        s.set_scroll_top(clamp(offset(vh, nh) + fy * nh - cy, nh, vh).round() as i32);
        zoom.set(new_zoom);
        update_view_rect();
    };
    // Wheel: scrolling up zooms in. `anchor_cursor` false zooms around the window
    // centre (used over the minimap, where the cursor isn't over the image).
    let wheel_zoom = move |ev: &web_sys::WheelEvent, anchor_cursor: bool| {
        ev.prevent_default();
        let Some((fit, vw, vh, _, _)) = geometry() else { return };
        let z = zoom.get_untracked();
        let cur = if z == 0.0 { fit } else { z };
        let px_per_unit = match ev.delta_mode() {
            1 => 16.0, // lines
            2 => vh,   // pages
            _ => 1.0,
        };
        let next = cur * (-ev.delta_y() * px_per_unit * 0.0015).exp();
        let (cx, cy) = if anchor_cursor { (ev.client_x() as f64, ev.client_y() as f64) } else { (vw / 2.0, vh / 2.0) };
        zoom_to(next, cx, cy);
    };
    let on_wheel = move |ev: web_sys::WheelEvent| wheel_zoom(&ev, true);
    // Zoom slider: position 0..1 maps logarithmically from fit to MAX_ZOOM.
    let slider_pos = move || {
        let z = zoom.get();
        let Some((fit, ..)) = geometry() else { return 0.0 };
        if z == 0.0 || MAX_ZOOM <= fit { 0.0 } else { ((z / fit).ln() / (MAX_ZOOM / fit).ln()).clamp(0.0, 1.0) }
    };
    let on_slider = move |t: f64| {
        let Some((fit, vw, vh, _, _)) = geometry() else { return };
        zoom_to(fit * (MAX_ZOOM / fit).max(1.0).powf(t), vw / 2.0, vh / 2.0);
    };
    let resize_handle = window_event_listener(leptos::ev::resize, move |_| update_view_rect());
    on_cleanup(move || resize_handle.remove());

    // Minimap: click or drag to centre the view on that point of the image.
    let minimap_drag = RwSignal::new(false);
    let pan_to = move |ev: &web_sys::PointerEvent| {
        let Some(el) = ev.current_target() else { return };
        let el: web_sys::Element = el.unchecked_into();
        let (mw, mh) = (el.client_width().max(1) as f64, el.client_height().max(1) as f64);
        let (fx, fy) = ((ev.offset_x() as f64 / mw).clamp(0.0, 1.0), (ev.offset_y() as f64 / mh).clamp(0.0, 1.0));
        if let Some(s) = stage_el() {
            s.set_scroll_left((fx * s.scroll_width() as f64 - s.client_width() as f64 / 2.0).round() as i32);
            s.set_scroll_top((fy * s.scroll_height() as f64 - s.client_height() as f64 / 2.0).round() as i32);
        }
        update_view_rect();
    };
    let src_aspect = move || src_info.with(|s| s.as_ref().map_or(16.0 / 9.0, |s| s.width as f64 / s.height.max(1) as f64));

    // The aspect ratio is always kept: setting one side derives the other.
    let on_width = move |v: u32| {
        capture_center();
        width.set(v);
        height.set(((v as f64 / aspect()).round() as u32).max(1));
    };
    let on_height = move |v: u32| {
        capture_center();
        height.set(v);
        width.set(((v as f64 * aspect()).round() as u32).max(1));
    };

    // Is the pointer on (or within 16 px of) the divider?
    let near_divider = move |ev: &web_sys::PointerEvent| {
        let Some(el) = ev.current_target() else { return false };
        let w = el.unchecked_into::<web_sys::Element>().client_width() as f64;
        (ev.offset_x() as f64 - split.get_untracked() / 100.0 * w).abs() <= 16.0
    };
    let split_from = move |ev: &web_sys::PointerEvent| {
        let Some(el) = ev.current_target() else { return };
        let el: web_sys::Element = el.unchecked_into();
        let w = el.client_width() as f64;
        if w > 0.0 {
            split.set((ev.offset_x() as f64 / w * 100.0).clamp(0.0, 100.0));
        }
    };

    let download_name = move || {
        src_info.with(|s| {
            let name = s.as_ref().map(|s| s.name.as_str()).unwrap_or("image");
            let stem = name.rsplit_once('.').map(|(a, _)| a).unwrap_or(name);
            format!("{stem}.jpg")
        })
    };

    // Zoom is measured in *original* pixels, so changing the output resolution
    // never changes the preview box: the output is just drawn over the same area.
    let src_width = move || src_info.with(|s| s.as_ref().map_or(1, |s| s.width)).max(1) as f64;
    let compare_style = move || {
        // The box takes the original's shape, so rounding the output height
        // (e.g. 843.75 -> 844) can't nudge it by a pixel.
        let (w, h) = src_info
            .with(|s| s.as_ref().map(|s| (s.width as f64, s.height as f64)))
            .unwrap_or((width.get().max(1) as f64, height.get().max(1) as f64));
        let z = zoom.get();
        let size = if z == 0.0 {
            format!("width: min(100vw, calc(100vh * {w} / {h}));")
        } else {
            format!("width: {}px;", src_width() * z)
        };
        let render = if z >= 2.0 { "image-rendering: pixelated;" } else { "" };
        format!("aspect-ratio: {w} / {h}; {size} {render}")
    };
    // Show output pixels as crisp blocks once each covers 2+ screen pixels.
    let after_render = move || {
        let z = zoom.get();
        let magnification = z * src_width() / width.get().max(1) as f64;
        if z > 0.0 && magnification >= 2.0 { "pixelated" } else { "auto" }
    };

    let open_button = move || view! {
        <label class="button">
            "Open image…"
            <input
                type="file"
                accept="image/png,image/jpeg"
                hidden
                on:change=move |ev| {
                    let input: web_sys::HtmlInputElement = event_target(&ev);
                    if let Ok(files) = js_sys::Reflect::get(&input, &"files".into()) {
                        load(files);
                    }
                    input.set_value("");
                }
            />
        </label>
    };

    // One-line file summary for the bottom-centre bar. Placeholders keep its size
    // steady while the first result is computed.
    let summary_view = move || view! {
        <div class="panel summary">
            {move || src_info.get().map(|s| view! {
                <span class="item input" title=s.name.clone()>
                    <span class="k">"In"</span>
                    <span class="name">{s.name.clone()}</span>
                    {format!(" · {}×{} · {}", s.width, s.height, fmt_bytes(s.bytes))}
                </span>
            })}
            <span class="item">
                <span class="k">"Out"</span>
                {move || out_info.get().map_or_else(
                    || "—".to_string(),
                    |o| format!("{}×{} · {} · quality {}", o.width, o.height, fmt_bytes(o.bytes), o.quality),
                )}
            </span>
            <span class="busy" class:on=move || busy.get()>"Processing…"</span>
        </div>
    };

    // Does the output fit the size limit? Shown under the File size controls.
    let fit_status = move || out_info.get().map(|o| {
        let target = (target_kb.get() * 1000.0) as usize;
        let (cls, msg) = if o.fits {
            ("ok", format!("✓ {} — under {}", fmt_bytes(o.bytes), fmt_bytes(target)))
        } else if o.auto {
            ("bad", format!("✗ over {} even at quality 1: lower the resolution", fmt_bytes(target)))
        } else {
            ("bad", format!("✗ {} — over {}", fmt_bytes(o.bytes), fmt_bytes(target)))
        };
        view! { <div class=format!("status {cls}")>{msg}</div> }
    });

    let download_button = move || view! {
        <a class="button primary"
            href=move || out_url.get().unwrap_or_default()
            download=download_name
            class:disabled=move || out_url.with(|u| u.is_none())
        >"Download JPEG"</a>
    };

    view! {
        <div
            class="app"
            on:dragover=move |ev| ev.prevent_default()
            on:drop=move |ev| {
                ev.prevent_default();
                if let Ok(dt) = js_sys::Reflect::get(&ev, &"dataTransfer".into()) {
                    if let Ok(files) = js_sys::Reflect::get(&dt, &"files".into()) {
                        load(files);
                    }
                }
            }
        >
            <Show
                when=move || src_info.with(|s| s.is_some())
                fallback=move || view! {
                    <div class="empty">
                        <h1>"PNG → JPEG resizer"</h1>
                        <p>"Drop a PNG anywhere, or"</p>
                        {open_button()}
                        <p class="hint">"Everything runs locally in your browser."</p>
                    </div>
                }
            >
                // Full-page comparison; the panels float over it.
                // Wheel zooms. (Leptos attaches listeners to the element itself here,
                // so the handler can cancel the browser's own scrolling.)
                <div class="stage"
                    on:scroll=move |_| update_view_rect()
                    on:wheel=on_wheel
                >
                    <div class="compare"
                        style=compare_style
                        style:cursor=move || cursor.get()
                        on:pointerdown=move |ev| {
                            if let Some(t) = ev.current_target() {
                                let _ = t.unchecked_into::<web_sys::Element>().set_pointer_capture(ev.pointer_id());
                            }
                            if zoom.get_untracked() > 0.0 && !near_divider(&ev) {
                                // Zoomed in: drag the image to pan.
                                if let Some(s) = stage_el() {
                                    pan_start.set_value(Some((
                                        ev.client_x() as f64, ev.client_y() as f64,
                                        s.scroll_left() as f64, s.scroll_top() as f64,
                                    )));
                                }
                                cursor.set("grabbing");
                            } else {
                                dragging.set(true);
                                split_from(&ev);
                            }
                        }
                        on:pointermove=move |ev| {
                            if dragging.get_untracked() {
                                split_from(&ev);
                            } else if let Some((x0, y0, l0, t0)) = pan_start.get_value() {
                                if let Some(s) = stage_el() {
                                    s.set_scroll_left((l0 - (ev.client_x() as f64 - x0)).round() as i32);
                                    s.set_scroll_top((t0 - (ev.client_y() as f64 - y0)).round() as i32);
                                }
                                update_view_rect();
                            } else {
                                cursor.set(if zoom.get_untracked() > 0.0 && !near_divider(&ev) { "grab" } else { "ew-resize" });
                            }
                        }
                        on:pointerup=move |_| { dragging.set(false); pan_start.set_value(None); cursor.set("ew-resize"); }
                        on:pointercancel=move |_| { dragging.set(false); pan_start.set_value(None); }
                        // Double-click: back to fit.
                        on:dblclick=move |_| { view_center.set_value(None); zoom.set(0.0); }
                    >
                        <img class="before" src=move || src_url.get().unwrap_or_default() draggable="false"/>
                        <img class="after" draggable="false"
                            src=move || out_url.get().unwrap_or_default()
                            style:clip-path=move || format!("inset(0 0 0 {}%)", split.get())
                            style:image-rendering=after_render
                        />
                        <div class="divider" style:left=move || format!("{}%", split.get())>
                            <div class="knob">"⇆"</div>
                        </div>
                    </div>
                </div>
                // Top left: open.
                <div class="panel brand">
                    {open_button()}
                </div>
                // Bottom centre: what went in, what came out.
                {summary_view()}

                // Top right, one glass panel: minimap (always shown) with the zoom slider under it.
                <div class="panel navigator">
                    <div class="minimap"
                        on:wheel=move |ev| wheel_zoom(&ev, false)
                        style:aspect-ratio=move || src_aspect().to_string()
                        on:pointerdown=move |ev| {
                            minimap_drag.set(true);
                            if let Some(t) = ev.current_target() {
                                let _ = t.unchecked_into::<web_sys::Element>().set_pointer_capture(ev.pointer_id());
                            }
                            pan_to(&ev);
                        }
                        on:pointermove=move |ev| if minimap_drag.get_untracked() { pan_to(&ev) }
                        on:pointerup=move |_| minimap_drag.set(false)
                        on:pointercancel=move |_| minimap_drag.set(false)
                    >
                        <img src=move || src_url.get().unwrap_or_default() draggable="false"/>
                        <div class="viewbox"
                            style:left=move || format!("{}%", view_rect.get().0 * 100.0)
                            style:top=move || format!("{}%", view_rect.get().1 * 100.0)
                            style:width=move || format!("{}%", view_rect.get().2 * 100.0)
                            style:height=move || format!("{}%", view_rect.get().3 * 100.0)
                        ></div>
                    </div>
                    <div class="zoomslider" on:wheel=move |ev| wheel_zoom(&ev, false)>
                        <input type="range" min="0" max="1" step="0.001" title="Zoom"
                            prop:value=move || slider_pos().to_string()
                            on:input=move |ev| { if let Ok(t) = event_target_value(&ev).parse::<f64>() { on_slider(t) } }
                        />
                    </div>
                </div>

                // Controls: bottom left, stacked. The tab on the right edge slides it away.
                <aside class="panel controls" class:collapsed=move || collapsed.get()>
                    <button class="tab"
                        title=move || if collapsed.get() { "Show controls" } else { "Hide controls" }
                        on:click=move |_| collapsed.update(|c| *c = !*c)
                    >{move || if collapsed.get() { "›" } else { "‹" }}</button>
                    <div class="panel-body">
                        <section>
                            <h2>"Resolution"</h2>
                            <div class="row">
                                <label>"W"
                                    <input type="number" min="1" max="16384"
                                        prop:value=move || width.get().to_string()
                                        on:change=move |ev| if let Ok(v) = event_target_value(&ev).parse() { on_width(v) }
                                    />
                                </label>
                                <span class="x">"×"</span>
                                <label>"H"
                                    <input type="number" min="1" max="16384"
                                        prop:value=move || height.get().to_string()
                                        on:change=move |ev| if let Ok(v) = event_target_value(&ev).parse() { on_height(v) }
                                    />
                                </label>
                            </div>
                            // Halving steps: 100%, 50%, 25%, ... Each is enabled only while
                            // the result keeps at least MIN_HALVED_SIDE px on the short side.
                            <div class="row presets">
                                {(0..HALVING_STEPS).map(|n| {
                                    let div = 1u32 << n;
                                    let target = move || src_info.with(|s| s.as_ref().map(|s| {
                                        let w = ((s.width as f64 / div as f64).round() as u32).max(1);
                                        let h = ((s.height as f64 / div as f64).round() as u32).max(1);
                                        (w, h, s.width.min(s.height) / div >= MIN_HALVED_SIDE)
                                    }));
                                    let label = match n {
                                        0 => "100%".to_string(),
                                        _ => format!("{}%", 100.0 / div as f64),
                                    };
                                    view! {
                                        <button class="step"
                                            prop:disabled=move || !target().is_some_and(|t| t.2)
                                            class:active=move || target().is_some_and(|t| t.0 == width.get())
                                            title=move || target().map(|(w, h, ok)| if ok {
                                                format!("{w}×{h}")
                                            } else {
                                                format!("Too small to halve again ({w}×{h})")
                                            }).unwrap_or_default()
                                            on:click=move |_| if let Some((w, _, true)) = target() { on_width(w) }
                                        >
                                            <span>{label}</span>
                                            <small>{move || target().map(|t| t.0.to_string()).unwrap_or_default()}</small>
                                        </button>
                                    }
                                }).collect_view()}
                            </div>
                        </section>

                        <section>
                            <h2>"File size"</h2>
                            <label class="field">"Max size (KB, 1 KB = 1000 bytes)"
                                <input type="number" min="1" step="1"
                                    prop:value=move || target_kb.get().to_string()
                                    on:change=move |ev| { if let Ok(v) = event_target_value(&ev).parse::<f64>() { if v > 0.0 { target_kb.set(v) } } }
                                />
                            </label>
                            <label class="check">
                                <input type="checkbox"
                                    prop:checked=move || auto.get()
                                    on:change=move |ev| auto.set(event_target_checked(&ev))
                                />
                                "Auto quality (highest that fits)"
                            </label>
                            // One slider: in auto mode it caps the search, otherwise it's
                            // the exact quality. The chosen quality shows in the top bar.
                            <Slider label="Quality" min=1.0 max=100.0 step=1.0
                                value=Signal::derive(move || if auto.get() { max_quality.get() } else { quality.get() } as f64)
                                on_input=move |v| if auto.get_untracked() { max_quality.set(v as u16) } else { quality.set(v as u16) }
                                hint=""
                            />
                            {fit_status}
                        </section>

                    </div>
                </aside>

                <div class="download">{download_button()}</div>

            </Show>

            {move || error.get().map(|e| view! {
                <div class="error" on:click=move |_| error.set(None) title="Dismiss">{e}</div>
            })}
        </div>
    }
}

#[component]
fn Slider(
    label: &'static str,
    min: f64,
    max: f64,
    step: f64,
    #[prop(into)] value: Signal<f64>,
    on_input: impl Fn(f64) + 'static,
    #[prop(optional, into)] disabled: Signal<bool>,
    hint: &'static str,
) -> impl IntoView {
    let decimals = if step < 1.0 { 1 } else { 0 };
    view! {
        <label class="slider" class:disabled=move || disabled.get()>
            <div class="slider-head">
                <span>{label}</span>
                <span class="val">{move || format!("{:.*}", decimals, value.get())}</span>
            </div>
            <input type="range" min=min max=max step=step
                prop:value=move || value.get().to_string()
                prop:disabled=move || disabled.get()
                on:input=move |ev| if let Ok(v) = event_target_value(&ev).parse() { on_input(v) }
            />
            {(!hint.is_empty()).then(|| view! { <small>{hint}</small> })}
        </label>
    }
}
