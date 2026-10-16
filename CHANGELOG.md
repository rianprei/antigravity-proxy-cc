# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] — 2026-10-16

### Added

- **Initial release**: Anthropic Messages API proxy for Google Antigravity
  (Gemini) models.
- **Wrapper proxy**: invokes the `agy` CLI as a subprocess per request
  (print mode, `--output-format stream-json`) — no HTTP upstream to
  reimplement. The CLI reuses its own OAuth session from `~/.gemini/`;
  the internal API (`daily-cloudcode-pa.googleapis.com`) speaks
  proto-over-JSON behind project gates that require exact Antigravity
  product metadata, so wrapping the CLI is the only stable surface.
- 2 models: `claude-gemini-3.8-flash` → Gemini 3.8 Flash (High),
  `claude-gemini-3.8-pro` → Gemini 3.8 Pro.
- Native effort control: Claude Code's `/effort` slider
  (`output_config.effort`) → `agy --effort` (`low|medium|high|xhigh|max`);
  `:<effort>` model-id suffixes tolerated and stripped.
- Timeout policy: 300s hard cap (`tokio::time::timeout`) around the entire
  subprocess stdout read loop, with hard `kill()` on expiry — no zombie `agy`
  can wedge the `current_thread` runtime — plus `--print-timeout 280s`
  so the CLI self-terminates first with 20s headroom.
- Prompt truncation: 120,000-byte cap (Linux `ARG_MAX`) with a loud
  `[TRUNCATED: …]` marker and stderr log, instead of an obscure OS error.
- Fail-closed quota: `quota`/`RESOURCE_EXHAUSTED`/`rate limit` errors →
  terminal `429 rate_limit_error` with `retry-after: 3600`; other upstream
  errors → `502`. Unknown model ids → `404`, empty prompt → `400`.
- Loopback-only bind (`127.0.0.1:4113` default), constant-time Bearer auth,
  32 MB body cap, boot-time check that the `agy` binary exists.
- Born at maximum optimization: hyper direct (no axum), tokio
  `current_thread`, `opt-level=3` + LTO fat + `codegen-units=1` +
  `panic=abort` + `strip`, no TLS stack in the binary (subprocess upstream),
  UPX `--best --lzma` → 311 KB binary, 7 direct dependencies (zero
  uuid/chrono/anyhow/clap/tracing/rustls), `eprintln!` logging.
- 6 unit tests, clippy clean with `-D warnings`, rustfmt clean.
