# Eval results

## Mechanical cases

Run by `evals/run.sh`, which `make preflight` invokes. The runner reports
`PASS`, `FAIL` or `SKIP`, and a `SKIP` always carries its reason — a suite that
skips silently reports green on a broken tool.

Record the output here after a run rather than describing it; the numbers
change with the environment (whether a device is attached, whether the origin
under test is reachable), and a stale table is worse than none.

```sh
make check          # cargo test, then evals/run.sh
sh evals/run.sh     # just the evals
```

Known environment gaps on this machine, which produce SKIPs rather than
failures:

| Requirement | State | Effect |
| --- | --- | --- |
| `network` | available | — |
| `gecko-browser` | LibreWolf 153.0.1 | — |
| `two-renders` | depends on prior `--screenshot` runs | parity cases skip on a fresh store |
| `adb` / a device | absent | Android cases cannot run at all |

## Trigger cases — not yet measured

The nine prompts in `suite.json` measure a property of the skill's
**description text**: whether it fires when it should and stays quiet when it
should not. That cannot be measured from inside the session being measured —
the description has already been loaded here, so this session's behaviour says
nothing about whether it would have triggered.

Measuring them properly means nine fresh sessions, one prompt each, recording
only whether the skill was invoked. Until that is done this table holds
**predictions from reading the description**, which is not the same thing and
is labelled accordingly.

| id | expect | predicted | reasoning |
| --- | --- | --- | --- |
| should-fire-page-slow | fire | fire | "page load timing (TTFB, DOMContentLoaded…)" is explicit in the description |
| should-fire-already-open-browser | fire | fire | "controlling an already-running browser session" is named |
| should-fire-compression | fire | **uncertain** | the description does not use the words compression, gzip or encoding anywhere |
| should-fire-cross-browser | fire | fire | "cross-browser render parity" is named |
| should-fire-android | fire | fire | "Android Firefox over adb" is named |
| must-not-fire-web-search | quiet | quiet | "Not a web search tool and not a scraper" is the last sentence |
| must-not-fire-scraping | quiet | quiet | same sentence |
| must-not-fire-generic-http | quiet | quiet | nothing in the description is about writing HTTP client code |
| must-not-fire-css-question | quiet | quiet | nothing about CSS or layout debugging |

### The one gap this reading found

`should-fire-compression` is the case the description would probably miss.
Deciding whether compression actually applied is one of the few things this
tool can answer and a header read cannot — it is the question the whole project
started from — and the description does not mention it.

The fix is a few words in the description, not a new feature:

> …page load timing (TTFB, DOMContentLoaded, resource timing), **whether
> compression actually applied (wire versus decoded bytes)**, network
> capture, …

**Applied.** The description now reads "…resource timing), whether compression
actually applied (wire vs decoded bytes), network capture, …".

The previous wording is kept here so the change is measurable rather than
merely made:

> Measure, debug and control browsers through a local caching proxy and a
> correlated debug store. Use for page load timing (TTFB, DOMContentLoaded,
> resource timing), network capture, cross-browser render parity, …

When the nine prompts are run, `should-fire-compression` is the case that
should have changed. If the four quiet-cases also start firing, the addition
was too broad and should be narrowed — widening a description is the usual way
one drifts into firing on everything.

## Method, for whoever runs these

1. Start a fresh session with the skill installed and nothing else loaded.
2. Send exactly one prompt from `suite.json`.
3. Record only whether the skill was invoked — not whether the answer was
   good. Trigger accuracy and answer quality are different measurements and
   mixing them makes both unreadable.
4. Five fire-cases and four quiet-cases. A description that fires on all nine
   is not better than one that fires on five; over-triggering turns every
   factual question into a browser launch.
