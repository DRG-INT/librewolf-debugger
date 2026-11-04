# Collecting everything

## Gecko-side

| Signal | How |
| --- | --- |
| HTTP, socket, DNS, cache trace | `MOZ_LOG='timestamp,sync,nsHttp:5,nsSocketTransport:5,nsHostResolver:5,cache2:5,DNS:5'` + `MOZ_LOG_FILE=<path>-%P.log` |
| TLS handshake | add `pipnss:5` |
| HTTP/2 frames | add `Http2Session:5` |
| Startup profile | `MOZ_PROFILER_STARTUP=1 MOZ_PROFILER_STARTUP_INTERVAL=1` |
| TLS keys for Wireshark | `SSLKEYLOGFILE=<path>` |
| Navigation + resource timing | through a transport, `performance.getEntriesByType(...)` |
| Network panel log | `devtools.netmonitor.persistlog=true`, or export HAR |
| Runtime state | `about:networking`, `about:cache`, `about:support` |

`MOZ_LOG` levels run 1 (error) to 5 (verbose). `timestamp` and `sync` are flags,
not levels: `sync` costs throughput but does not lose lines on a crash — worth
it when diagnosing a hang, not for routine timing runs, because the slowdown
distorts the thing being measured.

**`lwdbg logs` parses these into the SSoT** and joins them to timing records by
URL and timestamp. That join is the point: a `MOZ_LOG` line on its own rarely
answers anything, but the same request seen from the proxy, the log and the
Resource Timing API at once usually does.

## OS-side

```bash
sudo tcpdump -i any -n -s 0 -w <out>.pcap 'host <ip>'   # needs sudo
openssl s_client -connect <host>:443 -servername <host> -brief </dev/null
```

## Baseline — always run one

```bash
curl -sS -o /dev/null --compressed \
  -w 'dns=%{time_namelookup}s tcp=%{time_connect}s tls=%{time_appconnect}s TTFB=%{time_starttransfer}s total=%{time_total}s rate=%{speed_download}B/s\n' \
  <url>
```

`lwdbg measure --baseline curl` does this alongside the browser run. If the two
disagree by more than ~20%, the browser instrumentation is wrong — do not
interpret the numbers until that is resolved.

## Interpreting variance

Origins vary enormously. A single observed figure is not a property of the
server. Three runs, median, and report the spread alongside it — a median of
4.7s with a 3.2–9.3s range is a different finding from a tight 4.7s, and
collapsing them to one number destroys the distinction.
