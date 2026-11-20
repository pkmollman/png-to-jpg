//! Pure image processing: flatten alpha, resize (bilinear), and JPEG-encode to a size budget.

use fast_image_resize::{FilterType, ResizeAlg, ResizeOptions, Resizer};
use image::{DynamicImage, ExtendedColorType, RgbImage, codecs::jpeg::JpegEncoder};

/// Composite the image over a solid background (JPEG has no alpha channel).
pub fn flatten(src: &DynamicImage, bg: [u8; 3]) -> RgbImage {
    let rgba = src.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut out = RgbImage::new(w, h);
    for (o, p) in out.pixels_mut().zip(rgba.pixels()) {
        let a = p[3] as u32;
        for c in 0..3 {
            o[c] = ((p[c] as u32 * a + bg[c] as u32 * (255 - a) + 127) / 255) as u8;
        }
    }
    out
}

/// Output resolution.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PrepParams {
    pub width: u32,
    pub height: u32,
}

/// Bilinear resize to the output resolution.
pub fn prepare(flat: &RgbImage, p: &PrepParams) -> RgbImage {
    if flat.dimensions() == (p.width, p.height) {
        return flat.clone();
    }
    let mut dst = RgbImage::new(p.width, p.height);
    Resizer::new()
        .resize(flat, &mut dst, &ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear)))
        .expect("RGB8 source and destination are always compatible");
    dst
}

pub fn encode(img: &RgbImage, quality: u8) -> Vec<u8> {
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, quality.clamp(1, 100))
        .encode(
            img.as_raw(),
            img.width(),
            img.height(),
            ExtendedColorType::Rgb8,
        )
        .expect("JPEG encoding of an in-memory RGB buffer cannot fail");
    out
}

pub struct Encoded {
    pub bytes: Vec<u8>,
    /// JPEG quality.
    pub level: u16,
    /// False when even the lowest level is over budget (that result is returned).
    pub fits: bool,
}

/// Every encode of one prepared image with fixed format settings, so changing
/// only the budget, cap or manual level reuses earlier work.
#[derive(Default)]
pub struct EncodeCache {
    results: Vec<(u16, Vec<u8>)>,
    /// Real encodes performed (cache misses), for display/benchmarking.
    pub passes: u32,
}

impl EncodeCache {
    fn size(&mut self, level: u16, enc: &dyn Fn(u16) -> Vec<u8>) -> usize {
        if let Some((_, b)) = self.results.iter().find(|(k, _)| *k == level) {
            return b.len();
        }
        let bytes = enc(level);
        self.passes += 1;
        let n = bytes.len();
        self.results.push((level, bytes));
        n
    }

    fn bytes(&self, level: u16) -> Vec<u8> {
        self.results.iter().find(|(k, _)| *k == level).expect("encoded").1.clone()
    }

    pub fn encode(&mut self, level: u16, enc: &dyn Fn(u16) -> Vec<u8>) -> Vec<u8> {
        self.size(level, enc);
        self.bytes(level)
    }
}

/// The range of a format's size knob and how fast ln(size) typically grows per step.
#[derive(Clone, Copy)]
pub struct Levels {
    pub min: u16,
    pub max: u16,
    pub default_slope: f64,
}

pub const JPEG_LEVELS: Levels = Levels { min: 1, max: 100, default_slope: 0.03 };

#[cfg(test)]
fn jpeg_to_fit(img: &RgbImage, max_bytes: usize, cap: u16, hint: Option<u16>, cache: &mut EncodeCache) -> Encoded {
    encode_to_fit(&|q| encode(img, q as u8), JPEG_LEVELS, max_bytes, cap, hint, cache)
}

/// Find the highest JPEG quality <= `cap` whose output
/// is <= `max_bytes`.
///
/// log(size) is close to linear in JPEG quality, so each guess solves a secant
/// through known encodes near the budget, then the adjacent level is checked to
/// close the bracket when the result lands close. Bisection takes over if
/// guesses keep landing on one side. `hint` (the previous answer) is tried
/// first, and earlier encodes in `cache` are reused, so small parameter or
/// budget changes usually cost 0-3 encodes.
pub fn encode_to_fit(
    enc: &dyn Fn(u16) -> Vec<u8>,
    levels: Levels,
    max_bytes: usize,
    cap: u16,
    hint: Option<u16>,
    cache: &mut EncodeCache,
) -> Encoded {
    let min = levels.min;
    let cap = cap.clamp(min, levels.max);
    let fits = |n: usize| n <= max_bytes;
    let target = (max_bytes.max(1) as f64).ln();
    let done = |cache: &EncodeCache, level: u16, fits: bool| Encoded { bytes: cache.bytes(level), level, fits };

    // Bracket: `lo` fits, `hi` does not; either may be unknown.
    let in_range = |q: u16| q >= min && q <= cap;
    let mut lo = cache
        .results
        .iter()
        .filter(|(q, b)| in_range(*q) && fits(b.len()))
        .max_by_key(|(q, _)| *q)
        .map(|(q, b)| (*q, b.len()));
    let mut hi = cache
        .results
        .iter()
        .filter(|(q, b)| in_range(*q) && !fits(b.len()) && lo.is_none_or(|(l, _)| *q > l))
        .min_by_key(|(q, _)| *q)
        .map(|(q, b)| (*q, b.len()));

    let inside = |q: u16, lo: Option<(u16, usize)>, hi: Option<(u16, usize)>| {
        in_range(q) && lo.is_none_or(|(l, _)| q > l) && hi.is_none_or(|(h, _)| q < h)
    };
    let mut queue: Option<u16> = hint.filter(|&h| inside(h, lo, hi));
    // Some(slope): check the adjacent level next if the result lands within
    // about one step of the budget (slope = d ln(size)/d level).
    let mut check_neighbour: Option<f64> = queue.map(|_| levels.default_slope);
    let mut same_side = 0;
    let mut last_fit = None;
    let secant = |cache: &EncodeCache| secant(&cache.results, target, levels.default_slope);

    loop {
        match (lo, hi) {
            (Some((l, _)), _) if l == cap => return done(cache, l, true),
            (Some((l, _)), Some((h, _))) if h - l <= 1 => return done(cache, l, true),
            (None, Some((h, _))) if h == min => return done(cache, min, false),
            _ => {}
        }

        let q = if let Some(q) = queue.take().filter(|&q| inside(q, lo, hi)) {
            q
        } else if let (None, Some((l, _))) = (hi, lo) {
            // Everything so far fits: extrapolate up toward the budget.
            let (q0, s0, slope) = secant(cache);
            ((q0 + (target - s0) / slope).round().clamp((l + 1) as f64, cap as f64)) as u16
        } else if hi.is_none() {
            cap
        } else {
            let (h, hs) = hi.unwrap();
            match lo {
                // Bracketed: interpolate on log-size (bisect if one side keeps winning).
                Some((l, ls)) => {
                    if same_side >= 2 {
                        l + (h - l) / 2
                    } else {
                        let (ls, hs) = ((ls as f64).ln(), (hs as f64).ln());
                        check_neighbour = Some((hs - ls) / (h - l) as f64);
                        let t = if hs > ls { (target - ls) / (hs - ls) } else { 0.5 };
                        ((l as f64 + t * (h - l) as f64).round().clamp((l + 1) as f64, (h - 1) as f64)) as u16
                    }
                }
                // Only too-big results so far: extrapolate down along the secant.
                None => {
                    let (q0, s0, slope) = secant(cache);
                    ((q0 + (target - s0) / slope).round().clamp(min as f64, (h - 1) as f64)) as u16
                }
            }
        };

        let n = cache.size(q, enc);
        let ok = fits(n);
        same_side = if last_fit == Some(ok) { same_side + 1 } else { 1 };
        last_fit = Some(ok);
        if ok {
            lo = Some((q, n));
        } else {
            hi = Some((q, n));
        }
        // After a guess, the adjacent level is the most likely other edge.
        if let Some(slope) = check_neighbour.take() {
            let steps_away = (target - (n as f64).ln()).abs() / slope.max(1e-6);
            if steps_away <= 1.5 {
                queue = if ok { Some(q + 1) } else { q.checked_sub(1) };
            }
        }
    }
}

/// The known encode nearest the budget in log-size, plus the log-size slope
/// to the next-nearest one (or `default_slope` if that is unusable).
fn secant(results: &[(u16, Vec<u8>)], target: f64, default_slope: f64) -> (f64, f64, f64) {
    let mut pts: Vec<(f64, f64)> = results.iter().map(|(q, b)| (*q as f64, (b.len() as f64).ln())).collect();
    pts.sort_by(|a, b| (a.1 - target).abs().total_cmp(&(b.1 - target).abs()));
    let (q0, s0) = pts[0];
    let slope = pts
        .get(1)
        // Adjacent qualities give a noisy slope; require some spread.
        .filter(|p| (p.0 - q0).abs() >= 3.0)
        .map(|&(q1, s1)| (s1 - s0) / (q1 - q0))
        .filter(|m| *m > 1e-3)
        .unwrap_or(default_slope);
    (q0, s0, slope)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noisy(w: u32, h: u32) -> RgbImage {
        let mut s = 12345u32;
        RgbImage::from_fn(w, h, |_, _| {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            image::Rgb([(s >> 24) as u8, (s >> 16) as u8, (s >> 8) as u8])
        })
    }

    /// Highest quality <= cap that fits, by exhaustive search.
    fn brute(sizes: &[usize], budget: usize, cap: u16) -> Option<u16> {
        (1..=cap).rev().find(|&q| sizes[q as usize] <= budget)
    }

    #[test]
    fn matches_exhaustive_search_for_many_budgets() {
        let img = noisy(160, 120);
        let sizes: Vec<usize> = (0..=100u8).map(|q| encode(&img, q.max(1)).len()).collect();
        // JPEG size is not perfectly monotonic in quality; only compare where it is.
        let monotonic = sizes[1..].windows(2).all(|w| w[0] <= w[1]);
        for cap in [100u16, 92, 75, 40] {
            for budget in (sizes[1] / 2..sizes[100] + 500).step_by(997) {
                for hint in [None, Some(1), Some(50), Some(83), Some(100)] {
                    let r = jpeg_to_fit(&img, budget, cap, hint, &mut EncodeCache::default());
                    assert_eq!(r.bytes.len(), sizes[r.level as usize]);
                    match brute(&sizes, budget, cap) {
                        None => assert!(!r.fits && r.level == 1),
                        Some(best) => {
                            assert!(r.fits && r.bytes.len() <= budget, "budget {budget} cap {cap}");
                            assert!(r.level == cap || sizes[r.level as usize + 1] > budget);
                            if monotonic {
                                assert_eq!(r.level, best, "budget {budget} cap {cap} hint {hint:?}");
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn correct_hint_needs_two_encodes() {
        let img = noisy(160, 120);
        let budget = encode(&img, 70).len();
        let first = jpeg_to_fit(&img, budget, 92, None, &mut EncodeCache::default());
        let mut cache = EncodeCache::default();
        let again = jpeg_to_fit(&img, budget, 92, Some(first.level), &mut cache);
        assert_eq!(again.level, first.level);
        assert_eq!(cache.passes, 2);
    }

    fn smooth(w: u32, h: u32) -> RgbImage {
        let mut s = 777u32;
        RgbImage::from_fn(w, h, |x, y| {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            let n = ((s >> 24) % 9) as u32;
            let ring = (((x as f32 - 90.0).hypot(y as f32 - 60.0) / 7.0).sin() * 60.0) as i32;
            let c = |v: i32| (v + n as i32).clamp(0, 255) as u8;
            image::Rgb([c(x as i32 + ring), c(y as i32 * 2), c(150 - ring)])
        })
    }

    /// Encodes plain bisection (the previous algorithm) would need.
    fn bisection_passes(sizes: &[usize], budget: usize, cap: u8) -> u32 {
        if sizes[cap as usize] <= budget { return 1; }
        if sizes[1] > budget { return 2; }
        let (mut lo, mut hi, mut n) = (1u8, cap, 2);
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            n += 1;
            if sizes[mid as usize] <= budget { lo = mid } else { hi = mid }
        }
        n
    }

    #[test]
    fn cold_search_beats_bisection() {
        for (name, img) in [("noise", noisy(160, 120)), ("smooth", smooth(400, 300))] {
            let sizes: Vec<usize> = (0..=100u8).map(|q| encode(&img, q.max(1)).len()).collect();
            let (mut ours, mut base, mut worst, mut count) = (0, 0, 0, 0);
            for q in (5..=90u8).step_by(5) {
                let budget = sizes[q as usize];
                let mut cache = EncodeCache::default();
                jpeg_to_fit(&img, budget, 92, None, &mut cache);
                ours += cache.passes;
                worst = worst.max(cache.passes);
                base += bisection_passes(&sizes, budget, 92);
                count += 1;
            }
            eprintln!("{name}: ours avg {:.1} worst {worst} | bisection avg {:.1}",
                ours as f64 / count as f64, base as f64 / count as f64);
            // Measured: noise 4.8 vs 8.7, smooth 5.9 vs 8.6 encodes on average.
            assert!(ours * 4 <= base * 3, "{name}: {ours} vs bisection {base}");
            assert!(worst <= 9, "{name}: worst {worst}");
        }
    }

    #[test]
    fn nearby_hint_is_cheap() {
        let img = smooth(400, 300);
        let budget = encode(&img, 73).len();
        for hint in [68u16, 70, 76, 78] {
            let mut cache = EncodeCache::default();
            let r = jpeg_to_fit(&img, budget, 92, Some(hint), &mut cache);
            assert!(r.fits);
            assert!(cache.passes <= 5, "hint {hint}: {} encodes", cache.passes);
        }
    }

    #[test]
    fn reuses_cache_when_only_budget_changes() {
        let img = noisy(160, 120);
        let mut cache = EncodeCache::default();
        let a = jpeg_to_fit(&img, encode(&img, 60).len(), 92, None, &mut cache);
        let before = cache.passes;
        let b = jpeg_to_fit(&img, encode(&img, 60).len(), 92, Some(a.level), &mut cache);
        assert_eq!(a.level, b.level);
        assert_eq!(cache.passes, before, "same budget should need no new encodes");
    }

    #[test]
    fn reports_impossible_budget() {
        let r = jpeg_to_fit(&noisy(200, 150), 100, 90, None, &mut EncodeCache::default());
        assert!(!r.fits);
        assert_eq!(r.level, 1);
    }

    #[test]
    fn resize_hits_requested_dimensions() {
        let p = PrepParams { width: 100, height: 57 };
        assert_eq!(prepare(&noisy(301, 207), &p).dimensions(), (100, 57));
    }

    #[test]
    fn flatten_blends_alpha() {
        let mut src = image::RgbaImage::new(1, 1);
        src.put_pixel(0, 0, image::Rgba([0, 0, 0, 0]));
        let flat = flatten(&DynamicImage::ImageRgba8(src), [255, 255, 255]);
        assert_eq!(flat.get_pixel(0, 0).0, [255, 255, 255]);
    }
}
