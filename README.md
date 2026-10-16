# antigravity-proxy-cc

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org)
[![Build](https://img.shields.io/badge/build-passing-brightgreen.svg)](.github/workflows/ci.yml)
[![Binário](https://img.shields.io/badge/binary-311K%20UPX-blueviolet.svg)](#build)

An **Anthropic Messages-API-compatible** HTTP proxy, written in Rust, that lets
[Claude Code](https://docs.anthropic.com/en/docs/claude-code) drive **Google
Antigravity (Gemini)** models through your existing **`agy` CLI login** —
no API key, no paid tokens.

Claude Code speaks Anthropic `/v1/messages`; the proxy translates each request
into one `agy` print-mode invocation, and the CLI handles everything else —
OAuth session, wire protocol, product gates — reusing the credentials it already
keeps fresh in `~/.gemini/`.

```
claude --model claude-gemini-3.8-flash
  → antigravity-proxy-cc :4113 (Anthropic Messages → agy print mode)
    → agy CLI (reusa a sessão OAuth de ~/.gemini/)
      → daily-cloudcode-pa.googleapis.com (proto-over-JSON + product gates)
        → Gemini 3.8 responde
```

## Features

- **Zero API key** — the `agy` CLI reuses its own OAuth session from `~/.gemini/`
- **2 models**: `claude-gemini-3.8-flash`, `claude-gemini-3.8-pro`
- **Wrapper proxy** — no HTTP upstream to reimplement: the CLI *is* the client
- **Native effort control**: Claude Code's `/effort` slider → `--effort`
- **Timeout policy**: 300s hard cap on the subprocess (with hard kill — no
  zombie `agy` can wedge the proxy), `--print-timeout 280s` for 20s headroom
- **Prompt truncation**: 120KB cap (Linux `ARG_MAX`) with a loud truncation
  marker, instead of an obscure OS error
- **Fail-closed**: quota exhaustion → terminal `429` with `retry-after: 3600`,
  never silently falls back
- **Loopback-only**: binds `127.0.0.1`, never exposed to the network
- **Constant-time auth**: local Bearer token checked in O(n) without timing leaks

## Why a wrapper proxy?

The other proxies in this fleet ([codex-proxy-cc](https://github.com/rianprei/codex-proxy-cc),
[claude-nim](https://github.com/rianprei/claude-nim),
[grok-proxy-cc](https://github.com/rianprei/grok-proxy-cc)) are HTTP clients:
they call a JSON REST upstream themselves. Antigravity doesn't have one to call.

Google Antigravity's backend (`daily-cloudcode-pa.googleapis.com`) is an
internal CloudCode endpoint that speaks **proto-over-JSON** — protobuf messages
serialized into JSON with wrapper nesting — behind OAuth **project gates** that
check the **exact product metadata** of the Antigravity client: product name,
client version, user agent. A hand-rolled HTTP client would have to replicate
the proto encoding, the session refresh flow, *and* the metadata gates; when any
of those drifts, every request dies with an opaque permission error.

The `agy` CLI already implements all of it and keeps its OAuth session fresh in
`~/.gemini/`. Wrapping it shrinks the proxy's entire upstream surface to
"spawn a process, read NDJSON from stdout": Google can change wire format and
gates freely — the CLI updates, the proxy stays unchanged.

The tradeoffs, accepted knowingly:

- **One subprocess per request** (print mode: text in, text out — no native
  tool calling, no token streaming; Claude Code runs in text mode)
- **Prompt bounded by `ARG_MAX`** — see [Prompt truncation](#prompt-truncation)

## Born at maximum optimization

This proxy was designed from day one to need zero rework:

| Property | Value |
|----------|-------|
| HTTP framework | **hyper direct** (no axum layer) |
| Runtime | **tokio `current_thread`** (single-user loopback) |
| Upstream | **subprocess** (`agy` CLI) — no TLS stack in the binary at all |
| Profile | `opt-level=3`, `lto="fat"`, `codegen-units=1`, `panic="abort"`, `strip=true` |
| Binary | **311 KB** with `upx --best --lzma` |
| Dependencies | **7** — zero uuid, chrono, anyhow, clap, tracing, rustls |
| Logging | `eprintln!` direct (no logger framework) |

## Prerequisites

- Rust toolchain (`cargo`)
- The Antigravity CLI, logged in: run `agy` once and complete the OAuth flow.
  The proxy expects the binary at `~/.local/bin/agy` and checks for it at boot
- `upx` (optional, for binary compression)

## Quick start

```sh
# 1. Clone and build
git clone https://github.com/rianprei/antigravity-proxy-cc.git
cd antigravity-proxy-cc
cargo build --release
upx --best --lzma target/release/antigravity-proxy-cc   # optional

# 2. Generate a local auth token
echo "ANTIGRAVITY_MASTER_KEY=$(cat /proc/sys/kernel/random/uuid | tr -d '-')" \
  > ~/.config/antigravity-proxy-cc/proxy.env
chmod 600 ~/.config/antigravity-proxy-cc/proxy.env

# 3. Run
ANTIGRAVITY_PROXY_ADDR=127.0.0.1:4113 \
ANTIGRAVITY_MASTER_KEY=<your-key> \
./target/release/antigravity-proxy-cc
```

Then point Claude Code at it:

```sh
ANTHROPIC_BASE_URL="http://127.0.0.1:4113" \
ANTHROPIC_AUTH_TOKEN="<your-key>" \
CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1 \
claude --model claude-gemini-3.8-flash
```

Or add to your Claude Code settings' `modelPicker`:

```json
{
  "model": "claude-gemini-3.8-flash",
  "label": "Gemini 3.8 Flash",
  "description": "Google Antigravity via agy login",
  "behavesAs": "claude-sonnet-5"
}
```

## Model ids

| Public id | agy `--model` | Description |
|-----------|---------------|-------------|
| `claude-gemini-3.8-flash` | `Gemini 3.8 Flash (High)` | Fast, high-effort default |
| `claude-gemini-3.8-pro` | `Gemini 3.8 Pro` | Frontier |

An optional `:<effort>` suffix is tolerated and stripped
(`claude-gemini-3.8-pro:high` → `Gemini 3.8 Pro`). Effective effort comes from
Claude Code's native control (`/effort` slider, `output_config.effort`) →
`agy --effort low|medium|high|xhigh|max`.

## Timeout policy

The CLI runs agent loops that can legitimately take minutes, but a hung
subprocess must never wedge the proxy — on a `current_thread` runtime, one hang
blocks every subsequent request. Two layers:

1. **`--print-timeout 280s`** — agy's own print-mode deadline. 20s of headroom
   means the CLI usually terminates itself first, cleanly.
2. **300s hard cap** — `tokio::time::timeout` wraps the *entire* stdout read
   loop. On expiry the child gets `kill()` — hard, immediate, no zombie, no
   leaked file descriptors — and the proxy answers `502` with
   `"agy invocation timed out (300s cap)"`.

## Prompt truncation

The prompt travels as a single `argv` entry, and Linux `ARG_MAX` caps a single
argument at ~128KB. Rather than failing with an obscure
`OSError: argument list too long` on huge conversations, the proxy truncates at
**120,000 bytes** and appends a loud marker:

```
[TRUNCATED: prompt exceeded 120KB ARG_MAX limit]
```

…logging the original size to stderr. Claude Code sees an explicit truncation,
not a mystery failure.

## API

| Endpoint | Description |
|----------|-------------|
| `HEAD /` | Health probe |
| `GET /health` | Returns `ok` |
| `GET /v1/models` | Anthropic-shaped model list (for gateway discovery) |
| `POST /v1/messages/count_tokens` | Token estimation |
| `POST /v1/messages` | Anthropic Messages API (non-streaming JSON) |

## Architecture

See [docs/architecture.md](docs/architecture.md).

## Related projects

- [rianprei/codex-proxy-cc](https://github.com/rianprei/codex-proxy-cc) — same concept for OpenAI Codex (ChatGPT subscription)
- [rianprei/claude-nim](https://github.com/rianprei/claude-nim) — same concept for NVIDIA NIM
- [rianprei/grok-proxy-cc](https://github.com/rianprei/grok-proxy-cc) — same concept for Grok (xAI)

## License

[MIT](LICENSE) © rianprei
