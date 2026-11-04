//! CPU reference implementation.
//!
//! This is not a fallback in the sense of "worse but available". It is the
//! definition: the GPU path is tested against it, and any disagreement beyond
//! floating-point tolerance is a bug in the shader. The CI machine and the
//! Void and Windows vantage hosts may have no usable adapter at all, so this
//! path must stay correct and complete.

use crate::image::Luma;
use anyhow::Result;

/// Window side for SSIM. 8 is the classic block size and divides the common
/// screenshot widths without a remainder worth worrying about.
pub const WINDOW: usize = 8;

/// SSIM stabilisers from Wang et al. 2004, for 8-bit dynamic range:
/// C1 = (K1*L)^2 with K1 = 0.01, C2 = (K2*L)^2 with K2 = 0.03, L = 255.
pub const C1: f64 = 6.5025; // (0.01 * 255)^2
pub const C2: f64 = 58.5225; // (0.03 * 255)^2

/// Mean SSIM over non-overlapping windows.
///
/// Non-overlapping rather than a sliding Gaussian window: the sliding form is
/// ~64x the work for a difference that does not change any decision here, and
/// the GPU path maps one workgroup to one window because of it.
pub fn ssim(a: &Luma, b: &Luma) -> Result<f64> {
    let (a, b) = Luma::common_crop(a, b)?;
    let wx = a.width / WINDOW;
    let wy = a.height / WINDOW;
    if wx == 0 || wy == 0 {
        // Smaller than one window: compare the whole thing as a single block.
        return Ok(window_ssim(&a, &b, 0, 0, a.width, a.height));
    }
    let mut total = 0.0;
    for by in 0..wy {
        for bx in 0..wx {
            total += window_ssim(&a, &b, bx * WINDOW, by * WINDOW, WINDOW, WINDOW);
        }
    }
    Ok(total / (wx * wy) as f64)
}

fn window_ssim(a: &Luma, b: &Luma, x0: usize, y0: usize, w: usize, h: usize) -> f64 {
    let n = (w * h) as f64;
    let mut sa = 0.0;
    let mut sb = 0.0;
    let mut saa = 0.0;
    let mut sbb = 0.0;
    let mut sab = 0.0;
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            let va = a.at(x, y) as f64;
            let vb = b.at(x, y) as f64;
            sa += va;
            sb += vb;
            saa += va * va;
            sbb += vb * vb;
            sab += va * vb;
        }
    }
    let ma = sa / n;
    let mb = sb / n;
    // Population variance, matching the reference implementation's use of the
    // biased estimator.
    let va = saa / n - ma * ma;
    let vb = sbb / n - mb * mb;
    let cab = sab / n - ma * mb;

    let num = (2.0 * ma * mb + C1) * (2.0 * cab + C2);
    let den = (ma * ma + mb * mb + C1) * (va + vb + C2);
    if den == 0.0 {
        1.0
    } else {
        num / den
    }
}

/// 64-bit perceptual hash: 32x32 luma, 2D DCT-II, the 8x8 low-frequency
/// corner excluding DC, thresholded at its median.
///
/// DC is excluded because it is overall brightness; including it makes the
/// hash sensitive to a theme change that moved nothing.
pub fn phash(img: &Luma) -> Result<u64> {
    use crate::dct_table::{COS, K, N};
    let small = img.resize(N, N)?;
    let px: Vec<f64> = small.data.iter().map(|&v| v as f64).collect();

    // The cosines come from a shared table rather than from `cos`, because
    // V8 bundles its own fdlibm for `Math.cos` instead of using the platform
    // libm. The last-bit difference that causes is amplified by the median
    // threshold below into flipped hash bits — measured at 3 of 64 for a
    // 32-pixel checker — so the JavaScript implementation in gpu/js and this
    // one could not otherwise agree. See gpu/src/dct_table.rs.
    //
    // Separable DCT-II: rows then columns, in that order. Floating-point
    // addition is not associative, so the order is part of the contract with
    // the JavaScript side, not an implementation detail.

    let mut rows = vec![0.0f64; N * K];
    for y in 0..N {
        for u in 0..K {
            let mut s = 0.0;
            for x in 0..N {
                s += px[y * N + x] * COS[u][x];
            }
            rows[y * K + u] = s;
        }
    }
    let mut coef = vec![0.0f64; K * K];
    for u in 0..K {
        for v in 0..K {
            let mut s = 0.0;
            for y in 0..N {
                s += rows[y * K + v] * COS[u][y];
            }
            coef[u * K + v] = s;
        }
    }

    let mut vals: Vec<f64> = coef.to_vec();
    vals[0] = 0.0; // drop DC from the threshold as well as from the bits
    let mut sorted: Vec<f64> = vals.iter().skip(1).copied().collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = if sorted.is_empty() {
        0.0
    } else if sorted.len() % 2 == 1 {
        sorted[sorted.len() / 2]
    } else {
        (sorted[sorted.len() / 2 - 1] + sorted[sorted.len() / 2]) / 2.0
    };

    let mut hash = 0u64;
    for (i, &c) in coef.iter().enumerate() {
        if i != 0 && c > median {
            hash |= 1 << i;
        }
    }
    Ok(hash)
}

pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: usize, h: usize, v: u8) -> Luma {
        Luma::new(w, h, vec![v; w * h]).unwrap()
    }

    fn checker(w: usize, h: usize, size: usize) -> Luma {
        let data = (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                if (x / size + y / size).is_multiple_of(2) {
                    20
                } else {
                    230
                }
            })
            .collect();
        Luma::new(w, h, data).unwrap()
    }

    #[test]
    fn identical_images_score_one() {
        let a = checker(64, 64, 4);
        let s = ssim(&a, &a).unwrap();
        assert!(
            (s - 1.0).abs() < 1e-9,
            "SSIM of an image with itself was {s}"
        );
    }

    #[test]
    fn two_flat_fields_of_different_brightness_score_below_one() {
        let s = ssim(&solid(32, 32, 10), &solid(32, 32, 240)).unwrap();
        assert!(s < 0.2, "a black field vs a white field scored {s}");
    }

    #[test]
    fn an_inverted_image_scores_far_from_one() {
        let a = checker(64, 64, 4);
        let inv = Luma::new(64, 64, a.data.iter().map(|v| 255 - v).collect()).unwrap();
        let s = ssim(&a, &inv).unwrap();
        assert!(s < 0.1, "inverted scored {s}");
    }

    #[test]
    fn a_one_pixel_shift_degrades_but_does_not_collapse() {
        // This is the case that matters in practice: two engines placing the
        // same glyph one pixel apart. The score must drop, and it must not
        // drop to zero, or every cross-engine comparison reads as a total
        // mismatch.
        let a = checker(64, 64, 8);
        let mut d = a.data.clone();
        d.rotate_right(1);
        let b = Luma::new(64, 64, d).unwrap();
        let s = ssim(&a, &b).unwrap();
        assert!(s < 0.999, "a shift should be visible, got {s}");
        assert!(s > 0.0, "a one-pixel shift collapsed the score to {s}");
    }

    #[test]
    fn images_of_different_size_compare_over_their_overlap() {
        let a = checker(100, 80, 4);
        let b = checker(100, 60, 4);
        let s = ssim(&a, &b).unwrap();
        assert!(s > 0.99, "the overlapping region is identical, got {s}");
    }

    #[test]
    fn a_smaller_than_one_window_image_still_scores() {
        let s = ssim(&solid(3, 3, 100), &solid(3, 3, 100)).unwrap();
        assert!((s - 1.0).abs() < 1e-9);
    }

    #[test]
    fn phash_is_stable_for_the_same_image() {
        let a = checker(128, 128, 8);
        assert_eq!(phash(&a).unwrap(), phash(&a).unwrap());
    }

    #[test]
    fn phash_ignores_a_uniform_brightness_shift() {
        // A theme that lightened everything by 10 moved no layout. DC is
        // excluded from the hash precisely so this does not register.
        let a = checker(128, 128, 8);
        let b = Luma::new(
            128,
            128,
            a.data.iter().map(|v| v.saturating_add(10)).collect(),
        )
        .unwrap();
        let d = hamming(phash(&a).unwrap(), phash(&b).unwrap());
        assert!(d <= 2, "a brightness shift moved {d} bits");
    }

    #[test]
    fn phash_separates_structurally_different_images() {
        let a = checker(128, 128, 8);
        let b = checker(128, 128, 32);
        let d = hamming(phash(&a).unwrap(), phash(&b).unwrap());
        assert!(d > 8, "two different layouts differed by only {d} bits");
    }

    #[test]
    fn hamming_of_a_hash_with_itself_is_zero() {
        let h = phash(&checker(64, 64, 4)).unwrap();
        assert_eq!(hamming(h, h), 0);
    }

    #[test]
    fn the_dc_bit_is_never_set() {
        // Bit 0 carries overall brightness and is deliberately excluded.
        for size in [2, 4, 8, 16] {
            let h = phash(&checker(64, 64, size)).unwrap();
            assert_eq!(h & 1, 0, "DC bit set for size {size}");
        }
    }
}
