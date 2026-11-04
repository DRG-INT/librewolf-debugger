// The JavaScript half of the cross-language agreement check.
//
//   node --test gpu/js/
//
// `gpu/testdata/vectors.json` holds one set of expected numbers.
// `gpu/tests/golden_vectors.rs` asserts the Rust implementation against them;
// this file asserts this one. Neither owns the numbers, so a drift in either
// is caught, and changing them has to be deliberate.
//
// The agreement is exact, not approximate — see the note on the DCT table in
// parity.mjs for why that is achievable at all.

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  ssim,
  phash,
  hamming,
  resize,
  crop,
  commonCrop,
  lumaFromRgba,
  compare,
  reading,
  C1,
  C2,
  WINDOW,
} from "./parity.mjs";
import vectors from "../testdata/vectors.json" with { type: "json" };

// The helpers the vectors were generated from, matching gpu/src/cpu.rs tests.
const checker = (w, h, size) => {
  const d = new Uint8Array(w * h);
  for (let i = 0; i < w * h; i++) {
    const x = i % w;
    const y = Math.floor(i / w);
    d[i] = (Math.floor(x / size) + Math.floor(y / size)) % 2 === 0 ? 20 : 230;
  }
  return { width: w, height: h, data: d };
};
const solid = (w, h, v) => ({
  width: w,
  height: h,
  data: new Uint8Array(w * h).fill(v),
});
const invert = (img) => ({
  ...img,
  data: Uint8Array.from(img.data, (v) => 255 - v),
});
const brighten = (img, by) => ({
  ...img,
  data: Uint8Array.from(img.data, (v) => Math.min(255, v + by)),
});
const shiftRightOne = (img) => {
  const d = Uint8Array.from(img.data);
  const last = d[d.length - 1];
  for (let i = d.length - 1; i > 0; i--) d[i] = d[i - 1];
  d[0] = last;
  return { ...img, data: d };
};
const hex = (b) => b.toString(16).padStart(16, "0");

// Both sides compute in f64 with the same operation order, so they agree far
// beyond this. The tolerance exists only because the literals in the JSON are
// decimal text.
const EPS = 1e-12;

test("constants match the reference implementation", () => {
  assert.equal(C1, 6.5025);
  assert.equal(C2, 58.5225);
  assert.equal(WINDOW, 8);
});

test("SSIM matches the shared vectors", () => {
  const s = vectors.ssim;
  const c64_4 = checker(64, 64, 4);
  const c64_8 = checker(64, 64, 8);
  const c128_8 = checker(128, 128, 8);

  const close = (got, want, what) =>
    assert.ok(
      Math.abs(got - want) < EPS,
      `${what}: got ${got}, vectors.json says ${want}`,
    );

  close(ssim(c64_4, c64_4), s.identical_checker64_4, "identical");
  close(ssim(c64_4, invert(c64_4)), s.inverted_checker64_4, "inverted");
  close(
    ssim(c64_8, shiftRightOne(c64_8)),
    s.shifted_one_px_checker64_8,
    "one-pixel shift",
  );
  close(
    ssim(solid(32, 32, 10), solid(32, 32, 240)),
    s.solid_10_vs_240,
    "solid 10 vs 240",
  );

  const d = Uint8Array.from(c128_8.data);
  for (let y = 40; y < 70; y++) {
    for (let x = 30; x < 90; x++) d[y * 128 + x] = (d[y * 128 + x] + 37) & 0xff;
  }
  close(
    ssim(c128_8, { width: 128, height: 128, data: d }),
    s.perturbed_region_128_8,
    "perturbed region",
  );
});

test("pHash matches the shared vectors, bit for bit", () => {
  const p = vectors.phash;
  assert.equal(hex(phash(checker(128, 128, 8))), p.checker128_8);
  assert.equal(hex(phash(checker(128, 128, 32))), p.checker128_32);
  assert.equal(
    hex(phash(brighten(checker(128, 128, 8), 10))),
    p.checker128_8_plus10,
  );
});

test("Hamming distances match the shared vectors", () => {
  const h = vectors.hamming;
  const a = phash(checker(128, 128, 8));
  assert.equal(
    hamming(a, phash(checker(128, 128, 32))),
    h.checker8_vs_checker32,
  );
  // A brightness shift moves almost no bits, because DC is excluded.
  assert.equal(
    hamming(a, phash(brighten(checker(128, 128, 8), 10))),
    h.brightness_shift_10,
  );
});

test("image primitives match the shared vectors", () => {
  const i = vectors.image;
  // Box filter, not nearest neighbour.
  assert.equal(
    resize({ width: 2, height: 2, data: Uint8Array.from([0, 0, 200, 200]) }, 1, 1)
      .data[0],
    i.resize_2x2_to_1x1_of_0_0_200_200,
  );
  // Integer Rec. 709 with truncating division.
  assert.equal(
    lumaFromRgba(Uint8Array.from([128, 64, 32, 255]), 1, 1).data[0],
    i.luma_rec709_of_rgb_128_64_32,
  );
});

// ---------------------------------------------------------- own properties
//
// The same properties the Rust tests assert, so a reader of either side sees
// the same guarantees rather than having to infer them.

test("an image against itself is identical", () => {
  const a = checker(128, 128, 8);
  const r = compare(a, a);
  assert.ok(r.ssim > 0.999, `ssim was ${r.ssim}`);
  assert.equal(r.phashHamming, 0);
  assert.equal(r.backend, "js");
  assert.equal(r.sizeMismatch, null);
  assert.match(r.reading, /pixel-identical/);
});

test("a size difference is reported rather than hidden", () => {
  const r = compare(checker(128, 128, 8), checker(128, 96, 8));
  assert.match(r.sizeMismatch, /128x128 vs 128x96/);
  assert.equal(r.comparedWidth, 128);
  assert.equal(r.comparedHeight, 96);
});

test("mismatched sizes are cropped, never padded", () => {
  // Padding would invent a uniform difference and drag the score down for a
  // reason unrelated to rendering.
  const [a, b] = commonCrop(checker(100, 80, 4), checker(100, 60, 4));
  assert.equal(a.height, 60);
  assert.equal(b.height, 60);
  assert.equal(a.data.length, 100 * 60);
  assert.ok(ssim(checker(100, 80, 4), checker(100, 60, 4)) > 0.99);
});

test("no overlap is an error, not an empty comparison", () => {
  assert.throws(
    () => commonCrop(checker(10, 10, 2), { width: 0, height: 0, data: new Uint8Array(0) }),
    /no overlap/,
  );
});

test("a thin line survives the downscale the hash uses", () => {
  // Nearest-neighbour would drop it, and two pages differing by one line
  // would then hash identically.
  const w = 32;
  const h = 32;
  const d = new Uint8Array(w * h);
  for (let x = 0; x < w; x++) d[7 * w + x] = 255;
  const s = resize({ width: w, height: h, data: d }, 8, 8);
  assert.ok(Array.from(s.data).some((v) => v > 0), "the line vanished");
});

test("the DC bit is never set", () => {
  // Bit 0 carries overall brightness and is deliberately excluded.
  for (const size of [2, 4, 8, 16]) {
    assert.equal(phash(checker(64, 64, size)) & 1n, 0n, `size ${size}`);
  }
});

test("a one-pixel shift degrades but does not collapse", () => {
  const a = checker(64, 64, 8);
  const s = ssim(a, shiftRightOne(a));
  assert.ok(s < 0.999, `a shift should be visible, got ${s}`);
  assert.ok(s > 0, `a one-pixel shift collapsed the score to ${s}`);
});

test("crop rejects growing an image", () => {
  assert.throws(() => crop(checker(8, 8, 2), 16, 16), /cannot crop/);
});

test("lumaFromRgba refuses a short buffer rather than reading past it", () => {
  assert.throws(() => lumaFromRgba(new Uint8Array(8), 4, 4), /need/);
});

test("the reading always carries the cross-engine caveat", () => {
  for (const [s, h] of [
    [1.0, 0],
    [0.5, 30],
    [0.95, 3],
  ]) {
    assert.match(reading(s, h), /font rasterisation/);
  }
});

test("the hash is returned as hex so it survives JSON", () => {
  // The extension carries this across a native messaging port, where a
  // BigInt would not survive structured clone.
  const r = compare(checker(64, 64, 4), checker(64, 64, 8));
  assert.match(r.phashA, /^[0-9a-f]{16}$/);
  assert.match(r.phashB, /^[0-9a-f]{16}$/);
  assert.equal(typeof r.phashHamming, "number");
});
