//! antigravity-proxy-cc — Anthropic Messages API proxy for Google Antigravity.
//!
//! Wrapper proxy: invokes the `agy` CLI (Antigravity) as a subprocess per
//! request, translating Anthropic Messages → agy print mode → Anthropic SSE.
//!
//! Single-file, hyper direct, tokio current_thread. Loopback-only,
//! fail-closed. No HTTP upstream — the CLI handles auth internally.

use std::sync::Arc;

use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};

type BoxBody = UnsyncBoxBody<Bytes, std::io::Error>;

fn full_body(data: impl Into<Bytes>) -> BoxBody {
    use http_body_util::BodyExt as _;
    UnsyncBoxBody::new(Full::new(data.into()).map_err(|e| match e {}))
}

// ─── Constants ──────────────────────────────────────────────────────────────

const AGY_BIN: &str = "/home/rianprei/.local/bin/agy";
const BODY_CAP: usize = 32 * 1024 * 1024;
/// Hard cap on a single agy invocation. The CLI runs agent loops that can
/// legitimately take minutes, but a hung subprocess must never wedge the
/// proxy (current_thread runtime: one hang blocks every subsequent request).
const AGY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Static model table: (public id suffix, agy --model arg, display)
/// Verified against strings from agy.real binary 2026-10-16.
/// (High)/(Medium)/(Low) suffixes are reasoning EFFORTS (--effort flag),
/// not separate models — the proxy maps output_config.effort to --effort.
const MODELS: &[(&str, &str, &str)] = &[
    ("gemini-4-argon", "Gemini 4 Argon (High)", "Gemini 4 Argon"),
    ("gemini-3.8-flash", "Gemini 3.8 Flash (High)", "Gemini 3.8 Flash"),
    ("gemini-3.7-flash", "Gemini 3.7 Flash (High)", "Gemini 3.7 Flash"),
    ("gemini-3.6-flash", "Gemini 3.6 Flash (High)", "Gemini 3.6 Flash"),
    ("gemini-3.5-flash", "Gemini 3.5 Flash (High)", "Gemini 3.5 Flash"),
    ("gemini-3.1-pro", "Gemini 3.1 Pro (High)", "Gemini 3.1 Pro"),
    ("gemini-3.1-flash", "Gemini 3.1 Flash", "Gemini 3.1 Flash"),
    ("gemini-3-pro", "Gemini 3 Pro", "Gemini 3 Pro"),
];

// ─── Config ─────────────────────────────────────────────────────────────────

struct Cfg {
    master_key: String,
    addr: std::net::SocketAddr,
}

// ─── Model resolution ───────────────────────────────────────────────────────

fn resolve_model(model_id: &str) -> Option<&'static str> {
    let slug = model_id.strip_prefix("claude-").unwrap_or(model_id);
    // Strip :effort suffix
    let base = slug.split(':').next().unwrap_or(slug);
    MODELS
        .iter()
        .find(|(s, _, _)| *s == base)
        .map(|(_, m, _)| *m)
}

// ─── Quota detection ────────────────────────────────────────────────────────

fn is_quota_error(error_msg: &str) -> bool {
    error_msg.contains("quota")
        || error_msg.contains("RESOURCE_EXHAUSTED")
        || error_msg.contains("rate limit")
}

// ─── Token estimation ───────────────────────────────────────────────────────

fn estimate_tokens(s: &str) -> u64 {
    let ascii = s.chars().filter(|c| c.is_ascii()).count() as u64;
    let non_ascii = (s.chars().count() as u64).saturating_sub(ascii);
    (ascii / 4 + non_ascii / 2).max(1)
}

// ─── Constant-time auth check ───────────────────────────────────────────────

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ─── parse_addr: loopback-only ─────────────────────────────────────────────

fn parse_addr() -> std::net::SocketAddr {
    let raw = std::env::var("ANTIGRAVITY_PROXY_ADDR").unwrap_or_else(|_| "127.0.0.1:4113".into());
    raw.parse()
        .unwrap_or_else(|_| "127.0.0.1:4113".parse().unwrap())
}

// ─── Extract prompt from Anthropic Messages ────────────────────────────────

fn extract_prompt(body: &Value) -> String {
    if let Some(msgs) = body.get("messages").and_then(Value::as_array) {
        // Find the last user message
        for m in msgs.iter().rev() {
            if m.get("role").and_then(Value::as_str) == Some("user") {
                match m.get("content") {
                    Some(Value::String(s)) => return s.clone(),
                    Some(Value::Array(blocks)) => {
                        let text: Vec<String> = blocks
                            .iter()
                            .filter_map(|b| match b.get("type").and_then(Value::as_str)? {
                                "text" => b.get("text").and_then(Value::as_str).map(String::from),
                                "tool_result" => {
                                    let inner =
                                        b.get("content").and_then(Value::as_str).unwrap_or("");
                                    Some(format!("[tool_result: {inner}]"))
                                }
                                "image" => Some("[image]".into()),
                                _ => None,
                            })
                            .collect();
                        return text.join("\n");
                    }
                    _ => return String::new(),
                }
            }
        }
    }
    String::new()
}

// ─── Invoke agy CLI ─────────────────────────────────────────────────────────

struct AgyResult {
    response: String,
    status: String,
    error: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    thinking_tokens: u64,
}

async fn invoke_agy(prompt: &str, model: &str, effort: Option<&str>) -> Result<AgyResult, String> {
    let mut cmd = tokio::process::Command::new(AGY_BIN);
    cmd.arg("-p")
        .arg(prompt)
        .arg("--output-format")
        .arg("stream-json")
        .arg("--model")
        .arg(model)
        .arg("--dangerously-skip-permissions")
        .arg("--print-timeout")
        .arg("280s") // leave 20s headroom under our own 300s cap
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .current_dir("/tmp");

    if let Some(e) = effort {
        cmd.arg("--effort").arg(e);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn agy: {e}"))?;

    let stdout = child.stdout.take().ok_or("no stdout")?;

    // Wrap the ENTIRE read loop in a timeout: a hung agy (network stall,
    // agent loop stuck) must never wedge the proxy. current_thread runtime
    // means one hang would block every subsequent request.
    let read_result = tokio::time::timeout(AGY_TIMEOUT, async {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();

        let mut result = AgyResult {
            response: String::new(),
            status: String::new(),
            error: None,
            input_tokens: 0,
            output_tokens: 0,
            thinking_tokens: 0,
        };

        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if v.get("event").and_then(Value::as_str) == Some("result") {
                if let Some(r) = v.get("result") {
                    result.response = r
                        .get("response")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    result.status = r
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    result.error = r.get("error").and_then(Value::as_str).map(String::from);
                    if let Some(u) = r.get("usage") {
                        result.input_tokens =
                            u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                        result.output_tokens =
                            u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
                        result.thinking_tokens = u
                            .get("thinking_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                    }
                }
            }
        }
        result
    })
    .await;

    match read_result {
        Ok(result) => {
            let _ = child.wait().await;
            Ok(result)
        }
        Err(_elapsed) => {
            // Timeout: kill the subprocess hard. A zombie agy process
            // would leak file descriptors and memory on every retry.
            let _ = child.kill().await;
            Err("agy invocation timed out (300s cap)".to_string())
        }
    }
}

// ─── Main handler: POST /v1/messages ───────────────────────────────────────

async fn handle_messages(_cfg: Arc<Cfg>, body: Value) -> Response<BoxBody> {
    let model_id = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("claude-gemini-3.8-flash")
        .to_string();

    let Some(agy_model) = resolve_model(&model_id) else {
        let err = json!({"type":"error","error":{"type":"not_found_error","message":format!("unsupported model id: {model_id}")}});
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header("content-type", "application/json")
            .body(full_body(err.to_string()))
            .unwrap();
    };

    let prompt = extract_prompt(&body);
    if prompt.is_empty() {
        let err = json!({"type":"error","error":{"type":"invalid_request_error","message":"no user message found"}});
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header("content-type", "application/json")
            .body(full_body(err.to_string()))
            .unwrap();
    }

    // Linux ARG_MAX: 128KB per argument. Truncate with a loud marker rather
    // than fail with an obscure OS error on very long prompts.
    const MAX_PROMPT: usize = 120_000;
    let prompt = if prompt.len() > MAX_PROMPT {
        eprintln!(
            "[antigravity-proxy] prompt truncated: {} bytes → {}",
            prompt.len(),
            MAX_PROMPT
        );
        let mut s = prompt;
        s.truncate(MAX_PROMPT);
        s.push_str("\n\n[TRUNCATED: prompt exceeded 120KB ARG_MAX limit]");
        s
    } else {
        prompt
    };

    // Effort from output_config.effort (native Claude Code slider)
    let effort = body
        .pointer("/output_config/effort")
        .and_then(Value::as_str)
        .filter(|e| matches!(*e, "low" | "medium" | "high" | "xhigh" | "max"));

    let result = match invoke_agy(&prompt, agy_model, effort).await {
        Ok(r) => r,
        Err(e) => {
            let err = json!({"type":"error","error":{"type":"api_error","message":e}});
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .header("content-type", "application/json")
                .body(full_body(err.to_string()))
                .unwrap();
        }
    };

    // Fail-closed on quota
    if result.status == "ERROR" {
        let error_msg = result.error.unwrap_or_default();
        if is_quota_error(&error_msg) {
            let err =
                json!({"type":"error","error":{"type":"rate_limit_error","message":error_msg}});
            return Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .header("content-type", "application/json")
                .header("retry-after", "3600")
                .body(full_body(err.to_string()))
                .unwrap();
        }
        let err = json!({"type":"error","error":{"type":"api_error","message":error_msg}});
        return Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .header("content-type", "application/json")
            .body(full_body(err.to_string()))
            .unwrap();
    }

    // Success: build Anthropic Messages response
    let response_json = json!({
        "id": format!("msg_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos()),
        "type": "message",
        "role": "assistant",
        "model": model_id,
        "content": [{"type":"text","text":result.response}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {
            "input_tokens": result.input_tokens,
            "output_tokens": result.output_tokens,
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(full_body(response_json.to_string()))
        .unwrap()
}

// ─── Route handler ───────────────────────────────────────────────────────────

async fn route(cfg: Arc<Cfg>, req: Request<Incoming>) -> Response<BoxBody> {
    let method = req.method().as_str();
    let path = req.uri().path().to_string();

    if method == "HEAD" {
        return Response::builder()
            .status(StatusCode::OK)
            .body(full_body(Bytes::new()))
            .unwrap();
    }

    if method == "GET" && path == "/health" {
        return Response::builder()
            .status(StatusCode::OK)
            .body(full_body("ok"))
            .unwrap();
    }

    if !cfg.master_key.is_empty() {
        let auth = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let provided = auth.strip_prefix("Bearer ").unwrap_or("");
        if !constant_time_eq(provided.as_bytes(), cfg.master_key.as_bytes()) {
            let err = json!({"type":"error","error":{"type":"authentication_error","message":"invalid bearer token"}});
            return Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .header("content-type", "application/json")
                .body(full_body(err.to_string()))
                .unwrap();
        }
    }

    if method == "GET" && path == "/v1/models" {
        let models: Vec<Value> = MODELS
            .iter()
            .map(|(slug, _, display)| {
                json!({"id":format!("claude-{slug}"),"display_name":display,"type":"model","created_at":"2026-10-16T00:00:00Z"})
            })
            .collect();
        let out = json!({"data":models});
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(full_body(out.to_string()))
            .unwrap();
    }

    if method == "POST" && path == "/v1/messages/count_tokens" {
        let (_, body) = req.into_parts();
        let bytes = match body.collect().await {
            Ok(c) => c.to_bytes(),
            Err(_) => {
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(full_body("{\"error\":\"bad body\"}"))
                    .unwrap();
            }
        };
        let n = estimate_tokens(&String::from_utf8_lossy(&bytes));
        let out = json!({"input_tokens": n});
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(full_body(out.to_string()))
            .unwrap();
    }

    if method == "POST" && path == "/v1/messages" {
        let (_, body) = req.into_parts();
        let bytes = match body.collect().await {
            Ok(c) => c.to_bytes(),
            Err(_) => {
                let err = json!({"type":"error","error":{"type":"invalid_request_error","message":"bad body"}});
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .header("content-type", "application/json")
                    .body(full_body(err.to_string()))
                    .unwrap();
            }
        };
        if bytes.len() > BODY_CAP {
            let err = json!({"type":"error","error":{"type":"invalid_request_error","message":"body too large"}});
            return Response::builder()
                .status(StatusCode::PAYLOAD_TOO_LARGE)
                .header("content-type", "application/json")
                .body(full_body(err.to_string()))
                .unwrap();
        }
        let parsed: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(e) => {
                let err = json!({"type":"error","error":{"type":"invalid_request_error","message":format!("parse: {e}")}});
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .header("content-type", "application/json")
                    .body(full_body(err.to_string()))
                    .unwrap();
            }
        };
        return handle_messages(cfg, parsed).await;
    }

    let err = json!({"type":"error","error":{"type":"not_found_error","message":format!("no route: {method} {path}")}});
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header("content-type", "application/json")
        .body(full_body(err.to_string()))
        .unwrap()
}

// ─── Main ───────────────────────────────────────────────────────────────────

fn main() {
    let addr = parse_addr();
    let master_key = std::env::var("ANTIGRAVITY_MASTER_KEY").unwrap_or_default();

    if !std::path::Path::new(AGY_BIN).exists() {
        eprintln!("[antigravity-proxy] FATAL: {AGY_BIN} not found");
        std::process::exit(1);
    }

    if master_key.is_empty() {
        eprintln!("[antigravity-proxy] WARNING: ANTIGRAVITY_MASTER_KEY unset; any local process can use this proxy");
    }

    let cfg = Arc::new(Cfg { master_key, addr });

    eprintln!("[antigravity-proxy] listening on {addr} (loopback-only, fail-closed)");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(cfg.addr).await.expect("bind");
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let cfg = cfg.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req| {
                    let cfg = cfg.clone();
                    async move { Ok::<_, std::convert::Infallible>(route(cfg, req).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .keep_alive(true)
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_model() {
        assert_eq!(
            resolve_model("claude-gemini-3.8-flash"),
            Some("Gemini 3.8 Flash (High)")
        );
        assert_eq!(
            resolve_model("claude-gemini-3.8-flash:high"),
            Some("Gemini 3.8 Flash (High)")
        );
        assert_eq!(
            resolve_model("gemini-4-argon"),
            Some("Gemini 4 Argon (High)")
        );
        assert_eq!(
            resolve_model("gemini-3.1-pro"),
            Some("Gemini 3.1 Pro (High)")
        );
        assert_eq!(resolve_model("gemini-3-pro"), Some("Gemini 3 Pro"));
        assert_eq!(resolve_model("claude-sonnet-4"), None);
    }

    #[test]
    fn test_quota_detection() {
        assert!(is_quota_error("Individual quota reached"));
        assert!(is_quota_error("RESOURCE_EXHAUSTED (code 429)"));
        assert!(!is_quota_error("some other error"));
    }

    #[test]
    fn test_extract_prompt() {
        let body = json!({
            "model": "claude-gemini-3.8-flash",
            "messages": [
                {"role": "user", "content": "hello world"},
                {"role": "assistant", "content": "hi"},
                {"role": "user", "content": "what is 2+2?"}
            ]
        });
        assert_eq!(extract_prompt(&body), "what is 2+2?");
    }

    #[test]
    fn test_extract_prompt_blocks() {
        let body = json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "text", "text": "world"}
                ]}
            ]
        });
        assert_eq!(extract_prompt(&body), "hello\nworld");
    }

    #[test]
    fn test_estimate_tokens() {
        assert_eq!(estimate_tokens("hello world!"), 3);
        assert!(estimate_tokens("你好世界") > 0);
    }

    #[test]
    fn test_constant_time_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
    }
}
