# llm-loadgen

[![ci](https://github.com/eljandoubi/llm-loadgen/actions/workflows/ci.yml/badge.svg)](https://github.com/eljandoubi/llm-loadgen/actions/workflows/ci.yml)

An open-loop load generator for OpenAI-compatible streaming endpoints (vLLM, SGLang, TGI, the
llama.cpp server, a gateway in front of any of them). It reports time to first token, inter-token
latency and end-to-end latency, **measured from the moment each request was meant to start**, so a
server that stalls cannot hide the requests it delayed.

Written in Rust with no dependencies: the HTTP client, the SSE and JSON parsers, the latency
histogram and the argument parser are in this repository: about 2,500 lines of code and 2,600 lines
of tests.

## Why open loop

A closed-loop benchmark (N clients that each send a request, wait for the answer, send the next)
slows down when the server does. During a stall the clients are stuck inside their one slow request
each, and the thousands of requests that real users would have sent in that second are never made.
The tail latency it reports is far too good. Gil Tene named this *coordinated omission*.

An open-loop generator fixes the schedule in advance (Poisson arrivals by default, as independent
users produce) and keeps to it. A request that has to wait for a free connection still counts from
its intended start.

Both modes are here so that the difference can be measured. The server is the included mock, on the
same laptop (Apple M4), release build, one run each; latencies in milliseconds.

**The server freezes for one second, 3 s into a 10 s run. Requests normally take 10 ms.**

| mode | requests sent | p50 | p90 | p99 | p99.9 | max |
|:--|--:|--:|--:|--:|--:|--:|
| closed loop, 4 clients | 2,491 | 15.0 | 15.5 | **15.7** | 1,019.8 | 1,019.8 |
| open loop, 50 req/s | 500 | 17.8 | 23.2 | **921.6** | 1,017.4 | 1,017.4 |

The closed loop reports a p99 of 16 ms: only the 4 requests that were in flight met the freeze. The
open loop sent the 50 requests that were due during that second, and every one of them waited.

**The server can serve 20 requests per second (2 slots of 100 ms). 40 per second are offered for 10 s.**

| mode | throughput | p50 | p99 | max |
|:--|--:|--:|--:|--:|
| closed loop, 2 clients | 19.0 req/s | 106.0 | 108.2 | 108.2 |
| open loop, 40 req/s | 19.3 req/s, over 20.8 s | 5,472.3 | 10,747.9 | 11,945.5 |

The closed loop sees a healthy server. The open loop shows the queue that builds, and its report says
that the server needed 20.8 s to answer 10 s of load.

**What the generator itself can do.** Against a fast mock (about 1 ms to first token, 8 tokens), 800
req/s were offered and 819.6 were completed, with a start lag (how late requests began compared with
their intended time) of 0.2 ms at p50 and 1.4 ms at p99.

These are measurements of a mock server on one machine. They show what the tool reports, not how fast
any real server is.

## Use

```sh
cargo install --path .        # or: cargo build --release

llm-loadgen mock --port 8000 --ttft-ms 40 --itl-ms 5 --tokens 16 --slots 4    # a fake endpoint

llm-loadgen run --url http://127.0.0.1:8000/v1/chat/completions \
    --rate 20 --duration 3 --warmup 1 --max-tokens 16 --json report.json
```

```
llm-loadgen  open loop, poisson 20.0 req/s, 3.0 s measured after 1.0 s of warm-up, 64 connections
requests    offered 58  ok 58  failed 0  dropped 0
throughput  18.7 req/s  300 tokens/s   (target 20.0 req/s)

latency ms        p50       p90       p99     p99.9       max
ttft             48.9     124.4     155.7     155.7     155.7
itl               6.3       6.5       7.4       9.8       9.8
e2e             142.3     218.1     247.7     247.7     247.7

start lag         1.9       5.1       5.3       5.3       5.3
```

(A real run against the mock, debug build. A request takes about 115 ms there, so 4 slots serve at
most 35 req/s, and at 20 req/s the queue in front of them shows at the tail of the time to first token.)

Options (`llm-loadgen --help` lists them all): `--rate`, `--arrival poisson|constant`, `--duration`,
`--warmup`, `--connections` (most requests in flight), `--max-queue`, `--mode open|closed`,
`--prompt-words`, `--max-tokens`, `--timeout`, `--seed`, `--api-key` (or `OPENAI_API_KEY`), `--json`.
The mock also has `--stall AFTER:LEN`, `--error-status` and `--die-after` to reproduce faults.

Only `http://` URLs work: there is no TLS. For an https endpoint, run a local TLS-terminating proxy.

## What is measured

- **ttft**: intended start to the first event with text in its `delta.content`. The role chunk that
  servers send first, with empty content, does not count.
- **itl**: gap between the arrival of consecutive token events. Tokens that arrive in one read of
  the socket share a timestamp, so batching on the way shows up as zeros.
- **e2e**: intended start to the end of the stream.
- **start lag**: intended start to the moment the request actually started. If it is high, the
  generator (not the server) is the bottleneck: raise `--connections`. The report warns above 50 ms.
- **tokens**: the server's own `usage.completion_tokens` when it sends one (the request asks for it),
  otherwise the number of token events.
- **throughput** is the number of successful requests divided by the time the server took to answer
  everything that was due in the window, which is longer than the window when it is behind.
- Failed requests (error status, timeout, connection refused, connection closed mid-stream, error
  event, unreadable stream) are counted by kind and are left out of the latency percentiles; the report
  says so. Requests that find `--max-queue` full are dropped and counted, with a warning.

Latencies are kept in a log-linear histogram (values below 256 µs exact, above that never more than
0.78 percent wide), so memory is constant and a 12-second tail costs nothing.

## Design

```
src/
  arrival.rs   Poisson and constant schedules of intended start times
  rng.rs       splitmix64, checked against the reference sequence
  hist.rs      log-linear latency histogram
  sse.rs       incremental server-sent events parser
  json.rs      small JSON reader and writer (depth-limited)
  http.rs      URL, POST, response head, chunked decoder, streaming body reader
  client.rs    one request, measured from its intended start
  runner.rs    the open-loop scheduler and the closed-loop clients
  report.rs    counters, percentile table, JSON
  mock.rs      a fake OpenAI-style server with slots, a stall and faults
  cli.rs       argument parsing
```

The scheduler thread releases each request at its intended time into a bounded queue; a fixed set of
connection threads drains it. One request is one connection (`Connection: close`), so the end of a
stream is unambiguous.

## Tests

`cargo test` runs 138 tests, all against the standard library only:

- the histogram's buckets leave no gap up to `u64::MAX` and every value is within 0.78 percent of
  its bucket; quantiles are never below the truth;
- the SSE parser and the chunked decoder give the same result wherever the bytes are cut (every split
  position, and one byte at a time), including inside a CRLF and inside a multi-byte character;
- Poisson gaps have mean `1/rate` and unit coefficient of variation, and counts per second have mean
  and variance equal to the rate;
- a JSON property test writes 2,000 random values and reads them back;
- the mock and the client run over real sockets: slots make requests queue, a stall freezes streams
  in progress, an error status or a dropped connection becomes an outcome;
- the coordinated-omission scenario above runs as a test, in-process and through the real binary.

One bug came out of running the suite many times: on macOS an accepted socket inherits the listener's
non-blocking mode, so the mock rejected requests that arrived late. The test that reproduces it fails
without the fix.

## Limits

- No TLS, no HTTP/2, one connection per request, one OS thread per connection. Fine for thousands of
  requests per second on a laptop, not for hundreds of thousands.
- Chat-completions style streaming only; no `/v1/completions`, no tool calls, no images.
- Timers are `thread::sleep`: expect a start lag of a millisecond or so.
- It is a small tool made to show a measurement principle, not a replacement for mature load testers
  such as wrk2 or vegeta, or for benchmarking suites made for LLM servers.

## License

Apache-2.0, see [LICENSE](LICENSE).
