# Shaders and the GPU helper

```sh
lwdbg-gpu backends                  # what this machine runs, which compilers exist
lwdbg-gpu emit --out audit/shaders  # one WGSL source -> every target language
lwdbg-gpu verify                    # compile each with the platform's compiler
lwdbg-gpu compare a.png b.png       # parity, on the GPU
make shaders shaders-verify js-test
```

## Two binaries, and why

`lwdbg` links **no wgpu and no naga**. The GPU and shader work is a second
binary, `lwdbg-gpu`, built only when `./configure --with-gpu` asks for it.

That is not tidiness. Linking wgpu and naga into `lwdbg` under the
workspace's thin LTO is the peak-memory moment of the whole build: on this
8 GiB machine it stalled with `rustc` at 0% CPU and swap at 88%, and the link
never finished. Splitting means the tool anybody runs daily builds everywhere.

Verified rather than asserted:

```console
$ cargo tree -p lwdbg -e normal | grep -c -E 'wgpu|naga'
0
$ cargo tree -p lwdbg-gpu --features gpu -e normal | grep -c -E 'wgpu|naga'
12
```

**Cargo unifies features within one invocation**, so `cargo build -p lwdbg -p
lwdbg-gpu` would resolve `lwdbg-gpu` once with the feature on and `lwdbg`
would link wgpu through it. The Makefile therefore runs **two separate
invocations**. That is the only thing that makes the split real, and it is the
sort of detail that silently undoes this kind of work.

## What `lwdbg parity` does without the helper

| `--backend` | helper present | helper absent |
| --- | --- | --- |
| `cpu` | in-process CPU | in-process CPU |
| `auto` | delegates to the helper | in-process CPU, quietly — this is the documented fallback |
| `gpu` | delegates to the helper | **errors**, with the build command |

`auto` also falls back when the helper exists but cannot run — no adapter, a
driver problem — because a shader failure must not take the measurement with
it. An explicit `--backend gpu` never falls back: asking for the GPU and
getting the CPU without being told is how a benchmark becomes fiction.

Delegation costs two temporary PNGs and a subprocess, which is nothing beside
the GPU work itself.

## One shader source, five languages

`gpu/shaders/ssim.wgsl` is the only hand-written shader. SPIR-V, MSL, HLSL and
GLSL are all translated from it by naga, so there is no second copy to keep in
agreement. WGSL is the source because it is the only one of the five that
every backend can be reached *from*.

| Language | API | Extension | Verified by |
| --- | --- | --- | --- |
| WGSL | WebGPU | `.wgsl` | naga's validator, before anything is emitted |
| SPIR-V | Vulkan | `.spv` | `spirv-val`, or `glslangValidator -V` |
| MSL | Metal | `.metal` | `xcrun metal` → AIR |
| HLSL | Direct3D 12 | `.hlsl` | `dxc -T cs_6_0` |
| GLSL ES 3.10 | OpenGL ES / WebGL2 | `.comp` | `glslangValidator -S comp` |

Measured on this machine: all five emit (2.9–4.8 KB each); SPIR-V and GLSL are
**accepted with no diagnostics** by their validators; WGSL validates; MSL and
HLSL are skipped with their reasons.

## Emitting is not verifying

`verify` exists because "naga produced HLSL" and "HLSL a compiler accepts" are
different claims, and a release that shipped the first while reporting the
second would be worthless. So:

- a language whose compiler is **present and rejects** the shader → **FAIL**,
  and the gate goes red;
- a language whose compiler is **absent** → **SKIP, with the reason**, and the
  gate stays green.

That distinction is load-bearing and was got wrong once. `xcrun` exists on any
Mac with Command Line Tools, but the **Metal compiler is a separate Xcode
component**, so `xcrun -sdk macosx metal` fails with `unable to find utility
"metal"`. Checking only for `xcrun` reported a missing toolchain as a broken
shader. The probe is now `xcrun -sdk macosx -find metal`.

`dxc` has no supported macOS build at all, so HLSL is emitted here and
verified on the Windows host.

## The JavaScript fallback

`gpu/js/parity.mjs` computes the same SSIM and pHash in the browser. It is the
only path that reaches inside a page — a canvas, a WebGL scene, a font the
host does not have — and the fallback where there is no `navigator.gpu`.

It agrees with the Rust reference **bit for bit**, and that took a specific
fix. `gpu/testdata/vectors.json` holds one set of expected numbers;
`gpu/tests/golden_vectors.rs` and `gpu/js/parity.test.mjs` both assert against
it, so neither side owns them and a drift in either is caught.

### Why the DCT uses a table

The first cross-check failed: the two implementations disagreed on **3 of 64
bits** of the perceptual hash for a 32-pixel checker, while SSIM, the Hamming
distances and every image primitive matched exactly.

The cause is that **V8 does not use the platform libm for `Math.cos`** — it
bundles its own fdlibm port, for its own cross-platform determinism. So
JavaScript's cosine and Rust's `f64::cos` differ in the last bit, and the
median threshold in a perceptual hash amplifies that into flipped bits.

`gpu/testdata/dct-cos-8x32.json` therefore holds the 8×32 cosine table at 17
significant digits, which round-trips an f64 exactly in both languages.
`gpu/src/dct_table.rs` is generated from it so the Rust library needs no JSON
parser. With the table shared the DCT is pure multiply-add, which **is**
bit-identical in IEEE-754 for a fixed operation order — and both sides
document that order, because floating-point addition is not associative and
the order is part of the contract rather than an implementation detail.

Regenerate the table with:

```sh
cargo test -p lwdbg-gpu --features gpu --test golden_vectors \
    emit_dct_table -- --ignored --nocapture
```

### Other things both sides must do identically

- **luma is the integer Rec. 709 form** with truncating division, not a
  floating-point weighting;
- **resize is a box filter** with the same ceil-division index arithmetic. A
  box filter and not nearest-neighbour, because at the scales the hash uses
  nearest drops whole rows of text and two pages differing by one line hash
  the same;
- the hash crosses the extension's native messaging port as **hex**, because a
  `BigInt` does not survive structured clone.

16 JavaScript tests and 5 Rust golden-vector tests cover this.
