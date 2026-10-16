# Architecture

Single-file Rust proxy (~570 lines in `src/main.rs`), born at maximum
optimization. No axum, no tracing, no uuid, no chrono, no anyhow, no clap —
and no TLS stack at all, because it never makes an upstream HTTP call.

## Design principles

1. **Wrapper, not client** — the upstream is a subprocess. The proxy's entire
   contract with Google is "spawn `agy`, read NDJSON from stdout". Auth, wire
   format and product gates live in the CLI, which owns them.
2. **hyper direct** — the HTTP server is `hyper::server::conn::http1` with
   `TokioIo`, no framework layer.
3. **tokio `current_thread`** — loopback single-user proxy; the multi-thread
   scheduler's synchronization overhead is pure waste here. Corollary: any
   hang blocks *every* subsequent request, so hangs are impossible by
   construction (see [Timeout and kill policy](#timeout-and-kill-policy)).
4. **`eprintln!`** — direct stderr logging, no logger framework.
5. **Fail-closed everywhere** — quota is terminal, unknown models 404,
   no silent fallback, ever.

## Request flow

```
Claude Code (Anthropic Messages API)
    │
    │ POST /v1/messages  {model: "claude-gemini-3.8-flash", ...}
    ▼
route() ── constant-time Bearer check ──► 401 if no/wrong token
    │
    ├── HEAD /            → 200
    ├── GET /health       → 200 "ok"
    ├── GET /v1/models     → static catalog (2 models)
    ├── POST /count_tokens → estimate_tokens
    └── POST /v1/messages
        │
        ▼
    resolve_model()   claude-gemini-3.8-flash[:effort] → "Gemini 3.8 Flash (High)"
        │            unknown id → 404 (fail-closed)
        ▼
    extract_prompt()  last role:user message; text blocks joined,
        │            tool_result → "[tool_result: …]", image → "[image]"
        │            empty → 400
        ▼
    truncation        >120,000 bytes → truncate + "[TRUNCATED: …]" marker
        │            (Linux ARG_MAX caps a single argv entry at ~128KB)
        ▼
    invoke_agy()     tokio::process::Command:
        │              agy -p <prompt> --output-format stream-json
        │                 --model "Gemini 3.8 Flash (High)"
        │                 [--effort low|medium|high|xhigh|max]
        │                 --dangerously-skip-permissions
        │                 --print-timeout 280s
        │              cwd=/tmp, stderr=null, stdout=piped
        ▼
    agy CLI          reuses its OAuth session from ~/.gemini/
        ▼
    daily-cloudcode-pa.googleapis.com   (proto-over-JSON + project gates)
        ▼
    Gemini 3.8 responds
        ▼
    stream-json read loop (whole loop under a 300s timeout)
        │            event:"result" → AgyResult; other lines skipped
        ▼
    handle_messages()  Anthropic Messages JSON: content, stop_reason,
        │              usage{input_tokens, output_tokens}
        │              status:"ERROR" + quota → 429 + retry-after: 3600
        │              status:"ERROR" other  → 502
        ▼
Claude Code receives Anthropic-format JSON (non-streaming)
```

## The agy stream-json protocol

`agy --output-format stream-json` writes NDJSON to stdout — one JSON object
per line, tagged with an `event` field. The proxy cares about exactly one
of them:

```jsonc
{"event": "init", ...}               // session bootstrap — ignored
{"event": "step_update", ...}        // agent-loop progress — ignored
{"event": "result", "result": {
    "response": "…final answer…",
    "status": "SUCCESS" | "ERROR",
    "error": "…",                     // present when status == "ERROR"
    "usage": {
        "input_tokens": 1234,
        "output_tokens": 567,
        "thinking_tokens": 89
    }
}}
```

The read loop is line-oriented (`BufReader::lines`), skips non-JSON lines
(`let … else { continue }`), and overwrites the accumulator on every
`result` event so the **last** one wins. Everything captured lands in:

```rust
struct AgyResult {
    response: String,        // final answer text → content[0].text
    status: String,          // "SUCCESS" | "ERROR"
    error: Option<String>,   // upstream error message
    input_tokens: u64,       // → usage.input_tokens
    output_tokens: u64,      // → usage.output_tokens
    thinking_tokens: u64,     // captured; not surfaced in the response
}
```

`--dangerously-skip-permissions` is what makes non-interactive print mode
work: the subprocess runs with no TTY (`stdout` piped, `stderr` null,
`cwd=/tmp`), so it must never block waiting for a permission prompt that
nobody can answer.

## Timeout and kill policy

The CLI runs agent loops that can legitimately take minutes. But on a
`current_thread` runtime, one hung request blocks every subsequent one — so
a hang must be *impossible*, not merely unlikely. Two layers, inner first:

| Layer | Value | Who enforces it | What happens on expiry |
|-------|-------|-----------------|------------------------|
| Inner | `--print-timeout 280s` | the `agy` CLI itself | CLI terminates its own print run cleanly |
| Outer | `AGY_TIMEOUT = 300s` | the proxy: `tokio::time::timeout` around the **entire** stdout read loop | `child.kill().await` — hard kill, then `502 api_error` "agy invocation timed out (300s cap)" |

The 20s headroom means the CLI's own deadline fires first in the common
case; the proxy's kill is the backstop for a CLI that is itself wedged
(network stall, stuck agent loop). The hard kill matters because a zombie
`agy` would leak file descriptors and memory on every retry.

## Why a wrapper (fleet comparison)

This proxy is one of four, and the odd one out on purpose:

| | [codex-proxy-cc](https://github.com/rianprei/codex-proxy-cc) | [claude-nim](https://github.com/rianprei/claude-nim) | [grok-proxy-cc](https://github.com/rianprei/grok-proxy-cc) | **antigravity-proxy-cc** |
|---|---|---|---|---|
| Upstream | OpenAI Codex | NVIDIA NIM | Grok (xAI) | Google Antigravity (Gemini) |
| Shape | HTTP client | HTTP client | HTTP client | **subprocess wrapper** |
| Wire format | JSON REST | JSON REST | JSON REST + CLI identity headers | proto-over-JSON behind product gates |
| Auth handled by | proxy (session token) | proxy (API key) | proxy (OIDC from `~/.grok/auth.json`, auto-reload on 401) | **the `agy` CLI** (`~/.gemini/`) |
| Streaming | yes | yes | SSE translation | no — one text round-trip |
| Tools | translated | translated | translated | flattened to text markers |

The other three speak JSON REST APIs: the proxy can hold the credentials and
build the requests itself. Antigravity's backend
(`daily-cloudcode-pa.googleapis.com`) is an internal CloudCode endpoint that
serializes protobuf as JSON (wrapper-nested `stringValue`/`structValue`
fields), behind OAuth project gates that check **exact Antigravity product
metadata** — product name, client version, user agent. Reimplementing that
client means every Google-side change breaks the proxy in opaque ways.

Wrapping the CLI trades that whole fragile surface for two costs the proxy
can absorb: one process spawn per request, and print mode's text-in/text-out
shape (no streaming, no native tool calling). Google can change wire format
and gates freely — the CLI updates, the proxy stays unchanged.

## Binary size budget

| Stage | Size |
|-------|------|
| Source | ~570 lines, 7 direct deps, no TLS stack |
| `cargo build --release` (opt=3, LTO fat, codegen-units=1, strip, panic=abort) | ~846 KB |
| `upx --best --lzma` | **~311 KB** |

No `rustls`/`ring` in the dependency tree at all — the subprocess design
doesn't just simplify the protocol, it deletes the TLS stack (the binary's
usual dominant cost) from the binary entirely. This is the smallest proxy
in the fleet.

## Test coverage

6 unit tests covering:

- Model resolution (`claude-gemini-3.8-flash` → `Gemini 3.8 Flash (High)`,
  `:effort` suffix stripping, unknown ids → `None`)
- Quota detection (`quota`, `RESOURCE_EXHAUSTED`, `rate limit`)
- Prompt extraction (last user message wins; text blocks joined)
- Prompt extraction from content blocks
- Token estimation (ASCII + CJK)
- Constant-time auth comparison
