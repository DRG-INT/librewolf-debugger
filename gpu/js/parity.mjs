// Render parity in JavaScript: the C branch of the dispatch tree.
//
// Why this exists at all. The Rust crate can compare two PNGs on the GPU or
// the CPU, but neither reaches inside a page. A browser can render something
// only it can render — a canvas, a WebGL scene, a font the host does not have
// — and the only place to compare that is in the page itself. This module is
// that place, and it is also the fallback for a runtime with no `navigator.gpu`
// at all.
//
// The constraint that shapes every line below: **the numbers must match the
// Rust implementation exactly**, not approximately. A parity tool whose own
// two implementations disagree reports noise. So the operation order, the
// integer truncations and the constants are copied from `gpu/src/cpu.rs` and
// `gpu/src/image.rs` rather than rewritten idiomatically:
//
//   * luma is the integer Rec. 709 form with truncating division, not a
//     floating-point weighting;
//   * `resize` reproduces the same ceil-division index arithmetic;
//   * the DCT is separable, rows then columns, in that order, because
//     floating-point addition is not associative and a different order gives
//     a different last bit;
//   * SSIM uses population variance over non-overlapping 8x8 windows.
//
// JavaScript numbers are IEEE-754 doubles, the same as Rust's f64, so with the
// same order the results are bit-identical. `gpu/js/parity.test.mjs` checks
// that against vectors taken from the Rust tests.

import dctTable from "../testdata/dct-cos-8x32.json" with { type: "json" };

/// The shared cosine table, parsed once. Strings in the JSON rather than
/// numbers so that 17 significant digits survive any JSON tooling in between;
/// Number() parses them back to the exact same f64.
const DCT_COS = dctTable.cos.map((row) => Float64Array.from(row, Number));

// SSIM stabilisers from Wang et al. 2004 for 8-bit range:
// C1 = (0.01*255)^2, C2 = (0.03*255)^2.
export const C1 = 6.5025;
export const C2 = 58.5225;
export const WINDOW = 8;

/** @typedef {{width: number, height: number, data: Uint8Array}} Luma */

/**
 * RGBA bytes to 8-bit luma.
 *
 * Integer Rec. 709 with truncating division, matching `decode_png` in
 * `gpu/src/image.rs`. Comparison is on luma because subpixel antialiasing
 * differs between engines, so comparing colour channels reports differences
 * that are not defects.
 *
 * @param {Uint8Array|Uint8ClampedArray} rgba
 * @param {number} width
 * @param {number} height
 * @returns {Luma}
 */
export function lumaFromRgba(rgba, width, height) {
  if (rgba.length < width * height * 4) {
    throw new Error(
      `rgba has ${rgba.length} bytes, need ${width * height * 4} for ${width}x${height}`,
    );
  }
  const out = new Uint8Array(width * height);
  for (let i = 0, p = 0; i < out.length; i++, p += 4) {
    out[i] = Math.floor(
      (2126 * rgba[p] + 7152 * rgba[p + 1] + 722 * rgba[p + 2]) / 10000,
    );
  }
  return { width, height, data: out };
}

/**
 * Luma from anything with a 2D context: a canvas, an OffscreenCanvas, an
 * ImageBitmap already drawn into one.
 *
 * @param {HTMLCanvasElement|OffscreenCanvas} canvas
 * @returns {Luma}
 */
export function lumaFromCanvas(canvas) {
  const ctx = canvas.getContext("2d", { willReadFrequently: true });
  if (!ctx) throw new Error("no 2d context on this canvas");
  const { width, height } = canvas;
  const img = ctx.getImageData(0, 0, width, height);
  return lumaFromRgba(img.data, width, height);
}

/**
 * Box-filter resample.
 *
 * A box filter and not nearest-neighbour: at the scales the perceptual hash
 * uses, nearest drops whole rows of text, so two pages differing by one line
 * can hash identically. The index arithmetic mirrors `Luma::resize`.
 *
 * @param {Luma} img
 * @param {number} w
 * @param {number} h
 * @returns {Luma}
 */
export function resize(img, w, h) {
  if (w <= 0 || h <= 0) throw new Error(`cannot resize to ${w}x${h}`);
  if (w === img.width && h === img.height) {
    return { width: w, height: h, data: img.data.slice() };
  }
  const out = new Uint8Array(w * h);
  for (let oy = 0; oy < h; oy++) {
    const y0 = Math.floor((oy * img.height) / h);
    const y1 = Math.min(
      img.height,
      Math.max(y0 + 1, Math.floor(((oy + 1) * img.height + h - 1) / h)),
    );
    for (let ox = 0; ox < w; ox++) {
      const x0 = Math.floor((ox * img.width) / w);
      const x1 = Math.min(
        img.width,
        Math.max(x0 + 1, Math.floor(((ox + 1) * img.width + w - 1) / w)),
      );
      let sum = 0;
      let n = 0;
      for (let y = y0; y < y1; y++) {
        const row = y * img.width;
        for (let x = x0; x < x1; x++) {
          sum += img.data[row + x];
          n++;
        }
      }
      out[oy * w + ox] = Math.floor(sum / Math.max(n, 1));
    }
  }
  return { width: w, height: h, data: out };
}

/**
 * Crop both images to their common top-left region.
 *
 * Padding the shorter one with black would invent a large uniform difference
 * and drag SSIM down for a reason that has nothing to do with rendering. Two
 * browsers rarely agree on viewport height to the pixel.
 *
 * @param {Luma} a
 * @param {Luma} b
 * @returns {[Luma, Luma]}
 */
export function commonCrop(a, b) {
  const w = Math.min(a.width, b.width);
  const h = Math.min(a.height, b.height);
  if (w === 0 || h === 0) {
    throw new Error(
      `no overlap between ${a.width}x${a.height} and ${b.width}x${b.height}`,
    );
  }
  return [crop(a, w, h), crop(b, w, h)];
}

/**
 * @param {Luma} img
 * @param {number} w
 * @param {number} h
 * @returns {Luma}
 */
export function crop(img, w, h) {
  if (w > img.width || h > img.height) {
    throw new Error(`cannot crop ${img.width}x${img.height} to ${w}x${h}`);
  }
  if (w === img.width && h === img.height) {
    return { width: w, height: h, data: img.data.slice() };
  }
  const out = new Uint8Array(w * h);
  for (let y = 0; y < h; y++) {
    out.set(img.data.subarray(y * img.width, y * img.width + w), y * w);
  }
  return { width: w, height: h, data: out };
}

function windowSsim(a, b, x0, y0, w, h) {
  const n = w * h;
  let sa = 0;
  let sb = 0;
  let saa = 0;
  let sbb = 0;
  let sab = 0;
  for (let y = y0; y < y0 + h; y++) {
    const row = y * a.width;
    for (let x = x0; x < x0 + w; x++) {
      const va = a.data[row + x];
      const vb = b.data[row + x];
      sa += va;
      sb += vb;
      saa += va * va;
      sbb += vb * vb;
      sab += va * vb;
    }
  }
  const ma = sa / n;
  const mb = sb / n;
  // Population variance, matching the reference implementation's biased
  // estimator.
  const va = saa / n - ma * ma;
  const vb = sbb / n - mb * mb;
  const cab = sab / n - ma * mb;

  const num = (2 * ma * mb + C1) * (2 * cab + C2);
  const den = (ma * ma + mb * mb + C1) * (va + vb + C2);
  return den === 0 ? 1 : num / den;
}

/**
 * Mean SSIM over non-overlapping 8x8 windows.
 *
 * Non-overlapping rather than a sliding Gaussian window: the sliding form is
 * ~64x the work for a difference that changes no decision here, and the GPU
 * path maps one invocation to one window because of it.
 *
 * @param {Luma} a
 * @param {Luma} b
 * @returns {number}
 */
export function ssim(a, b) {
  const [ca, cb] = commonCrop(a, b);
  const wx = Math.floor(ca.width / WINDOW);
  const wy = Math.floor(ca.height / WINDOW);
  if (wx === 0 || wy === 0) {
    // Smaller than one window: compare the whole thing as a single block.
    return windowSsim(ca, cb, 0, 0, ca.width, ca.height);
  }
  let total = 0;
  for (let by = 0; by < wy; by++) {
    for (let bx = 0; bx < wx; bx++) {
      total += windowSsim(ca, cb, bx * WINDOW, by * WINDOW, WINDOW, WINDOW);
    }
  }
  return total / (wx * wy);
}

/**
 * 64-bit perceptual hash: 32x32 luma, 2D DCT-II, the 8x8 low-frequency corner
 * excluding DC, thresholded at its median.
 *
 * DC is excluded because it is overall brightness: including it makes the hash
 * sensitive to a theme change that moved nothing.
 *
 * Returned as a BigInt, because a Number cannot hold 64 bits exactly and
 * splitting it into halves would make the Hamming distance a different
 * function from the Rust one.
 *
 * @param {Luma} img
 * @returns {bigint}
 */
export function phash(img) {
  const N = 32;
  const K = 8;
  const small = resize(img, N, N);
  const px = Float64Array.from(small.data);

  // The cosines come from the shared table, not from Math.cos.
  //
  // V8 does not use the platform libm for Math.cos — it bundles its own
  // fdlibm port. The resulting last-bit difference from Rust's f64::cos is
  // amplified by the median threshold below into flipped hash bits: measured
  // at 3 of 64 for a 32-pixel checker, which is how this was found. With the
  // table shared, the DCT is pure multiply-add and bit-identical.
  //
  // Separable DCT-II, rows then columns, in that order. Floating-point
  // addition is not associative, so the order is part of the contract with
  // the Rust side rather than an implementation detail.
  const cos = DCT_COS;

  const rows = new Float64Array(N * K);
  for (let y = 0; y < N; y++) {
    for (let u = 0; u < K; u++) {
      let s = 0;
      for (let x = 0; x < N; x++) s += px[y * N + x] * cos[u][x];
      rows[y * K + u] = s;
    }
  }
  const coef = new Float64Array(K * K);
  for (let u = 0; u < K; u++) {
    for (let v = 0; v < K; v++) {
      let s = 0;
      for (let y = 0; y < N; y++) s += rows[y * K + v] * cos[u][y];
      coef[u * K + v] = s;
    }
  }

  const sorted = Array.from(coef.subarray(1)).sort((p, q) => p - q);
  let median = 0;
  if (sorted.length > 0) {
    median =
      sorted.length % 2 === 1
        ? sorted[(sorted.length - 1) / 2]
        : (sorted[sorted.length / 2 - 1] + sorted[sorted.length / 2]) / 2;
  }

  let hash = 0n;
  for (let i = 0; i < coef.length; i++) {
    if (i !== 0 && coef[i] > median) hash |= 1n << BigInt(i);
  }
  return hash;
}

/**
 * @param {bigint} a
 * @param {bigint} b
 * @returns {number}
 */
export function hamming(a, b) {
  let x = a ^ b;
  let n = 0;
  while (x !== 0n) {
    x &= x - 1n;
    n++;
  }
  return n;
}

/**
 * Plain-language reading of the two numbers, with the cross-engine caveat
 * attached. Identical wording to the Rust `reading()`, so a result read in a
 * page and one read from the CLI say the same thing.
 *
 * @param {number} ssimValue
 * @param {number} hammingValue
 * @returns {string}
 */
export function reading(ssimValue, hammingValue) {
  const structural =
    hammingValue === 0
      ? "the same page structurally"
      : hammingValue <= 5
        ? "structurally near-identical"
        : hammingValue <= 15
          ? "structurally similar"
          : "structurally different";
  const pixels =
    ssimValue > 0.999
      ? "pixel-identical"
      : ssimValue > 0.98
        ? "all but identical"
        : ssimValue > 0.9
          ? "close, with visible local differences"
          : ssimValue > 0.7
            ? "clearly different in places"
            : "substantially different";
  return (
    `${pixels} (SSIM ${ssimValue.toFixed(4)}), ${structural} ` +
    `(pHash distance ${hammingValue}). Across engines, font rasterisation and ` +
    `widget metrics differ by design — look at the images before calling ` +
    `this a defect.`
  );
}

/**
 * Compare two luma images. Mirrors the Rust `compare()` return shape so a
 * consumer does not have to care which side produced the answer.
 *
 * @param {Luma} a
 * @param {Luma} b
 * @returns {{ssim: number, phashA: string, phashB: string, phashHamming: number,
 *            backend: string, comparedWidth: number, comparedHeight: number,
 *            sizeMismatch: string|null, reading: string}}
 */
export function compare(a, b) {
  const sizeMismatch =
    a.width !== b.width || a.height !== b.height
      ? `${a.width}x${a.height} vs ${b.width}x${b.height}; scored over the ` +
        `${Math.min(a.width, b.width)}x${Math.min(a.height, b.height)} overlap`
      : null;
  const s = ssim(a, b);
  const ha = phash(a);
  const hb = phash(b);
  const ham = hamming(ha, hb);
  return {
    ssim: s,
    // Hex strings rather than BigInt, so the object survives JSON and
    // structured clone — which is how it crosses the extension's port.
    phashA: ha.toString(16).padStart(16, "0"),
    phashB: hb.toString(16).padStart(16, "0"),
    phashHamming: ham,
    backend: "js",
    comparedWidth: Math.min(a.width, b.width),
    comparedHeight: Math.min(a.height, b.height),
    sizeMismatch,
    reading: reading(s, ham),
  };
}
