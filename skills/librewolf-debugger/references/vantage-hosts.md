# Vantage points

```bash
lwdbg vantage https://example.com/ --host local --host void --host winlab --runs 3
```

## What this layer is for

One machine cannot tell a slow server from a slow route. This origin was
measured swinging from **0.22 s TTFB** to a **180 s timeout** — a 360x spread —
and no amount of re-measuring from one place resolves which end owns it.

Two or more vantage points do:

- slow windows **coinciding** across hosts on different networks → the origin;
- slow windows **not coinciding** → the path.

`lwdbg vantage` prints that reading explicitly, and refuses to draw the
conclusion from fewer than two usable hosts. Measured from `local` alone it
says so: *"1 of 1 hosts produced a median. Two or more are needed before
anything can be said about origin versus route."*

## Nothing is installed on the remote host

The remote measurement is `curl` with an explicit `--write-out` format, which
exists on every host that matters — the Void box, and Git-Bash or WSL on the
Windows one. `uname -sr` is captured alongside so the result says what it ran
on.

The same browser user-agent is sent as the local measurement uses. Without
that, a server that rate-limits curl's default UA makes every remote host look
slow for a reason that has nothing to do with the network.

## Hosts

`~/.ssh/config` defines the aliases; the tool never carries an address or a
credential:

```text
Host void      HostName 192.168.1.135   User voidunix   Port 22
Host winlab    HostName 192.168.1.134   User Labor_pc   Port 2222
```

Key authentication is installed once with `scripts/setup-vantage-ssh.zsh`, which
reads the password from the environment and stores it nowhere. After that every
call uses `BatchMode=yes`, so a host that is off or has lost its key fails fast
instead of hanging on a prompt.

Both hosts were powered off when this was first run, and the report said
`unreachable: ... Operation timed out` for each rather than silently producing a
one-host answer.

## A URL with shell metacharacters is refused

The URL is interpolated into a command that runs through a remote shell. A
quote, backtick, `$` or newline in it is refused outright rather than escaped,
because escaping for an unknown remote shell is a guess and the failure mode is
command execution on another machine.

## `local` is measured the same way

`--host local` runs the identical script through `sh` instead of `ssh`, so the
local number is produced by the same code path as the remote ones and is
directly comparable. A separate local implementation would be a second thing
to keep in agreement.

## Reading it against the other layers

| Layer | Answers |
| --- | --- |
| `lwdbg measure --via curl` | what one machine sees, in detail, with the body |
| `lwdbg measure --via marionette` | what a real browser sees, including parse and render |
| `lwdbg vantage` | whether the other two are seeing the origin or the route |

All three default to three runs and a median, for the same reason: a single
observation is not a property of a server.
