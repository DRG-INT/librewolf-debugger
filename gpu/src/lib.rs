//! Perceptual comparison of browser renders, on the GPU where that pays and
//! on the CPU always.
//!
//! The claim "this tool works with every browser" is only worth anything if it
//! can be checked, and the check is: render the same URL in every engine
//! available and compare the pixels. That is an embarrassingly parallel image
//! problem over many pairs, which is where a shader earns its place.
//!
//! Scores are **evidence for a human, not a verdict**. Engines differ
//! legitimately in font rasterisation, widget metrics and scrollbar width. A
//! cross-engine SSIM of 0.85 may be perfect agreement about layout; the number
//! locates a page worth looking at, it does not decide anything.

pub mod cpu;
/// Generated cosine table, shared with the JavaScript implementation.
pub mod dct_table;
pub mod image;

#[cfg(feature = "gpu")]
pub mod gpu;

/// One WGSL source translated to every target language, and compile-verified
/// where a compiler for it exists. Behind the `gpu` feature because it pulls
/// naga in, and the binary that measures page loads has no use for it.
#[cfg(feature = "gpu")]
pub mod translate;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Whether the wgpu backend was compiled into this build.
///
/// Reported, not inferred. `Backend::parse("gpu")` succeeds either way
/// because it only parses a name, and `Backend::Auto` silently falls back —
/// so a caller that needs to know whether the GPU path exists at all, rather
/// than whether it happened to be used, has to ask this.
///
/// A wrong answer here is not cosmetic: the eval suite used to assume the
/// fallback was universal and reported a failure on a build with the feature
/// off, which is a gate failing for a reason that is not a defect.
pub const fn gpu_compiled_in() -> bool {
    cfg!(feature = "gpu")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// Use the GPU when one is available, otherwise the CPU. Reports which.
    Auto,
    Gpu,
    Cpu,
}

impl Backend {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "auto" => Backend::Auto,
            "gpu" => Backend::Gpu,
            "cpu" => Backend::Cpu,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Parity {
    pub ssim: f64,
    pub phash_a: u64,
    pub phash_b: u64,
    pub phash_hamming: u32,
    /// Which path actually produced the SSIM: "gpu" or "cpu". Never "auto".
    pub backend: String,
    pub adapter: Option<String>,
    /// Dimensions actually compared, after cropping to the overlap.
    pub compared_width: usize,
    pub compared_height: usize,
    /// Set when the two renders were not the same size, so the score is over
    /// a crop rather than the whole of either.
    pub size_mismatch: Option<String>,
    /// Plain-language reading of the numbers. Not a verdict.
    pub reading: String,
}

/// Compare two PNG screenshots.
pub fn compare_png(a_png: &[u8], b_png: &[u8], backend: Backend) -> Result<Parity> {
    let a = image::decode_png(a_png)?;
    let b = image::decode_png(b_png)?;
    compare(&a, &b, backend)
}

pub fn compare(a: &image::Luma, b: &image::Luma, backend: Backend) -> Result<Parity> {
    let size_mismatch = (a.width != b.width || a.height != b.height).then(|| {
        format!(
            "{}x{} vs {}x{}; scored over the {}x{} overlap",
            a.width,
            a.height,
            b.width,
            b.height,
            a.width.min(b.width),
            a.height.min(b.height)
        )
    });

    let (used, ssim, adapter) = run_ssim(a, b, backend)?;

    let ha = cpu::phash(a)?;
    let hb = cpu::phash(b)?;
    let ham = cpu::hamming(ha, hb);

    Ok(Parity {
        ssim,
        phash_a: ha,
        phash_b: hb,
        phash_hamming: ham,
        backend: used,
        adapter,
        compared_width: a.width.min(b.width),
        compared_height: a.height.min(b.height),
        size_mismatch,
        reading: reading(ssim, ham),
    })
}

#[cfg(feature = "gpu")]
fn run_ssim(
    a: &image::Luma,
    b: &image::Luma,
    backend: Backend,
) -> Result<(String, f64, Option<String>)> {
    match backend {
        Backend::Cpu => Ok(("cpu".into(), cpu::ssim(a, b)?, None)),
        Backend::Gpu => {
            let g = gpu::Gpu::new()?;
            let adapter = Some(format!("{} ({})", g.adapter_name, g.backend));
            Ok(("gpu".into(), g.ssim(a, b)?, adapter))
        }
        Backend::Auto => match gpu::Gpu::new() {
            Ok(g) => {
                let adapter = Some(format!("{} ({})", g.adapter_name, g.backend));
                match g.ssim(a, b) {
                    Ok(v) => Ok(("gpu".into(), v, adapter)),
                    // A shader that fails at run time must not take the
                    // measurement with it.
                    Err(_) => Ok(("cpu".into(), cpu::ssim(a, b)?, None)),
                }
            }
            Err(_) => Ok(("cpu".into(), cpu::ssim(a, b)?, None)),
        },
    }
}

#[cfg(not(feature = "gpu"))]
fn run_ssim(
    a: &image::Luma,
    b: &image::Luma,
    backend: Backend,
) -> Result<(String, f64, Option<String>)> {
    if backend == Backend::Gpu {
        anyhow::bail!("this build has no GPU support (feature \"gpu\" is off)");
    }
    Ok(("cpu".into(), cpu::ssim(a, b)?, None))
}

/// Turn the two numbers into a sentence, with the cross-engine caveat
/// attached. The thresholds are reading aids, not thresholds anything
/// branches on.
fn reading(ssim: f64, hamming: u32) -> String {
    let structural = match hamming {
        0 => "the same page structurally",
        1..=5 => "structurally near-identical",
        6..=15 => "structurally similar",
        _ => "structurally different",
    };
    let pixels = if ssim > 0.999 {
        "pixel-identical"
    } else if ssim > 0.98 {
        "all but identical"
    } else if ssim > 0.90 {
        "close, with visible local differences"
    } else if ssim > 0.70 {
        "clearly different in places"
    } else {
        "substantially different"
    };
    format!(
        "{pixels} (SSIM {ssim:.4}), {structural} (pHash distance {hamming}). \
         Across engines, font rasterisation and widget metrics differ by \
         design — look at the images before calling this a defect."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::Luma;

    fn checker(w: usize, h: usize, size: usize) -> Luma {
        let data = (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                if (x / size + y / size).is_multiple_of(2) {
                    30
                } else {
                    220
                }
            })
            .collect();
        Luma::new(w, h, data).unwrap()
    }

    #[test]
    fn backend_names_round_trip_and_reject_nonsense() {
        assert_eq!(Backend::parse("gpu"), Some(Backend::Gpu));
        assert_eq!(Backend::parse("CPU"), Some(Backend::Cpu));
        assert_eq!(Backend::parse("auto"), Some(Backend::Auto));
        assert_eq!(Backend::parse("metal"), None);
    }

    #[test]
    fn the_reported_backend_is_never_auto() {
        let a = checker(64, 64, 8);
        let p = compare(&a, &a, Backend::Auto).unwrap();
        assert!(
            p.backend == "gpu" || p.backend == "cpu",
            "backend was {:?}, which tells the reader nothing",
            p.backend
        );
    }

    #[test]
    fn an_image_against_itself_is_identical_on_whichever_backend_ran() {
        let a = checker(128, 128, 8);
        let p = compare(&a, &a, Backend::Auto).unwrap();
        assert!(p.ssim > 0.999, "SSIM {} on backend {}", p.ssim, p.backend);
        assert_eq!(p.phash_hamming, 0);
        assert!(p.reading.contains("pixel-identical"));
        assert!(p.size_mismatch.is_none());
    }

    #[test]
    fn a_size_difference_is_reported_rather_than_hidden() {
        let a = checker(128, 128, 8);
        let b = checker(128, 96, 8);
        let p = compare(&a, &b, Backend::Cpu).unwrap();
        let m = p.size_mismatch.expect("mismatch must be reported");
        assert!(m.contains("128x128 vs 128x96"), "got: {m}");
        assert_eq!((p.compared_width, p.compared_height), (128, 96));
    }

    /// The whole point of keeping the CPU path: it is the definition the
    /// shader is checked against. If this ever fails, the shader is wrong.
    #[test]
    fn gpu_and_cpu_agree_within_float_tolerance() {
        let a = checker(128, 128, 8);
        let mut d = a.data.clone();
        // Perturb a region so the score is not trivially 1.0.
        for y in 40..70 {
            for x in 30..90 {
                d[y * 128 + x] = d[y * 128 + x].wrapping_add(37);
            }
        }
        let b = Luma::new(128, 128, d).unwrap();

        let c = compare(&a, &b, Backend::Cpu).unwrap();
        let g = match compare(&a, &b, Backend::Gpu) {
            Ok(g) => g,
            // No adapter here (CI, or a vantage host without one). The CPU
            // answer stands, and that is the documented behaviour.
            Err(e) => {
                eprintln!("skipping: no GPU available ({e})");
                return;
            }
        };
        assert_eq!(g.backend, "gpu");
        assert!(
            (g.ssim - c.ssim).abs() < 1e-4,
            "gpu {} vs cpu {} — the shader disagrees with the reference",
            g.ssim,
            c.ssim
        );
    }

    #[test]
    fn the_reading_always_carries_the_cross_engine_caveat() {
        for (s, h) in [(1.0, 0u32), (0.5, 30), (0.95, 3)] {
            let r = reading(s, h);
            assert!(r.contains("font rasterisation"), "missing caveat in: {r}");
        }
    }
}
