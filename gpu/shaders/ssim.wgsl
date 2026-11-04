// Per-window SSIM.
//
// One invocation computes one 8x8 window's SSIM and writes it to `out`. The
// host sums the results. The reduction is left on the host deliberately: a
// multi-stage GPU reduction would save a few microseconds on an array that is
// already small (a 1280x800 render is 16000 windows) and would add a second
// pipeline to keep in agreement with the CPU reference.
//
// Statistics are accumulated in f32. The CPU reference uses f64, so the two
// agree to about 1e-5 rather than exactly; the parity test asserts that
// tolerance rather than equality, and the SSoT records which backend produced
// a score so a disagreement is visible instead of averaged away.

struct Params {
    width: u32,
    height: u32,
    windows_x: u32,
    windows_y: u32,
    window: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
};

@group(0) @binding(0) var<storage, read> img_a: array<f32>;
@group(0) @binding(1) var<storage, read> img_b: array<f32>;
@group(0) @binding(2) var<storage, read_write> out: array<f32>;
@group(0) @binding(3) var<uniform> p: Params;

// (0.01 * 255)^2 and (0.03 * 255)^2, 8-bit dynamic range.
const C1: f32 = 6.5025;
const C2: f32 = 58.5225;

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let bx = gid.x;
    let by = gid.y;
    if (bx >= p.windows_x || by >= p.windows_y) {
        return;
    }

    let x0 = bx * p.window;
    let y0 = by * p.window;

    var sa: f32 = 0.0;
    var sb: f32 = 0.0;
    var saa: f32 = 0.0;
    var sbb: f32 = 0.0;
    var sab: f32 = 0.0;

    for (var dy: u32 = 0u; dy < p.window; dy = dy + 1u) {
        let row = (y0 + dy) * p.width;
        for (var dx: u32 = 0u; dx < p.window; dx = dx + 1u) {
            let i = row + x0 + dx;
            let va = img_a[i];
            let vb = img_b[i];
            sa = sa + va;
            sb = sb + vb;
            saa = saa + va * va;
            sbb = sbb + vb * vb;
            sab = sab + va * vb;
        }
    }

    let n = f32(p.window * p.window);
    let ma = sa / n;
    let mb = sb / n;
    let va = saa / n - ma * ma;
    let vb = sbb / n - mb * mb;
    let cab = sab / n - ma * mb;

    let num = (2.0 * ma * mb + C1) * (2.0 * cab + C2);
    let den = (ma * ma + mb * mb + C1) * (va + vb + C2);

    var score: f32 = 1.0;
    if (den != 0.0) {
        score = num / den;
    }
    out[by * p.windows_x + bx] = score;
}
