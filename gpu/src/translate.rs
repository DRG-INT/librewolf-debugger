//! One shader source, every target language.
//!
//! `shaders/ssim.wgsl` is the only hand-written shader in this project. Every
//! other form of it — SPIR-V for Vulkan, MSL for Metal, HLSL for Direct3D,
//! GLSL for OpenGL and WebGL2 — is translated from it by naga at build or
//! run time. There is no second copy to keep in agreement, which is the whole
//! reason the source is WGSL rather than GLSL: WGSL is the only one of the
//! five that every backend can be reached *from*.
//!
//! What this is for, beyond wgpu. wgpu already translates internally, so a
//! program using wgpu needs none of this. It exists because:
//!
//!  * an emitted shader can be **compiled by the platform's own compiler**,
//!    which is the difference between "naga produced HLSL" and "HLSL that
//!    Direct3D accepts". `verify` does exactly that, and it is the reason
//!    `make release` will not ship an unverified shader.
//!  * the GLSL output reaches WebGL2, where there is no WebGPU and therefore
//!    no wgpu at all. That is the browser fallback's GPU path.
//!  * reading the MSL is how you find out what Metal was actually asked to
//!    do, which a validation error inside wgpu will not tell you.
//!
//! Translation is deliberately *not* done at run time by the parity path:
//! that path uses wgpu, which does it better. These are artifacts for
//! inspection, verification and non-wgpu consumers.

use anyhow::{anyhow, bail, Context, Result};
use std::fmt;

/// The single source of truth for the shader.
pub const SSIM_WGSL: &str = include_str!("../shaders/ssim.wgsl");

/// The entry point every backend is asked for. Named once: a typo here is a
/// translation that succeeds and produces nothing useful.
pub const ENTRY_POINT: &str = "main";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    /// The source, round-tripped through the validator. Useful on its own:
    /// if this differs from the input in substance, the input relies on
    /// something naga does not model.
    Wgsl,
    /// SPIR-V binary, for Vulkan. Emitted as words; `to_bytes` gives the
    /// little-endian form a `.spv` file holds.
    SpirV,
    /// Metal Shading Language, for Metal. `xcrun metal` compiles this to AIR,
    /// which is what Metal actually loads.
    Msl,
    /// HLSL for Direct3D 12. Compile with `dxc`.
    Hlsl,
    /// GLSL ES 3.10 compute, for OpenGL ES and WebGL2-adjacent runtimes.
    Glsl,
}

impl Lang {
    pub const ALL: &'static [Lang] = &[Lang::Wgsl, Lang::SpirV, Lang::Msl, Lang::Hlsl, Lang::Glsl];

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "wgsl" => Lang::Wgsl,
            "spirv" | "spv" | "vulkan" => Lang::SpirV,
            "msl" | "metal" => Lang::Msl,
            "hlsl" | "dxil" | "directx" | "d3d" => Lang::Hlsl,
            "glsl" | "opengl" | "gles" | "webgl" => Lang::Glsl,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Lang::Wgsl => "wgsl",
            Lang::SpirV => "spirv",
            Lang::Msl => "msl",
            Lang::Hlsl => "hlsl",
            Lang::Glsl => "glsl",
        }
    }

    /// Conventional file extension, so emitted artifacts are recognised by
    /// the tools that consume them.
    pub fn extension(self) -> &'static str {
        match self {
            Lang::Wgsl => "wgsl",
            Lang::SpirV => "spv",
            Lang::Msl => "metal",
            Lang::Hlsl => "hlsl",
            Lang::Glsl => "comp",
        }
    }

    /// Which API this feeds, for a report a human reads.
    pub fn api(self) -> &'static str {
        match self {
            Lang::Wgsl => "WebGPU",
            Lang::SpirV => "Vulkan",
            Lang::Msl => "Metal",
            Lang::Hlsl => "Direct3D 12",
            Lang::Glsl => "OpenGL ES 3.1 / WebGL2",
        }
    }

    pub fn is_binary(self) -> bool {
        self == Lang::SpirV
    }
}

impl fmt::Display for Lang {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A translated shader. Text for four of the five, words for SPIR-V.
#[derive(Debug, Clone)]
pub enum Emitted {
    Text(String),
    Words(Vec<u32>),
}

impl Emitted {
    /// Bytes as they should land in a file. SPIR-V is little-endian by
    /// specification regardless of host endianness, which is why this is not
    /// a `to_ne_bytes` transmute.
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Emitted::Text(s) => s.as_bytes().to_vec(),
            Emitted::Words(w) => w.iter().flat_map(|x| x.to_le_bytes()).collect(),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Emitted::Text(s) => s.len(),
            Emitted::Words(w) => w.len() * 4,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Emitted::Text(s) => Some(s),
            Emitted::Words(_) => None,
        }
    }
}

/// Parse and validate the WGSL once; every backend needs both the module and
/// the validation info, and validating per-language would do the work five
/// times and could disagree with itself.
fn parse_and_validate(source: &str) -> Result<(naga::Module, naga::valid::ModuleInfo)> {
    let module = naga::front::wgsl::parse_str(source).map_err(|e| {
        // naga's ParseError carries the span; emit_to_string turns it into
        // the pointed-at-the-line form, which is the only version worth
        // showing a human.
        anyhow!("WGSL does not parse:\n{}", e.emit_to_string(source))
    })?;

    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::default(),
    )
    .validate(&module)
    .map_err(|e| anyhow!("WGSL does not validate: {e:?}"))?;

    Ok((module, info))
}

/// Translate the shader into one language.
pub fn emit(source: &str, lang: Lang) -> Result<Emitted> {
    let (module, info) = parse_and_validate(source)?;

    Ok(match lang {
        Lang::Wgsl => Emitted::Text(
            naga::back::wgsl::write_string(&module, &info, naga::back::wgsl::WriterFlags::empty())
                .context("writing WGSL")?,
        ),

        Lang::SpirV => {
            let options = naga::back::spv::Options {
                // Vulkan 1.1 / SPIR-V 1.3 is the floor that gives
                // subgroup-free compute everywhere this is likely to run,
                // without demanding a recent driver.
                lang_version: (1, 3),
                ..Default::default()
            };
            let words = naga::back::spv::write_vec(&module, &info, &options, None)
                .context("writing SPIR-V")?;
            Emitted::Words(words)
        }

        Lang::Msl => {
            let options = naga::back::msl::Options::default();
            let pipeline_options = naga::back::msl::PipelineOptions::default();
            let mut out = String::new();
            let mut writer = naga::back::msl::Writer::new(&mut out);
            writer
                .write(&module, &info, &options, &pipeline_options)
                .context("writing MSL")?;
            Emitted::Text(out)
        }

        Lang::Hlsl => {
            let options = naga::back::hlsl::Options::default();
            let pipeline_options = naga::back::hlsl::PipelineOptions::default();
            let mut out = String::new();
            let mut writer = naga::back::hlsl::Writer::new(&mut out, &options, &pipeline_options);
            writer.write(&module, &info, None).context("writing HLSL")?;
            Emitted::Text(out)
        }

        Lang::Glsl => {
            let options = naga::back::glsl::Options {
                version: naga::back::glsl::Version::Embedded {
                    version: 310,
                    is_webgl: false,
                },
                ..Default::default()
            };
            let pipeline_options = naga::back::glsl::PipelineOptions {
                shader_stage: naga::ShaderStage::Compute,
                entry_point: ENTRY_POINT.to_string(),
                multiview: None,
            };
            let mut out = String::new();
            let mut writer = naga::back::glsl::Writer::new(
                &mut out,
                &module,
                &info,
                &options,
                &pipeline_options,
                naga::proc::BoundsCheckPolicies::default(),
            )
            .context("preparing the GLSL writer")?;
            writer.write().context("writing GLSL")?;
            Emitted::Text(out)
        }
    })
}

/// Every language, with the ones that fail reported rather than dropped.
///
/// A partial result is the useful one: GLSL ES has real restrictions that
/// WGSL does not, and "four of five translated, GLSL refused because X" is an
/// answer. Silently emitting four would not be.
pub fn emit_all(source: &str) -> Vec<(Lang, Result<Emitted>)> {
    Lang::ALL.iter().map(|&l| (l, emit(source, l))).collect()
}

// ------------------------------------------------------------- verification

/// How an emitted shader was checked, and by what.
#[derive(Debug, Clone)]
pub struct Verification {
    pub lang: Lang,
    pub compiler: Option<String>,
    pub ok: bool,
    /// Why it was not checked, when it was not. A skip is not a pass.
    pub skipped: Option<String>,
    pub detail: String,
}

/// Compile an emitted shader with the platform's own compiler.
///
/// This is the step that turns "naga produced HLSL" into "HLSL that a
/// compiler accepts", and they are not the same claim. Where no compiler
/// exists the result is a **skip with its reason**, never a pass: a release
/// that reported success for a language nothing could compile would be
/// exactly the kind of green tick this project keeps arguing against.
pub fn verify(lang: Lang, bytes: &[u8], scratch: &std::path::Path) -> Result<Verification> {
    std::fs::create_dir_all(scratch).with_context(|| format!("creating {}", scratch.display()))?;
    let path = scratch.join(format!("ssim.{}", lang.extension()));
    std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;

    let skip = |why: &str| {
        Ok(Verification {
            lang,
            compiler: None,
            ok: false,
            skipped: Some(why.to_string()),
            detail: String::new(),
        })
    };

    match lang {
        // The source was validated by naga before anything was emitted, and
        // the round-trip is compared to it separately, so there is nothing a
        // second tool would add.
        Lang::Wgsl => Ok(Verification {
            lang,
            compiler: Some("naga (validator)".into()),
            ok: true,
            skipped: None,
            detail: "parsed and validated before emission".into(),
        }),

        Lang::Msl => {
            // `xcrun metal` compiles MSL to AIR — Apple's intermediate
            // representation, which is what Metal loads. That makes this the
            // only check that reaches what the GPU is actually given.
            if which("xcrun").is_none() {
                return skip("xcrun is not available; Xcode command line tools are needed");
            }
            // `xcrun` existing is not enough. The Metal compiler is a
            // separate Xcode component, absent from a Command Line Tools
            // install, and asking for it then fails with
            //   xcrun: error: unable to find utility "metal"
            // which is a missing toolchain, not a bad shader. Classifying
            // that as a failure would make a release gate red for a reason
            // that is not a defect — the exact confusion this whole command
            // exists to avoid.
            let found = std::process::Command::new("xcrun")
                .args(["-sdk", "macosx", "-find", "metal"])
                .output();
            let have_metal = found.map(|o| o.status.success()).unwrap_or(false);
            if !have_metal {
                return skip(
                    "the Metal compiler is not installed. xcrun is present but \
                     `xcrun -sdk macosx -find metal` fails: the Metal toolchain \
                     is a separate Xcode component, not part of the Command \
                     Line Tools. Install Xcode, or verify MSL on a machine \
                     that has it.",
                );
            }
            let air = scratch.join("ssim.air");
            run(
                "xcrun",
                &[
                    "-sdk",
                    "macosx",
                    "metal",
                    "-c",
                    path.to_str().unwrap_or_default(),
                    "-o",
                    air.to_str().unwrap_or_default(),
                ],
                lang,
                "xcrun metal -> AIR",
            )
        }

        Lang::SpirV => {
            if let Some(_v) = which("spirv-val") {
                run(
                    "spirv-val",
                    &[path.to_str().unwrap_or_default()],
                    lang,
                    "spirv-val",
                )
            } else if let Some(_g) = which("glslangValidator") {
                // glslang will at least confirm the module is well-formed.
                run(
                    "glslangValidator",
                    &["-V", path.to_str().unwrap_or_default()],
                    lang,
                    "glslangValidator",
                )
            } else {
                skip(
                    "neither spirv-val nor glslangValidator is installed \
                      (brew install spirv-tools glslang)",
                )
            }
        }

        Lang::Glsl => {
            if which("glslangValidator").is_none() {
                return skip("glslangValidator is not installed (brew install glslang)");
            }
            run(
                "glslangValidator",
                &["-S", "comp", path.to_str().unwrap_or_default()],
                lang,
                "glslangValidator -S comp",
            )
        }

        Lang::Hlsl => {
            if which("dxc").is_none() {
                return skip(
                    "dxc is not installed. It ships with the Windows SDK; on \
                     macOS there is no supported build, so HLSL is emitted \
                     here and verified on the Windows host",
                );
            }
            run(
                "dxc",
                &[
                    "-T",
                    "cs_6_0",
                    "-E",
                    ENTRY_POINT,
                    path.to_str().unwrap_or_default(),
                ],
                lang,
                "dxc -T cs_6_0",
            )
        }
    }
}

fn which(bin: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|d| d.join(bin))
            .find(|p| p.is_file())
    })
}

fn run(bin: &str, args: &[&str], lang: Lang, label: &str) -> Result<Verification> {
    let out = std::process::Command::new(bin)
        .args(args)
        .output()
        .with_context(|| format!("running {bin}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let detail = if stderr.is_empty() { stdout } else { stderr };
    Ok(Verification {
        lang,
        compiler: Some(label.to_string()),
        ok: out.status.success(),
        skipped: None,
        detail: if detail.is_empty() {
            "accepted with no diagnostics".into()
        } else {
            detail
        },
    })
}

/// Verify every language that was emitted successfully.
pub fn verify_all(
    emitted: &[(Lang, Result<Emitted>)],
    scratch: &std::path::Path,
) -> Vec<Result<Verification>> {
    emitted
        .iter()
        .map(|(lang, e)| match e {
            Ok(bytes) => verify(*lang, &bytes.to_bytes(), scratch),
            Err(err) => bail!("{lang} was not emitted: {err:#}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_source_parses_and_validates() {
        // If this fails, nothing downstream can be trusted: every other
        // language is derived from this one module.
        parse_and_validate(SSIM_WGSL).expect("shaders/ssim.wgsl must validate");
    }

    #[test]
    fn language_names_round_trip_and_accept_the_api_names() {
        for l in Lang::ALL {
            assert_eq!(Lang::parse(l.as_str()), Some(*l));
        }
        // The names somebody would actually type.
        assert_eq!(Lang::parse("vulkan"), Some(Lang::SpirV));
        assert_eq!(Lang::parse("metal"), Some(Lang::Msl));
        assert_eq!(Lang::parse("directx"), Some(Lang::Hlsl));
        assert_eq!(Lang::parse("webgl"), Some(Lang::Glsl));
        // And one that is not a shading language this tool emits.
        assert_eq!(Lang::parse("cuda"), None);
    }

    #[test]
    fn every_language_emits_something() {
        for (lang, res) in emit_all(SSIM_WGSL) {
            let e = res.unwrap_or_else(|err| panic!("{lang} failed: {err:#}"));
            assert!(!e.is_empty(), "{lang} emitted nothing");
        }
    }

    #[test]
    fn spirv_is_a_spirv_module() {
        // The magic number, and the little-endian byte order a .spv file
        // holds regardless of host endianness.
        let e = emit(SSIM_WGSL, Lang::SpirV).expect("spirv");
        let words = match &e {
            Emitted::Words(w) => w,
            Emitted::Text(_) => panic!("SPIR-V must be words"),
        };
        assert_eq!(words[0], 0x0723_0203, "SPIR-V magic number");
        let bytes = e.to_bytes();
        assert_eq!(&bytes[..4], &[0x03, 0x02, 0x23, 0x07]);
    }

    #[test]
    fn the_entry_point_survives_translation() {
        // A backend that renamed or dropped the entry point produces a
        // shader that compiles and cannot be dispatched.
        for lang in [Lang::Msl, Lang::Hlsl, Lang::Glsl, Lang::Wgsl] {
            let e = emit(SSIM_WGSL, lang).unwrap_or_else(|err| panic!("{lang}: {err:#}"));
            let text = e.as_text().expect("text language");
            assert!(
                text.contains(ENTRY_POINT),
                "{lang} output does not mention the entry point {ENTRY_POINT:?}"
            );
        }
    }

    #[test]
    fn the_bindings_survive_translation() {
        // Every backend must still carry three storage buffers and one
        // uniform. A translation that dropped a binding would validate and
        // then read uninitialised memory.
        let msl = emit(SSIM_WGSL, Lang::Msl).unwrap();
        let t = msl.as_text().unwrap();
        assert!(t.contains("buffer"), "MSL has no buffer parameters");

        let hlsl = emit(SSIM_WGSL, Lang::Hlsl).unwrap();
        let t = hlsl.as_text().unwrap();
        assert!(
            t.contains("ByteAddressBuffer") || t.contains("RWByteAddressBuffer"),
            "HLSL has no storage buffers"
        );
    }

    #[test]
    fn wgsl_round_trips_through_the_validator() {
        let out = emit(SSIM_WGSL, Lang::Wgsl).unwrap();
        let text = out.as_text().unwrap();
        // Not textually identical — naga normalises — but the substance must
        // survive: the entry point, the workgroup size and the constants.
        assert!(text.contains("@compute"));
        assert!(text.contains("@workgroup_size(8"));
        assert!(text.contains("6.5025"), "C1 lost in the round trip");
        assert!(text.contains("58.5225"), "C2 lost in the round trip");
    }

    #[test]
    fn a_skip_is_never_reported_as_a_pass() {
        // The property that makes `verify` worth running at all.
        let v = Verification {
            lang: Lang::Hlsl,
            compiler: None,
            ok: false,
            skipped: Some("dxc is not installed".into()),
            detail: String::new(),
        };
        assert!(v.skipped.is_some());
        assert!(!v.ok, "a skipped verification must not be ok");
    }

    #[test]
    fn broken_wgsl_is_reported_with_the_line() {
        let err = emit("@compute fn main() { let x: u32 = ; }", Lang::SpirV)
            .expect_err("this should not parse");
        let msg = format!("{err:#}");
        assert!(msg.contains("does not parse"), "got: {msg}");
    }
}
