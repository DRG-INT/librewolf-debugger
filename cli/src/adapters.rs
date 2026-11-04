//! Uni: one canonical context layer, many agent harnesses.
//!
//! There is exactly one source of truth — `skills/librewolf-debugger/SKILL.md`
//! plus `references/*.md`. Every other harness gets a *generated* entry point
//! that points back at those absolute paths. Nothing is duplicated, so a fact
//! corrected in the canonical reference is corrected everywhere at once.
//!
//! Three shapes cover every CLI observed on this machine:
//!
//! * `Skill` — the harness already reads `SKILL.md` directories. Install is a
//!   symlink, so there is no copy to go stale. (Claude Code, Kilo Code, Codex)
//! * `Agent` — the harness reads single markdown files with frontmatter.
//!   (opencode, Kilo)
//! * `GeminiExtension` — an extension directory with a context file.
//!   (Gemini CLI, Antigravity CLI)
//! * `AgentsFile` — the cross-tool `AGENTS.md` convention, spliced into a
//!   delimited block so a hand-written file is never clobbered. This is the
//!   fallback for any CLI not named here.

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const SKILL_NAME: &str = "librewolf-debugger";
const BLOCK_BEGIN: &str = "<!-- lwdbg:begin — generated, do not edit by hand -->";
const BLOCK_END: &str = "<!-- lwdbg:end -->";

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Skill,
    Agent,
    GeminiExtension,
    AgentsFile,
}

pub struct Harness {
    pub id: &'static str,
    pub label: &'static str,
    pub kind: Kind,
    /// Where the entry point goes, relative to $HOME. Empty means the caller
    /// must supply a path with `--out`.
    pub root: &'static str,
    /// Executable whose presence on PATH proves the harness is installed.
    pub bin: Option<&'static str>,
    /// Config directory under $HOME that proves the harness is installed.
    pub config: Option<&'static str>,
    /// True when the install path was observed on this machine or is the
    /// harness's documented location. False when it is inferred from a
    /// sibling tool — the install still happens, but says so.
    pub path_verified: bool,
    pub note: &'static str,
}

/// Every harness `lwdbg` knows how to wire itself into.
///
/// `path_verified = false` entries are honest guesses, not discoveries. They
/// are kept because `adapters emit` is useful even when the install path is
/// wrong: the generated text can be pasted wherever the harness does read.
pub const HARNESSES: &[Harness] = &[
    Harness {
        id: "claude",
        label: "Claude Code",
        kind: Kind::Skill,
        root: ".claude/skills",
        bin: Some("claude"),
        config: Some(".claude"),
        path_verified: true,
        note: "reads SKILL.md directories with references/",
    },
    Harness {
        id: "kilocode",
        label: "Kilo Code",
        kind: Kind::Skill,
        root: ".kilocode/skills",
        bin: Some("kilocode"),
        config: Some(".kilocode"),
        path_verified: true,
        note: "same SKILL.md convention as Claude Code",
    },
    Harness {
        id: "codex",
        label: "OpenAI Codex",
        kind: Kind::Skill,
        root: ".codex/skills",
        bin: Some("codex"),
        config: Some(".codex"),
        path_verified: true,
        note: "same SKILL.md convention; quoted frontmatter values also parse",
    },
    Harness {
        id: "opencode",
        label: "opencode",
        kind: Kind::Agent,
        root: ".config/opencode/agent",
        bin: Some("opencode"),
        config: Some(".config/opencode"),
        path_verified: true,
        note: "agent markdown with frontmatter; legacy agents/ is also read",
    },
    Harness {
        id: "kilo",
        label: "Kilo",
        kind: Kind::Agent,
        root: ".config/kilo/agent",
        bin: Some("kilo"),
        config: Some(".config/kilo"),
        path_verified: true,
        note: "agent markdown, same frontmatter shape as opencode",
    },
    Harness {
        id: "gemini",
        label: "Gemini CLI",
        kind: Kind::GeminiExtension,
        root: ".gemini/extensions",
        bin: Some("gemini"),
        config: Some(".gemini"),
        path_verified: true,
        note: "extension directory with a context file",
    },
    Harness {
        id: "antigravity",
        label: "Antigravity CLI (agy)",
        kind: Kind::GeminiExtension,
        root: ".gemini/extensions",
        bin: Some("agy"),
        config: Some(".gemini/antigravity-cli"),
        path_verified: false,
        note: "shares ~/.gemini with Gemini CLI, so the same extension dir is \
               the best guess — unconfirmed on this build",
    },
    Harness {
        id: "copilot",
        label: "GitHub Copilot CLI",
        kind: Kind::AgentsFile,
        root: ".copilot",
        bin: Some("copilot"),
        config: Some(".copilot"),
        path_verified: false,
        note: "documented context is repo-level AGENTS.md; a global one is \
               inferred, not confirmed",
    },
    Harness {
        id: "agents",
        label: "generic AGENTS.md (any other LLM CLI)",
        kind: Kind::AgentsFile,
        root: "",
        bin: None,
        config: None,
        path_verified: true,
        note: "the cross-tool fallback; needs --out <path>",
    },
];

pub fn harness(id: &str) -> Result<&'static Harness> {
    HARNESSES.iter().find(|h| h.id == id).ok_or_else(|| {
        let ids: Vec<&str> = HARNESSES.iter().map(|h| h.id).collect();
        anyhow!("unknown harness {id:?}; known: {}", ids.join(", "))
    })
}

// ---------------------------------------------------------------- canonical

/// The one source of truth, parsed.
pub struct Canon {
    pub description: String,
    /// Everything after the frontmatter.
    pub body: String,
    /// Absolute path of the skill directory.
    pub dir: PathBuf,
    /// Absolute path of the `lwdbg` binary, when one can be found.
    pub bin: Option<PathBuf>,
}

/// Locate the canonical skill directory: `$LWDBG_SKILL`, else relative to the
/// running binary (`<repo>/target/<profile>/lwdbg` -> `<repo>/skills/<name>`),
/// else the current directory.
pub fn find_skill_dir() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("LWDBG_SKILL") {
        let p = PathBuf::from(p);
        if p.join("SKILL.md").is_file() {
            return Ok(std::fs::canonicalize(p)?);
        }
        return Err(anyhow!("LWDBG_SKILL={} has no SKILL.md", p.display()));
    }
    // Baked in by the Makefile (`LWDBG_DATADIR`). Empty for a plain cargo
    // build from the repo, where the candidates below find the tree instead.
    let baked = env!("LWDBG_DATADIR");
    if !baked.is_empty() {
        let p = Path::new(baked).join("skills").join(SKILL_NAME);
        if p.join("SKILL.md").is_file() {
            return Ok(std::fs::canonicalize(p)?);
        }
    }

    let mut candidates = Vec::new();
    // Used to locate this program's own data files, not to make an
    // authorization decision. Anyone who can replace the executable
    // already has everything this would protect.
    // nosemgrep: rust.lang.security.current-exe.current-exe
    if let Ok(exe) = std::env::current_exe() {
        // target/debug/lwdbg -> ../../..
        let mut d = exe.parent().map(Path::to_path_buf);
        for _ in 0..4 {
            if let Some(dir) = &d {
                candidates.push(dir.join("skills").join(SKILL_NAME));
                d = dir.parent().map(Path::to_path_buf);
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("skills").join(SKILL_NAME));
        candidates.push(cwd.clone());
    }
    for c in candidates {
        if c.join("SKILL.md").is_file() {
            return Ok(std::fs::canonicalize(c)?);
        }
    }
    Err(anyhow!(
        "cannot find the skill directory. Looked at $LWDBG_SKILL, the installed \
         {}, paths relative to the executable, and the current directory. Set \
         LWDBG_SKILL=<repo>/skills/{SKILL_NAME}, or install with `make install`.",
        if baked.is_empty() {
            "(no install prefix was baked in)".to_string()
        } else {
            format!("{baked}/skills/{SKILL_NAME}")
        }
    ))
}

pub fn load_canon(dir: &Path) -> Result<Canon> {
    let path = dir.join("SKILL.md");
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let (front, body) = split_frontmatter(&raw)
        .ok_or_else(|| anyhow!("{} has no --- frontmatter ---", path.display()))?;
    let description = front_value(front, "description")
        .ok_or_else(|| anyhow!("{} frontmatter has no description:", path.display()))?;
    Ok(Canon {
        description,
        body: body.trim().to_string(),
        dir: dir.to_path_buf(),
        bin: find_lwdbg_bin(dir),
    })
}

/// The binary a generated entry point should name. `lwdbg` is not installed on
/// PATH by this tool, so an absolute path is the honest thing to print.
fn find_lwdbg_bin(skill_dir: &Path) -> Option<PathBuf> {
    // Used to locate this program's own data files, not to make an
    // authorization decision. Anyone who can replace the executable
    // already has everything this would protect.
    // nosemgrep: rust.lang.security.current-exe.current-exe
    if let Ok(exe) = std::env::current_exe() {
        if exe.is_file() {
            return std::fs::canonicalize(exe).ok();
        }
    }
    let repo = skill_dir.parent()?.parent()?;
    for p in ["target/release/lwdbg", "target/debug/lwdbg"] {
        let c = repo.join(p);
        if c.is_file() {
            return std::fs::canonicalize(c).ok();
        }
    }
    None
}

fn split_frontmatter(raw: &str) -> Option<(&str, &str)> {
    let rest = raw.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let front = &rest[..end];
    let after = &rest[end + 4..];
    Some((front, after.strip_prefix('\n').unwrap_or(after)))
}

/// Read one `key: value` out of frontmatter. Values may wrap onto indented
/// continuation lines, which is how a long description stays readable.
fn front_value(front: &str, key: &str) -> Option<String> {
    let mut out: Option<String> = None;
    for line in front.lines() {
        if let Some(rest) = line.strip_prefix(&format!("{key}:")) {
            out = Some(rest.trim().to_string());
            continue;
        }
        if let Some(v) = out.as_mut() {
            let is_continuation = line.starts_with(' ') || line.starts_with('\t');
            if !is_continuation {
                break;
            }
            v.push(' ');
            v.push_str(line.trim());
        }
    }
    out.map(|v| v.trim_matches('"').trim().to_string())
}

// ------------------------------------------------------------------- emit

/// One file a harness install writes.
#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct Emitted {
    /// Path relative to the harness target. Empty means the target itself.
    pub rel: String,
    pub contents: String,
}

/// Rewrite `references/x.md` into absolute paths. A generated entry point can
/// live anywhere, so a relative pointer would be a dead link.
fn absolutize(body: &str, dir: &Path) -> String {
    body.replace("references/", &format!("{}/references/", dir.display()))
}

/// How a generated entry point should name the binary.
///
/// A path that is reachable as a bare `lwdbg` is written that way, because a
/// hard-coded absolute path survives badly across machines. When it is not on
/// PATH the absolute path is the only thing that works, and the file says so
/// rather than printing a command the harness cannot run.
fn bin_reference(canon: &Canon) -> (String, String) {
    match &canon.bin {
        None => (
            "lwdbg".into(),
            "not built yet — run `cargo build --release`".into(),
        ),
        Some(p) => {
            let on_path = p
                .parent()
                .map(|d| {
                    std::env::var_os("PATH")
                        .map(|v| std::env::split_paths(&v).any(|e| e == d))
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            if on_path {
                ("lwdbg".into(), format!("on PATH at {}", p.display()))
            } else {
                (
                    p.display().to_string(),
                    "not on PATH — call it by this absolute path, or symlink it into a \
                     directory that is"
                        .into(),
                )
            }
        }
    }
}

fn provenance(canon: &Canon) -> String {
    let (bin, where_) = bin_reference(canon);
    format!(
        "<!-- Generated by `lwdbg adapters install`. Do not edit here: edit\n\
         {canon}/SKILL.md and regenerate, or the harnesses drift apart. -->\n\n\
         **Binary:** `{bin}` ({where_}). **References:** `{canon}/references/*.md`.\n",
        canon = canon.dir.display(),
    )
}

/// The always-loaded form: short on purpose. An `AGENTS.md` block is in
/// context for every single turn, so it carries the trigger and the pointer,
/// not the manual.
fn agents_block(canon: &Canon) -> String {
    let (bin, _) = bin_reference(canon);
    format!(
        "{BLOCK_BEGIN}\n\
         ## librewolf-debugger (`lwdbg`)\n\n\
         {desc}\n\n\
         - Binary: `{bin}`\n\
         - Full instructions: `{dir}/SKILL.md` — read it before using the tool.\n\
         - Start with `{bin} doctor --json` then `{bin} detect --json`.\n\
         - Measure with three runs and report the median; one run proves nothing.\n\
         - The proxy sees all traffic of any browser pointed at it. Scope it:\n  \
           `{bin} proxy start --only-host <host>`. Never proxy Tor Browser.\n\
         {BLOCK_END}",
        desc = canon.description,
        bin = bin,
        dir = canon.dir.display(),
    )
}

pub fn emit(h: &Harness, canon: &Canon) -> Result<Vec<Emitted>> {
    let body = absolutize(&canon.body, &canon.dir);
    Ok(match h.kind {
        // Nothing is generated: the harness reads the canonical directory
        // itself, so the install is a symlink.
        Kind::Skill => vec![],

        Kind::Agent => vec![Emitted {
            rel: String::new(),
            contents: format!(
                "---\n\
                 description: {desc}\n\
                 mode: all\n\
                 steps: 40\n\
                 color: \"#F2994A\"\n\
                 permission:\n\
                 \x20 bash: ask\n\
                 \x20 edit:\n\
                 \x20   \"*\": ask\n\
                 ---\n\
                 {prov}\n\
                 {body}\n",
                desc = canon.description,
                prov = provenance(canon),
                body = body,
            ),
        }],

        Kind::GeminiExtension => vec![
            Emitted {
                rel: "gemini-extension.json".into(),
                contents: format!(
                    "{}\n",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "name": SKILL_NAME,
                        "version": env!("CARGO_PKG_VERSION"),
                        "description": canon.description,
                        "contextFileName": "GEMINI.md",
                        // Gemini CLI and Antigravity share ~/.gemini, so both
                        // resolve to this directory. The marker is what lets
                        // the second install recognise the first one's work
                        // instead of refusing to touch a "foreign" file.
                        "_generated_by": "lwdbg adapters install",
                    }))?
                ),
            },
            Emitted {
                rel: "GEMINI.md".into(),
                contents: format!(
                    "# {SKILL_NAME}\n\n{prov}\n{body}\n",
                    prov = provenance(canon),
                    body = body
                ),
            },
        ],

        Kind::AgentsFile => vec![Emitted {
            rel: String::new(),
            contents: agents_block(canon),
        }],
    })
}

// ---------------------------------------------------------------- install

#[derive(Serialize, Debug)]
pub struct Status {
    pub id: String,
    pub label: String,
    pub kind: Kind,
    /// The harness itself appears to be installed on this machine.
    pub harness_present: bool,
    /// How presence was established.
    pub evidence: String,
    pub target: String,
    /// absent | linked | linked_elsewhere | generated | present_foreign | needs_out
    pub state: String,
    pub path_verified: bool,
    pub note: String,
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}

pub fn target_path(h: &Harness, home: &Path, out: Option<&Path>) -> Result<PathBuf> {
    if let Some(o) = out {
        return Ok(o.to_path_buf());
    }
    if h.root.is_empty() {
        return Err(anyhow!(
            "{} has no fixed location — pass --out <path>",
            h.id
        ));
    }
    let root = home.join(h.root);
    Ok(match h.kind {
        Kind::Skill | Kind::GeminiExtension => root.join(SKILL_NAME),
        Kind::Agent => root.join(format!("{SKILL_NAME}.md")),
        Kind::AgentsFile => root.join("AGENTS.md"),
    })
}

pub fn status(h: &Harness, home: &Path, canon: &Canon) -> Status {
    let mut evidence = String::new();
    let mut present = false;
    if let Some(b) = h.bin {
        if let Some(p) = which(b) {
            present = true;
            evidence = p.display().to_string();
        }
    }
    if !present {
        if let Some(c) = h.config {
            let p = home.join(c);
            if p.is_dir() {
                present = true;
                evidence = format!("{} (config only)", p.display());
            }
        }
    }
    if h.bin.is_none() && h.config.is_none() {
        evidence = "n/a".into();
    }

    let target = target_path(h, home, None);
    let (target_s, state) = match &target {
        Err(_) => ("(needs --out)".to_string(), "needs_out".to_string()),
        Ok(t) => {
            let s = if let Ok(dest) = std::fs::read_link(t) {
                if dest == canon.dir {
                    "linked"
                } else {
                    "linked_elsewhere"
                }
            } else if !t.exists() {
                "absent"
            } else if h.kind == Kind::AgentsFile {
                match std::fs::read_to_string(t) {
                    Ok(c) if c.contains(BLOCK_BEGIN) => "generated",
                    Ok(_) => "present_foreign",
                    Err(_) => "present_foreign",
                }
            } else {
                match std::fs::read_to_string(if h.kind == Kind::GeminiExtension {
                    t.join("GEMINI.md")
                } else {
                    t.clone()
                }) {
                    Ok(c) if c.contains("Generated by `lwdbg adapters install`") => "generated",
                    _ => "present_foreign",
                }
            };
            (t.display().to_string(), s.to_string())
        }
    };

    Status {
        id: h.id.into(),
        label: h.label.into(),
        kind: h.kind,
        harness_present: present,
        evidence,
        target: target_s,
        state,
        path_verified: h.path_verified,
        note: h.note.into(),
    }
}

/// Replace the delimited block in `existing`, or append one. Anything the user
/// wrote outside the markers survives untouched.
pub fn splice_block(existing: &str, block: &str) -> String {
    if let (Some(a), Some(b)) = (existing.find(BLOCK_BEGIN), existing.find(BLOCK_END)) {
        if a < b {
            let end = b + BLOCK_END.len();
            return format!("{}{}{}", &existing[..a], block, &existing[end..]);
        }
    }
    let mut out = existing.trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(block);
    out.push('\n');
    out
}

#[derive(Serialize, Debug)]
pub struct InstallReport {
    pub id: String,
    pub target: String,
    pub action: String,
    pub files: Vec<String>,
    pub warning: Option<String>,
}

pub fn install(
    h: &Harness,
    home: &Path,
    canon: &Canon,
    out: Option<&Path>,
    copy: bool,
    force: bool,
    dry_run: bool,
) -> Result<InstallReport> {
    let target = target_path(h, home, out)?;
    let mut files = Vec::new();
    let warning = (!h.path_verified).then(|| {
        format!(
            "{} is an inferred path, not a confirmed one. If the harness does not \
             pick it up, run `lwdbg adapters emit --harness {}` and place the text \
             where it does read.",
            target.display(),
            h.id
        )
    });

    let action = match h.kind {
        Kind::Skill => {
            let parent = target
                .parent()
                .ok_or_else(|| anyhow!("{} has no parent", target.display()))?;
            if !dry_run {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            let existing = std::fs::read_link(&target).ok();
            match existing {
                Some(d) if d == canon.dir => "already-linked".to_string(),
                Some(_) | None if target.exists() || target.symlink_metadata().is_ok() => {
                    if !force {
                        return Err(anyhow!(
                            "{} already exists. Inspect it, then pass --force to replace it.",
                            target.display()
                        ));
                    }
                    if !dry_run {
                        if target.symlink_metadata()?.file_type().is_symlink() || target.is_file() {
                            std::fs::remove_file(&target)?;
                        } else {
                            std::fs::remove_dir_all(&target)?;
                        }
                        link_or_copy(&canon.dir, &target, copy)?;
                    }
                    files.push(target.display().to_string());
                    if copy {
                        "replaced-copy"
                    } else {
                        "replaced-symlink"
                    }
                    .to_string()
                }
                _ => {
                    if !dry_run {
                        link_or_copy(&canon.dir, &target, copy)?;
                    }
                    files.push(target.display().to_string());
                    if copy { "copy" } else { "symlink" }.to_string()
                }
            }
        }

        Kind::Agent | Kind::GeminiExtension => {
            let emitted = emit(h, canon)?;
            let base = if h.kind == Kind::GeminiExtension {
                if !dry_run {
                    std::fs::create_dir_all(&target)?;
                }
                target.clone()
            } else {
                if let Some(p) = target.parent() {
                    if !dry_run {
                        std::fs::create_dir_all(p)?;
                    }
                }
                target.clone()
            };
            let mut unchanged = 0usize;
            for e in &emitted {
                let p = if e.rel.is_empty() {
                    base.clone()
                } else {
                    base.join(&e.rel)
                };
                let current = std::fs::read_to_string(&p).ok();
                if let Some(c) = &current {
                    if c == &e.contents {
                        unchanged += 1;
                        files.push(p.display().to_string());
                        continue;
                    }
                    let ours = c.contains("Generated by `lwdbg adapters install`")
                        || c.contains("\"_generated_by\"");
                    if !ours && !force {
                        return Err(anyhow!(
                            "{} exists and was not generated by lwdbg. Inspect it, then \
                             pass --force.",
                            p.display()
                        ));
                    }
                }
                if !dry_run {
                    std::fs::write(&p, &e.contents).map_err(|err| write_hint(&p, err))?;
                }
                files.push(p.display().to_string());
            }
            if unchanged == emitted.len() {
                "already-generated".to_string()
            } else {
                "generated".to_string()
            }
        }

        Kind::AgentsFile => {
            let block = agents_block(canon);
            if let Some(p) = target.parent() {
                if !dry_run {
                    std::fs::create_dir_all(p)?;
                }
            }
            let existing = std::fs::read_to_string(&target).unwrap_or_default();
            let had = existing.contains(BLOCK_BEGIN);
            let next = splice_block(&existing, &block);
            if !dry_run {
                std::fs::write(&target, next).map_err(|e| write_hint(&target, e))?;
            }
            files.push(target.display().to_string());
            if had { "block-updated" } else { "block-added" }.to_string()
        }
    };

    Ok(InstallReport {
        id: h.id.into(),
        target: target.display().to_string(),
        action,
        files,
        warning,
    })
}

/// Turn a bare `Permission denied` into something actionable.
///
/// Some of these harness directories are owned by root (a privileged
/// installer created them), and the plain io error does not say so. `sudo`
/// needs an interactive password here, so the remedy is a command for the
/// user to run, not something to attempt.
fn write_hint(path: &Path, e: std::io::Error) -> anyhow::Error {
    if e.kind() != std::io::ErrorKind::PermissionDenied {
        return anyhow!("writing {}: {e}", path.display());
    }
    let dir = path.parent().unwrap_or(path);
    let owner = owner_of(dir).unwrap_or_else(|| "another user".into());
    anyhow!(
        "cannot write {}: the directory is owned by {owner}, not you.\n\
         Either hand it over:   ! sudo chown -R $(id -un) {dir}\n\
         or install as root:    ! sudo -E env LWDBG_SKILL=$LWDBG_SKILL lwdbg adapters \
         install --harness <id>\n\
         or skip the path:      lwdbg adapters emit --harness <id>  (then place it by hand)",
        path.display(),
        dir = dir.display(),
    )
}

#[cfg(unix)]
fn owner_of(dir: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let uid = std::fs::metadata(dir).ok()?.uid();
    if uid == 0 {
        return Some("root".into());
    }
    Some(format!("uid {uid}"))
}

#[cfg(not(unix))]
fn owner_of(_dir: &Path) -> Option<String> {
    None
}

fn link_or_copy(src: &Path, dst: &Path, copy: bool) -> Result<()> {
    if !copy {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(src, dst)
                .with_context(|| format!("symlinking {} -> {}", dst.display(), src.display()))?;
            return Ok(());
        }
        #[cfg(not(unix))]
        {
            // No symlink without privileges on Windows; copying is the
            // honest fallback and the caller is told which happened.
            return copy_tree(src, dst);
        }
    }
    copy_tree(src, dst)
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to).with_context(|| format!("copying {}", from.display()))?;
        }
    }
    Ok(())
}

pub fn uninstall(
    h: &Harness,
    home: &Path,
    out: Option<&Path>,
    dry_run: bool,
) -> Result<InstallReport> {
    let target = target_path(h, home, out)?;
    let mut files = Vec::new();

    let action = if h.kind == Kind::AgentsFile {
        let existing = std::fs::read_to_string(&target).unwrap_or_default();
        if !existing.contains(BLOCK_BEGIN) {
            "nothing-to-do".to_string()
        } else {
            let (a, b) = (
                existing.find(BLOCK_BEGIN).unwrap(),
                existing.find(BLOCK_END).unwrap() + BLOCK_END.len(),
            );
            let next = format!("{}{}", &existing[..a], &existing[b..]);
            let next = next.trim_end().to_string();
            if !dry_run {
                if next.is_empty() {
                    std::fs::remove_file(&target)?;
                } else {
                    std::fs::write(&target, format!("{next}\n"))?;
                }
            }
            files.push(target.display().to_string());
            "block-removed".to_string()
        }
    } else if target.symlink_metadata().is_err() {
        "nothing-to-do".to_string()
    } else {
        let md = target.symlink_metadata()?;
        if !dry_run {
            if md.file_type().is_symlink() || md.is_file() {
                std::fs::remove_file(&target)?;
            } else {
                std::fs::remove_dir_all(&target)?;
            }
        }
        files.push(target.display().to_string());
        "removed".to_string()
    };

    Ok(InstallReport {
        id: h.id.into(),
        target: target.display().to_string(),
        action,
        files,
        warning: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon() -> Canon {
        Canon {
            description: "Measure and debug browsers.".into(),
            body: "See `references/proxy.md` and `references/engines.md`.".into(),
            dir: PathBuf::from("/opt/lwdbg/skills/librewolf-debugger"),
            bin: Some(PathBuf::from("/opt/lwdbg/target/release/lwdbg")),
        }
    }

    #[test]
    fn harness_ids_are_unique() {
        let mut ids: Vec<&str> = HARNESSES.iter().map(|h| h.id).collect();
        ids.sort_unstable();
        let n = ids.len();
        ids.dedup();
        assert_eq!(n, ids.len(), "duplicate harness id");
    }

    #[test]
    fn references_become_absolute() {
        let c = canon();
        let out = absolutize(&c.body, &c.dir);
        assert!(out.contains("/opt/lwdbg/skills/librewolf-debugger/references/proxy.md"));
        // One pass only: the inserted path must not be rewritten again.
        assert!(!out.contains("references/librewolf-debugger/references"));
    }

    #[test]
    fn frontmatter_description_joins_wrapped_lines() {
        let raw = "---\nname: x\ndescription: first part\n  second part\nmode: all\n---\nbody\n";
        let (front, body) = split_frontmatter(raw).expect("frontmatter");
        assert_eq!(
            front_value(front, "description").as_deref(),
            Some("first part second part")
        );
        assert_eq!(body.trim(), "body");
    }

    #[test]
    fn frontmatter_absent_is_an_error_not_a_guess() {
        assert!(split_frontmatter("no frontmatter here\n").is_none());
    }

    #[test]
    fn splice_is_idempotent_and_keeps_handwritten_text() {
        let c = canon();
        let block = agents_block(&c);
        let hand = "# My project\n\nDo not break this.\n";
        let once = splice_block(hand, &block);
        let twice = splice_block(&once, &block);
        assert_eq!(once, twice, "splicing twice must not duplicate the block");
        assert!(twice.starts_with("# My project"));
        assert!(twice.contains("Do not break this."));
        assert_eq!(twice.matches(BLOCK_BEGIN).count(), 1);
    }

    #[test]
    fn agents_block_stays_small() {
        // This text is in context on every turn of every session. A long
        // block is a real cost, so the size is a tested property.
        let n = agents_block(&canon()).lines().count();
        assert!(n <= 20, "AGENTS.md block grew to {n} lines");
    }

    #[test]
    fn skill_harnesses_generate_nothing() {
        let c = canon();
        for h in HARNESSES.iter().filter(|h| h.kind == Kind::Skill) {
            assert!(
                emit(h, &c).unwrap().is_empty(),
                "{} should be a symlink, not a copy",
                h.id
            );
        }
    }

    #[test]
    fn agent_emission_carries_description_and_absolute_refs() {
        let c = canon();
        let h = harness("opencode").unwrap();
        let e = emit(h, &c).unwrap();
        assert_eq!(e.len(), 1);
        assert!(e[0]
            .contents
            .starts_with("---\ndescription: Measure and debug browsers."));
        assert!(e[0]
            .contents
            .contains("/opt/lwdbg/skills/librewolf-debugger/references/proxy.md"));
    }

    #[test]
    fn gemini_emission_has_manifest_and_context() {
        let c = canon();
        let e = emit(harness("gemini").unwrap(), &c).unwrap();
        let names: Vec<&str> = e.iter().map(|x| x.rel.as_str()).collect();
        assert_eq!(names, vec!["gemini-extension.json", "GEMINI.md"]);
        let m: serde_json::Value = serde_json::from_str(&e[0].contents).unwrap();
        assert_eq!(m["contextFileName"], "GEMINI.md");
    }

    #[test]
    fn targets_match_each_harness_convention() {
        let home = PathBuf::from("/home/u");
        let t = |id: &str| target_path(harness(id).unwrap(), &home, None).unwrap();
        assert_eq!(t("claude"), home.join(".claude/skills/librewolf-debugger"));
        assert_eq!(t("codex"), home.join(".codex/skills/librewolf-debugger"));
        assert_eq!(
            t("opencode"),
            home.join(".config/opencode/agent/librewolf-debugger.md")
        );
        assert_eq!(
            t("gemini"),
            home.join(".gemini/extensions/librewolf-debugger")
        );
        assert_eq!(t("copilot"), home.join(".copilot/AGENTS.md"));
        assert!(target_path(harness("agents").unwrap(), &home, None).is_err());
    }

    #[test]
    fn gemini_and_antigravity_resolve_to_the_same_directory_and_say_so() {
        let home = PathBuf::from("/home/u");
        let g = target_path(harness("gemini").unwrap(), &home, None).unwrap();
        let a = target_path(harness("antigravity").unwrap(), &home, None).unwrap();
        assert_eq!(g, a, "both read ~/.gemini, so one install serves both");
        // The manifest must carry the marker, or the second install would
        // refuse its own output as a foreign file.
        let e = emit(harness("gemini").unwrap(), &canon()).unwrap();
        let m: serde_json::Value = serde_json::from_str(&e[0].contents).unwrap();
        assert_eq!(m["_generated_by"], "lwdbg adapters install");
    }

    #[test]
    fn unverified_paths_are_flagged_so_the_report_is_honest() {
        assert!(!harness("antigravity").unwrap().path_verified);
        assert!(!harness("copilot").unwrap().path_verified);
        assert!(harness("claude").unwrap().path_verified);
    }

    #[test]
    fn uninstall_removing_the_only_block_removes_the_file_content() {
        let c = canon();
        let only = splice_block("", &agents_block(&c));
        let a = only.find(BLOCK_BEGIN).unwrap();
        let b = only.find(BLOCK_END).unwrap() + BLOCK_END.len();
        assert_eq!(format!("{}{}", &only[..a], &only[b..]).trim(), "");
    }
}
