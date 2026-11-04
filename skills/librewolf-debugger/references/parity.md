# Render parity

The claim "this works with every browser" is only worth something if it can be
checked. The check is: render the same URL in every engine available and
compare the pixels.

```bash
# two or more Gecko browsers, rendered and compared pairwise
lwdbg parity --url https://example.com/ \
             --browser /Applications/LibreWolf.app \
             --browser /Applications/Firefox.app

# two renders already in the store
lwdbg parity --render-a 1 --render-b 2 --backend gpu
```

Every pair is compared, so with three browsers the outlier is visible rather
than inferred. Results go into the `parity` table keyed by `(render_a,
render_b, backend)`, so a GPU and a CPU score for the same pair coexist instead
of one overwriting the other.

## Read the numbers as evidence, not as a verdict

Two numbers come back:

- **SSIM** over non-overlapping 8x8 windows, on 8-bit luma. 1.0 is identical.
- **pHash distance**: Hamming distance between two 64-bit perceptual hashes
  (32x32 luma → 2D DCT-II → the 8x8 low-frequency corner, thresholded at its
  median, DC excluded).

SSIM is sensitive to a one-pixel shift; pHash is not. That difference is the
point. Two engines that laid the page out identically but rasterised glyphs
differently give a **low-ish SSIM and a pHash distance near zero** — which
reads as "same page, different font rendering", exactly the distinction worth
making. A real layout difference moves both.

DC is excluded from the hash so a theme that lightened everything by ten
levels, moving nothing, does not register as a change.

Cross-engine scores below 1.0 are normal. Font rasterisation, widget metrics
and scrollbar width differ by design. A score locates a page worth looking at;
it does not decide anything.

## Luma, not colour

Comparison is on luma. Subpixel antialiasing differs between engines, so
comparing colour channels reports differences that are not defects. Luma keeps
the question about layout and glyph placement.

## Mismatched sizes are cropped, not padded

Two browsers rarely agree on viewport height to the pixel. Padding the shorter
one with black would invent a large uniform difference and drag SSIM down for a
reason unrelated to rendering. The comparison is over the common top-left
region, and the discarded extent is reported in `size_mismatch` rather than
being silently dropped.

## GPU and CPU

The CPU implementation is the **definition**; the shader is tested against it,
and `gpu_and_cpu_agree_within_float_tolerance` fails if they diverge by more
than 1e-4 (the shader accumulates in f32, the reference in f64).

`--backend auto` uses the GPU when there is one, falls back to the CPU when
there is not — including when the shader fails at run time — and always reports
which path produced the number. It never reports "auto".

Verified on this machine: **Apple M2 (Metal)**, GPU and CPU both returning
1.0000 on an identical pair. The same WGSL is intended to run on Vulkan on the
Void host and D3D12 on the Windows host.

## Why the GPU is here and not in the proxy

Deliberate placement. For HTTP-sized objects the kernel launch and the
host-device transfer dominate; BLAKE3 already does 1–3 GB/s on a single core
and the TLS work goes to the CPU's crypto extensions. A shader on the proxy's
hot path would be measurably **slower**.

Where it pays is many image pairs at once, which is what comparing a page
across eight browser builds is. The reduction is left on the host on purpose: a
1280x800 render is 16 000 windows, a multi-stage GPU reduction would save
microseconds, and it would add a second pipeline to keep in agreement with the
CPU reference.

## Only Gecko can be driven here

`--browser` launches and drives a browser, and that needs a transport. In this
build that means Gecko. A Blink or WebKit render has to be supplied as a stored
PNG and compared with `--render-a/--render-b`. The comparison itself is
engine-agnostic; the *driving* is not, which is the same asymmetry that makes
the proxy the backbone.

## A full-page capture that looks broken may be correct

On `neptunium.gd` the viewport and full-page captures came back pixel-identical
at 1366x683. That was not a bug: `document.documentElement.scrollHeight ==
clientHeight == 683`, a fixed-height single-screen layout with internal
scrolling. Check `scrollHeight` before concluding the capture ignored
`--full-page`.
