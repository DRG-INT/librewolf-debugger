//! `lwdbg-gpu` — the shader and GPU side, as its own binary.
//!
//! Separate from `lwdbg` on purpose, and the reason is measured rather than
//! aesthetic. Linking wgpu and naga under the workspace's thin LTO is the
//! peak-memory moment of the whole build: on this 8 GiB machine it stalled
//! with rustc at 0% CPU and swap at 88%, and the link never finished. Keeping
//! them in a second binary means the tool that measures page loads — which is
//! the thing anybody runs daily — builds everywhere, and the GPU work is
//! opt-in through `./configure --with-gpu`.
//!
//! The split is also honest about capability. `lwdbg parity` keeps working
//! without this binary: the CPU implementation is the reference the shader is
//! tested against, so nothing is lost functionally, only speed on large
//! batches.
//!
//! ```text
//!   lwdbg-gpu backends            what this machine can actually run
//!   lwdbg-gpu emit --lang msl     one WGSL source -> every target language
//!   lwdbg-gpu verify              compile each one with the platform compiler
//!   lwdbg-gpu compare a.png b.png SSIM and pHash, on the GPU where there is one
//! ```

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use lwdbg_gpu::translate::{self, Lang};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "lwdbg-gpu",
    about = "Shader translation, GPU parity, and compile verification",
    long_about = "The GPU half of lwdbg, in its own binary so that the main \
                  tool never links wgpu or naga.\n\n\
                  shaders/ssim.wgsl is the only hand-written shader here. SPIR-V, \
                  MSL, HLSL and GLSL are all translated from it, so there is no \
                  second copy to keep in agreement.",
    version
)]
struct Cli {
    /// Machine-readable output.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// What this machine can run, and which shader compilers are present.
    Backends,
    /// Translate the shader. Without --lang, every language.
    Emit {
        /// wgsl | spirv | msl | hlsl | glsl — or vulkan, metal, directx, webgl.
        #[arg(long)]
        lang: Option<String>,
        /// Write into this directory instead of standard output.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Compile each emitted shader with the platform's own compiler.
    ///
    /// This is what separates "naga produced HLSL" from "HLSL a compiler
    /// accepts". A language with no compiler here is reported as skipped,
    /// with the reason — never as passing.
    Verify {
        #[arg(long)]
        lang: Option<String>,
        /// Where to put the intermediate files.
        #[arg(long)]
        scratch: Option<PathBuf>,
    },
    /// Compare two PNG screenshots.
    Compare {
        a: PathBuf,
        b: PathBuf,
        /// auto | gpu | cpu
        #[arg(long, default_value = "auto")]
        backend: String,
    },
    /// The golden vectors shared with the JavaScript implementation.
    Vectors,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.cmd {
        Cmd::Backends => backends(cli.json),
        Cmd::Emit { lang, out } => emit(lang.as_deref(), out.as_deref(), cli.json),
        Cmd::Verify { lang, scratch } => verify(lang.as_deref(), scratch.as_deref(), cli.json),
        Cmd::Compare { a, b, backend } => compare(a, b, backend, cli.json),
        Cmd::Vectors => vectors(cli.json),
    }
}

fn parse_lang(s: &str) -> Result<Lang> {
    Lang::parse(s).ok_or_else(|| {
        let names: Vec<&str> = Lang::ALL.iter().map(|l| l.as_str()).collect();
        anyhow!(
            "unknown shader language {s:?}; known: {}. \
             API names also work: vulkan, metal, directx, opengl, webgl.",
            names.join(", ")
        )
    })
}

fn backends(json: bool) -> Result<()> {
    let adapter = match lwdbg_gpu::gpu::Gpu::new() {
        Ok(g) => serde_json::json!({
            "available": true,
            "adapter": g.adapter_name,
            "backend": g.backend,
        }),
        Err(e) => serde_json::json!({
            "available": false,
            // Not a failure: a headless CI box and the Void host legitimately
            // have no adapter, and the CPU path is the reference anyway.
            "reason": e.to_string(),
        }),
    };

    let langs: Vec<serde_json::Value> = Lang::ALL
        .iter()
        .map(|&l| {
            serde_json::json!({
                "lang": l.as_str(),
                "api": l.api(),
                "extension": l.extension(),
                "binary": l.is_binary(),
            })
        })
        .collect();

    let out = serde_json::json!({
        "gpu": adapter,
        "shader_languages": langs,
        "shader_source": "shaders/ssim.wgsl",
    });

    if json {
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!("GPU");
    if out["gpu"]["available"] == serde_json::Value::Bool(true) {
        println!(
            "  {} ({})",
            out["gpu"]["adapter"].as_str().unwrap_or("?"),
            out["gpu"]["backend"].as_str().unwrap_or("?")
        );
    } else {
        println!(
            "  none: {}",
            out["gpu"]["reason"].as_str().unwrap_or("unknown")
        );
        println!("  parity still works; it reports backend: cpu");
    }
    println!("\nShader languages, all translated from shaders/ssim.wgsl");
    for l in Lang::ALL {
        println!("  {:<6} -> {:<24} .{}", l.as_str(), l.api(), l.extension());
    }
    Ok(())
}

fn emit(lang: Option<&str>, out_dir: Option<&std::path::Path>, json: bool) -> Result<()> {
    let langs: Vec<Lang> = match lang {
        Some(s) => vec![parse_lang(s)?],
        None => Lang::ALL.to_vec(),
    };

    let mut report = Vec::new();
    let mut failed = 0;

    for l in langs {
        match translate::emit(translate::SSIM_WGSL, l) {
            Ok(e) => {
                let bytes = e.to_bytes();
                match out_dir {
                    Some(d) => {
                        std::fs::create_dir_all(d)
                            .with_context(|| format!("creating {}", d.display()))?;
                        let p = d.join(format!("ssim.{}", l.extension()));
                        std::fs::write(&p, &bytes)
                            .with_context(|| format!("writing {}", p.display()))?;
                        report.push(serde_json::json!({
                            "lang": l.as_str(), "ok": true,
                            "bytes": bytes.len(), "path": p.display().to_string(),
                        }));
                        if !json {
                            println!(
                                "{:<6} {:>8} bytes  {}",
                                l.as_str(),
                                bytes.len(),
                                p.display()
                            );
                        }
                    }
                    None => {
                        if let Some(text) = e.as_text() {
                            if !json {
                                println!("// ===== {} ({}) =====", l.as_str(), l.api());
                                print!("{text}");
                                if !text.ends_with('\n') {
                                    println!();
                                }
                            }
                            report.push(serde_json::json!({
                                "lang": l.as_str(), "ok": true,
                                "bytes": bytes.len(), "source": text,
                            }));
                        } else {
                            // SPIR-V is binary; printing it to a terminal
                            // would be useless and would corrupt the stream.
                            if !json {
                                println!(
                                    "{:<6} {:>8} bytes  (binary — use --out to write it)",
                                    l.as_str(),
                                    bytes.len()
                                );
                            }
                            report.push(serde_json::json!({
                                "lang": l.as_str(), "ok": true,
                                "bytes": bytes.len(), "binary": true,
                            }));
                        }
                    }
                }
            }
            Err(e) => {
                failed += 1;
                let msg = format!("{e:#}");
                if !json {
                    println!("{:<6} FAILED  {}", l.as_str(), msg);
                }
                report.push(serde_json::json!({
                    "lang": l.as_str(), "ok": false, "error": msg,
                }));
            }
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn verify(lang: Option<&str>, scratch: Option<&std::path::Path>, json: bool) -> Result<()> {
    // Not the shared temp dir. These are generated shaders rather than
    // secrets, but a predictable path in a world-writable directory is still
    // somewhere an attacker can pre-create a symlink and have this process
    // overwrite whatever it points at. The tool's own directory costs nothing.
    let scratch = match scratch {
        Some(p) => p.to_path_buf(),
        None => {
            let home = std::env::var_os("LWDBG_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/lwdbg"))
                })
                .ok_or_else(|| anyhow!("neither LWDBG_HOME nor HOME is set"))?;
            let d = home.join("shaders");
            std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700))
                    .with_context(|| format!("securing {}", d.display()))?;
            }
            d
        }
    };

    let langs: Vec<Lang> = match lang {
        Some(s) => vec![parse_lang(s)?],
        None => Lang::ALL.to_vec(),
    };

    let mut report = Vec::new();
    let mut failed = 0;
    let mut skipped = 0;

    for l in langs {
        let emitted = translate::emit(translate::SSIM_WGSL, l);
        let row = match emitted {
            Err(e) => {
                failed += 1;
                serde_json::json!({
                    "lang": l.as_str(), "ok": false, "stage": "emit",
                    "error": format!("{e:#}"),
                })
            }
            Ok(e) => match translate::verify(l, &e.to_bytes(), &scratch) {
                Err(err) => {
                    failed += 1;
                    serde_json::json!({
                        "lang": l.as_str(), "ok": false, "stage": "verify",
                        "error": format!("{err:#}"),
                    })
                }
                Ok(v) => {
                    if v.skipped.is_some() {
                        skipped += 1;
                    } else if !v.ok {
                        failed += 1;
                    }
                    serde_json::json!({
                        "lang": l.as_str(),
                        "api": l.api(),
                        "ok": v.ok,
                        "compiler": v.compiler,
                        "skipped": v.skipped,
                        "detail": v.detail,
                    })
                }
            },
        };

        if !json {
            let verdict = if row["skipped"].is_string() {
                "SKIP"
            } else if row["ok"] == serde_json::Value::Bool(true) {
                "ok"
            } else {
                "FAIL"
            };
            println!(
                "{:<5} {:<6} {:<24} {}",
                verdict,
                l.as_str(),
                row["compiler"].as_str().unwrap_or("-"),
                row["skipped"]
                    .as_str()
                    .or_else(|| row["detail"].as_str())
                    .or_else(|| row["error"].as_str())
                    .unwrap_or("")
            );
        }
        report.push(row);
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "\n{} verified, {} skipped, {} failed. A skip is not a pass: the \
             language was emitted but nothing here can compile it.",
            report.len() - skipped - failed,
            skipped,
            failed
        );
    }

    // Skips do not fail the command. A machine without dxc is not a broken
    // release; a shader that dxc rejects is.
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn compare(a: &std::path::Path, b: &std::path::Path, backend: &str, json: bool) -> Result<()> {
    let backend = lwdbg_gpu::Backend::parse(backend)
        .ok_or_else(|| anyhow!("backend must be auto, gpu or cpu"))?;
    let pa = std::fs::read(a).with_context(|| format!("reading {}", a.display()))?;
    let pb = std::fs::read(b).with_context(|| format!("reading {}", b.display()))?;
    let p = lwdbg_gpu::compare_png(&pa, &pb, backend)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&p)?);
    } else {
        println!(
            "SSIM {:.4}   pHash distance {}   {}x{}   backend {}{}",
            p.ssim,
            p.phash_hamming,
            p.compared_width,
            p.compared_height,
            p.backend,
            p.adapter.map(|x| format!(" [{x}]")).unwrap_or_default()
        );
        if let Some(m) = &p.size_mismatch {
            println!("size  {m}");
        }
        println!("{}", p.reading);
    }
    Ok(())
}

fn vectors(json: bool) -> Result<()> {
    // The same file the JavaScript test asserts against. Printed rather than
    // recomputed so there is one set of numbers, not two that drift.
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/vectors.json");
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    if json {
        print!("{text}");
    } else {
        println!("Golden vectors, shared with gpu/js/parity.test.mjs:");
        print!("{text}");
    }
    Ok(())
}
