# Agent harnesses — one context layer, many CLIs

`lwdbg` is not written for one agent runner. The canonical context is
`skills/librewolf-debugger/SKILL.md` plus `references/*.md`, and
`lwdbg adapters` generates a native entry point for every harness found on the
machine. Nothing is duplicated: a fact corrected in a reference is corrected
everywhere at once.

```bash
lwdbg adapters list                      # every harness, installed?, wired?
lwdbg adapters install --all             # wire everything present
lwdbg adapters install --harness codex   # just one
lwdbg adapters emit --harness opencode   # print it, install nothing
lwdbg adapters uninstall --all
```

## The four shapes

| Shape | Harnesses | What is installed |
| --- | --- | --- |
| `SKILL.md` directory | Claude Code, Kilo Code, Codex | a **symlink** to the canonical directory — no copy to go stale |
| agent markdown | opencode, Kilo | one `.md` with frontmatter; reference paths rewritten absolute |
| Gemini extension | Gemini CLI, Antigravity CLI | `gemini-extension.json` + `GEMINI.md` |
| `AGENTS.md` block | Copilot CLI, anything else | a delimited block spliced in, leaving hand-written text alone |

Measured locations on this machine:

```text
~/.claude/skills/librewolf-debugger/              symlink    verified
~/.kilocode/skills/librewolf-debugger/            symlink    verified
~/.codex/skills/librewolf-debugger/               symlink    verified
~/.config/opencode/agent/librewolf-debugger.md    generated  verified
~/.config/kilo/agent/librewolf-debugger.md        generated  root-owned, see below
~/.gemini/extensions/librewolf-debugger/          generated  verified (Gemini)
~/.gemini/extensions/librewolf-debugger/          generated  INFERRED (Antigravity)
~/.copilot/AGENTS.md                              block      INFERRED
```

`lwdbg adapters list` marks an **INFERRED** path as such. Inferred means the
location is a reasonable guess from a sibling tool, not something observed. If
the harness does not pick it up, `emit` the text and place it where that
harness actually reads.

## Things that will bite

- **Gemini CLI and Antigravity CLI share `~/.gemini`**, so both resolve to the
  same extension directory. One install serves both; the second reports
  `already-generated`. The generated manifest carries a `_generated_by` marker
  so the second install recognises the first one's output instead of refusing
  to touch a "foreign" file.
- **`~/.config/kilo/agent` is owned by root** on this machine (0755 root:staff),
  so the install fails with a permission error. `sudo` needs an interactive
  password here, so this is the user's to run:

```text
  ! sudo chown -R $(id -un) ~/.config/kilo/agent
  ```

  then `lwdbg adapters install --harness kilo`.

- **An `AGENTS.md` block is in context on every turn.** It is deliberately
  under 20 lines, and that size is a tested property. Put detail in the
  references, never in the block.
- **The symlink is the point.** `--copy` exists for a harness that cannot
  follow one, and a copy goes stale the moment a reference is edited.

## Adding a harness that is not in the table

```bash
lwdbg adapters emit --harness agents --out /path/to/that/tools/AGENTS.md
```

Or read `lwdbg adapters emit --harness agents` and paste. The generated text
carries the absolute path of `SKILL.md`, so any runner that can read a file and
run a command has everything it needs.
