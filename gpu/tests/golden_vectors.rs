//! The Rust half of the cross-language agreement check.
//!
//! `gpu/testdata/vectors.json` holds one set of expected numbers. This file
//! asserts the Rust implementation against them; `gpu/js/parity.test.mjs`
//! asserts the JavaScript one against the same file. Neither owns the
//! numbers, so a drift in either is caught, and a "fix" that changes both to
//! agree on something new has to change the file deliberately.
//!
//! Why this matters more than it looks. A parity tool exists to say whether
//! two renders differ. If its own two implementations disagree, every answer
//! it gives is noise of unknown size. The numbers below are exact, not
//! approximate: IEEE-754 doubles with the same operation order produce the
//! same bits in both languages, and the JavaScript module documents the
//! places where that constrains how it is written.

use lwdbg_gpu::cpu;
use lwdbg_gpu::image::Luma;

fn vectors() -> serde_json::Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/vectors.json");
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
    serde_json::from_str(&text).expect("vectors.json must be valid JSON")
}

/// The helper the vectors were generated from: cells of `size` pixels filled
/// with 20 and 230 alternately.
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

fn solid(w: usize, h: usize, v: u8) -> Luma {
    Luma::new(w, h, vec![v; w * h]).unwrap()
}

/// `Array.prototype.rotate_right(1)` has no Rust equivalent on a Vec, so the
/// shift is spelled out to match what the JavaScript generator did.
fn shift_right_one(img: &Luma) -> Luma {
    let mut d = img.data.clone();
    d.rotate_right(1);
    Luma::new(img.width, img.height, d).unwrap()
}

fn brighten(img: &Luma, by: u8) -> Luma {
    Luma::new(
        img.width,
        img.height,
        img.data.iter().map(|v| v.saturating_add(by)).collect(),
    )
    .unwrap()
}

fn invert(img: &Luma) -> Luma {
    Luma::new(
        img.width,
        img.height,
        img.data.iter().map(|v| 255 - v).collect(),
    )
    .unwrap()
}

/// Both languages compute in f64 with the same operation order, so the values
/// agree far beyond this. The tolerance exists because the *literals in the
/// JSON* are decimal text, and the last digit of a round trip through decimal
/// is not guaranteed.
const EPS: f64 = 1e-12;

fn close(got: f64, want: f64, what: &str) {
    assert!(
        (got - want).abs() < EPS,
        "{what}: got {got}, vectors.json says {want} (difference {})",
        (got - want).abs()
    );
}

#[test]
fn ssim_matches_the_shared_vectors() {
    let v = vectors();
    let s = &v["ssim"];

    let c64_4 = checker(64, 64, 4);
    let c64_8 = checker(64, 64, 8);
    let c128_8 = checker(128, 128, 8);

    close(
        cpu::ssim(&c64_4, &c64_4).unwrap(),
        s["identical_checker64_4"].as_f64().unwrap(),
        "identical",
    );
    close(
        cpu::ssim(&c64_4, &invert(&c64_4)).unwrap(),
        s["inverted_checker64_4"].as_f64().unwrap(),
        "inverted",
    );
    close(
        cpu::ssim(&c64_8, &shift_right_one(&c64_8)).unwrap(),
        s["shifted_one_px_checker64_8"].as_f64().unwrap(),
        "one-pixel shift",
    );
    close(
        cpu::ssim(&solid(32, 32, 10), &solid(32, 32, 240)).unwrap(),
        s["solid_10_vs_240"].as_f64().unwrap(),
        "solid 10 vs 240",
    );

    // The localised perturbation from the GPU/CPU agreement test, which is
    // the only vector here that is neither 1.0 nor near-degenerate.
    let mut d = c128_8.data.clone();
    for y in 40..70 {
        for x in 30..90 {
            d[y * 128 + x] = d[y * 128 + x].wrapping_add(37);
        }
    }
    let perturbed = Luma::new(128, 128, d).unwrap();
    close(
        cpu::ssim(&c128_8, &perturbed).unwrap(),
        s["perturbed_region_128_8"].as_f64().unwrap(),
        "perturbed region",
    );
}

#[test]
fn phash_matches_the_shared_vectors() {
    let v = vectors();
    let p = &v["phash"];

    let hex = |h: u64| format!("{h:016x}");

    let c128_8 = checker(128, 128, 8);
    let c128_32 = checker(128, 128, 32);

    assert_eq!(
        hex(cpu::phash(&c128_8).unwrap()),
        p["checker128_8"].as_str().unwrap(),
        "checker 8px"
    );
    assert_eq!(
        hex(cpu::phash(&c128_32).unwrap()),
        p["checker128_32"].as_str().unwrap(),
        "checker 32px"
    );
    assert_eq!(
        hex(cpu::phash(&brighten(&c128_8, 10)).unwrap()),
        p["checker128_8_plus10"].as_str().unwrap(),
        "checker 8px brightened by 10"
    );
}

#[test]
fn hamming_distances_match_the_shared_vectors() {
    let v = vectors();
    let h = &v["hamming"];

    let c128_8 = checker(128, 128, 8);
    let c128_32 = checker(128, 128, 32);

    assert_eq!(
        cpu::hamming(cpu::phash(&c128_8).unwrap(), cpu::phash(&c128_32).unwrap()) as u64,
        h["checker8_vs_checker32"].as_u64().unwrap(),
        "two different layouts"
    );
    assert_eq!(
        cpu::hamming(
            cpu::phash(&c128_8).unwrap(),
            cpu::phash(&brighten(&c128_8, 10)).unwrap()
        ) as u64,
        h["brightness_shift_10"].as_u64().unwrap(),
        "a brightness shift must move almost no bits"
    );
}

#[test]
fn image_primitives_match_the_shared_vectors() {
    let v = vectors();
    let i = &v["image"];

    // Box filter, not nearest neighbour.
    let a = Luma::new(2, 2, vec![0, 0, 200, 200]).unwrap();
    assert_eq!(
        a.resize(1, 1).unwrap().data[0] as u64,
        i["resize_2x2_to_1x1_of_0_0_200_200"].as_u64().unwrap(),
        "2x2 -> 1x1 must average"
    );

    // Integer Rec. 709 with truncating division. (2126*128 + 7152*64 +
    // 722*32) / 10000 = 75.296, truncated to 75 — a float weighting would
    // round to 75 here too, but not everywhere, which is why both languages
    // do the integer form.
    let mut png = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut png, 1, 1);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&[128, 64, 32, 255]).unwrap();
    }
    let decoded = lwdbg_gpu::image::decode_png(&png).unwrap();
    assert_eq!(
        decoded.at(0, 0) as u64,
        i["luma_rec709_of_rgb_128_64_32"].as_u64().unwrap(),
        "Rec. 709 luma of (128, 64, 32)"
    );
}

#[test]
fn the_vectors_file_documents_what_it_is_for() {
    // A golden-vector file with no explanation becomes a set of magic numbers
    // nobody dares change. This asserts the explanation exists.
    let v = vectors();
    let c = v["$comment"].as_array().expect("$comment must be present");
    let text = c
        .iter()
        .filter_map(|x| x.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        text.contains("cpu.rs") && text.contains("parity.test.mjs"),
        "the comment must name both sides that assert these numbers"
    );
}

/// Generator for the shared DCT cosine table, run by hand:
///
/// ```sh
/// cargo test -p lwdbg-gpu --features gpu --test golden_vectors \
///     emit_dct_table -- --ignored --nocapture
/// ```
///
/// Why a table instead of calling `cos` on both sides. V8 does not use the
/// platform libm for `Math.cos` — it bundles its own fdlibm port, for
/// cross-platform determinism of its own. So Rust's `f64::cos` and
/// JavaScript's `Math.cos` differ in the last bit, and in a perceptual hash
/// that difference is amplified by the median threshold into flipped bits:
/// the two implementations disagreed on 3 of 64 bits for one pattern, which
/// is how this was found.
///
/// The table is 8x32 values fixed forever by K=8 and N=32. Printed at 17
/// significant digits, which round-trips an f64 exactly in both languages,
/// the DCT becomes pure multiply-add — and that *is* bit-identical in
/// IEEE-754 given the same operation order, which both sides document.
#[test]
#[ignore]
fn emit_dct_table() {
    const N: usize = 32;
    const K: usize = 8;
    let mut rows = Vec::new();
    for u in 0..K {
        let mut row = Vec::new();
        for x in 0..N {
            let v =
                (std::f64::consts::PI * (2.0 * x as f64 + 1.0) * u as f64 / (2.0 * N as f64)).cos();
            row.push(format!("{v:.17e}"));
        }
        rows.push(row);
    }
    println!("DCT_TABLE_BEGIN");
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "$comment": [
                "Cosine table for the 32-point DCT-II used by the perceptual hash,",
                "the first 8 frequencies. Shared by gpu/src/cpu.rs and",
                "gpu/js/parity.mjs so both compute the same coefficients.",
                "",
                "Not computed at run time on either side: V8 bundles its own fdlibm",
                "for Math.cos rather than using the platform libm, so its cosine",
                "differs from Rust's in the last bit. The median threshold in a",
                "perceptual hash amplifies that into flipped bits — measured at 3 of",
                "64 for one pattern. With the table shared, the DCT is pure",
                "multiply-add, which is bit-identical in IEEE-754 for a fixed",
                "operation order.",
                "",
                "17 significant digits round-trips an f64 exactly in both languages.",
                "Regenerate with: cargo test -p lwdbg-gpu --features gpu --test",
                "golden_vectors emit_dct_table -- --ignored --nocapture"
            ],
            "n": N,
            "k": K,
            "cos": rows
        }))
        .unwrap()
    );
    println!("DCT_TABLE_END");
}
