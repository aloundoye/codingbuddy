use anyhow::{Result, anyhow};
use clap::CommandFactory;
use clap_complete::generate;
use codingbuddy_jsonrpc::{JsonRpcRequest, JsonRpcResponse, RpcHandler};
use serde_json::{Value, json};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

const MAX_RPC_BODY_BYTES: u64 = 10 * 1024 * 1024;

static HEADER_JSON: LazyLock<tiny_http::Header> =
    LazyLock::new(|| "Content-Type: application/json".parse().unwrap());
static HEADER_CORS: LazyLock<tiny_http::Header> =
    LazyLock::new(|| "Access-Control-Allow-Origin: *".parse().unwrap());
static HEADER_CORS_METHODS: LazyLock<tiny_http::Header> = LazyLock::new(|| {
    "Access-Control-Allow-Methods: POST, GET, OPTIONS"
        .parse()
        .unwrap()
});
static HEADER_CORS_HEADERS: LazyLock<tiny_http::Header> = LazyLock::new(|| {
    "Access-Control-Allow-Headers: Content-Type"
        .parse()
        .unwrap()
});

use crate::{Cli, CompletionsArgs, NativeHostArgs, ServeArgs};

const MAX_NATIVE_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

pub(crate) fn run_completions(args: CompletionsArgs) -> Result<()> {
    let mut cmd = Cli::command();
    generate(args.shell, &mut cmd, "codingbuddy", &mut io::stdout());
    Ok(())
}

pub(crate) fn run_serve(args: ServeArgs, json_mode: bool) -> Result<()> {
    match args.transport.as_str() {
        "stdio" => {
            if json_mode {
                println!(
                    "{}",
                    serde_json::json!({"status": "starting", "transport": "stdio"})
                );
            } else {
                eprintln!("codingbuddy: starting JSON-RPC server on stdio...");
            }
            let workspace = std::env::current_dir()?;
            let handler = codingbuddy_jsonrpc::IdeRpcHandler::new(&workspace)?;
            codingbuddy_jsonrpc::run_stdio_server(&handler)
        }
        "http" => {
            let bind = format!("{}:{}", args.host, args.port);
            if json_mode {
                println!(
                    "{}",
                    json!({"status": "starting", "transport": "http", "address": bind})
                );
            } else {
                eprintln!("codingbuddy: starting HTTP server on http://{bind}");
                if args.web {
                    eprintln!("codingbuddy: web UI available at http://{bind}/");
                }
            }
            let workspace = std::env::current_dir()?;
            let handler = Arc::new(codingbuddy_jsonrpc::IdeRpcHandler::new(&workspace)?);
            run_http_server(&bind, handler, args.web)
        }
        other => Err(anyhow!(
            "unsupported transport '{}' (supported: stdio, http)",
            other
        )),
    }
}

fn run_http_server(
    bind: &str,
    handler: Arc<codingbuddy_jsonrpc::IdeRpcHandler>,
    serve_web: bool,
) -> Result<()> {
    let server =
        tiny_http::Server::http(bind).map_err(|e| anyhow!("failed to bind HTTP server: {e}"))?;
    let dist_dir = if serve_web { resolve_web_dist() } else { None };

    for request in server.incoming_requests() {
        let method = request.method().to_string();
        let url = request.url().to_string();

        match (method.as_str(), url.as_str()) {
            ("POST", "/rpc") => handle_rpc_request(request, &handler),
            ("GET", "/health") => {
                let _ = request.respond(
                    tiny_http::Response::from_string(r#"{"status":"ok"}"#)
                        .with_header(HEADER_JSON.clone()),
                );
            }
            ("GET", "/") if dist_dir.is_some() => {
                serve_web_index(request, dist_dir.as_deref().unwrap())
            }
            ("GET", path) if dist_dir.is_some() && path.starts_with("/assets/") => {
                serve_web_asset(request, dist_dir.as_deref().unwrap(), path)
            }
            ("OPTIONS", _) => {
                let _ = request.respond(
                    tiny_http::Response::from_string("")
                        .with_header(HEADER_CORS.clone())
                        .with_header(HEADER_CORS_METHODS.clone())
                        .with_header(HEADER_CORS_HEADERS.clone()),
                );
            }
            _ => {
                let _ = request.respond(
                    tiny_http::Response::from_string(r#"{"error":"not found"}"#)
                        .with_status_code(404)
                        .with_header(HEADER_JSON.clone()),
                );
            }
        }
    }
    Ok(())
}

fn send_parse_error(request: tiny_http::Request) {
    let _ = request.respond(
        tiny_http::Response::from_string(
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}"#,
        )
        .with_status_code(400)
        .with_header(HEADER_JSON.clone())
        .with_header(HEADER_CORS.clone()),
    );
}

fn handle_rpc_request(
    mut request: tiny_http::Request,
    handler: &codingbuddy_jsonrpc::IdeRpcHandler,
) {
    let mut body = String::new();
    if request
        .as_reader()
        .take(MAX_RPC_BODY_BYTES)
        .read_to_string(&mut body)
        .is_err()
    {
        send_parse_error(request);
        return;
    }

    let rpc_req = match serde_json::from_str::<JsonRpcRequest>(&body) {
        Ok(req) => req,
        Err(_) => {
            send_parse_error(request);
            return;
        }
    };

    let response = match handler.handle(&rpc_req.method, rpc_req.params) {
        Ok(result) => JsonRpcResponse::success(rpc_req.id, result),
        Err(e) => {
            JsonRpcResponse::error(rpc_req.id, codingbuddy_jsonrpc::ERR_INTERNAL, e.to_string())
        }
    };

    let json = serde_json::to_string(&response).unwrap_or_default();
    let _ = request.respond(
        tiny_http::Response::from_string(json)
            .with_header(HEADER_JSON.clone())
            .with_header(HEADER_CORS.clone()),
    );
}

fn resolve_web_dist() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        let beside_exe = exe.parent().map(|p| p.join("web/dist"));
        if beside_exe.as_ref().is_some_and(|p| p.is_dir()) {
            return beside_exe;
        }
    }
    let workspace = std::env::current_dir().ok()?;
    let dev = workspace.join("web/dist");
    if dev.is_dir() {
        return Some(dev);
    }
    None
}

fn serve_web_index(request: tiny_http::Request, dist: &Path) {
    match std::fs::read_to_string(dist.join("index.html")) {
        Ok(html) => {
            let _ = request.respond(
                tiny_http::Response::from_string(html).with_header(
                    "Content-Type: text/html; charset=utf-8"
                        .parse::<tiny_http::Header>()
                        .unwrap(),
                ),
            );
        }
        Err(_) => {
            let _ = request
                .respond(tiny_http::Response::from_string("not found").with_status_code(404));
        }
    }
}

fn serve_web_asset(request: tiny_http::Request, dist: &Path, path: &str) {
    let clean = path.trim_start_matches('/');
    if clean.contains("..") {
        let _ =
            request.respond(tiny_http::Response::from_string("forbidden").with_status_code(403));
        return;
    }
    match std::fs::read(dist.join(clean)) {
        Ok(data) => {
            let content_type = if path.ends_with(".js") {
                "application/javascript"
            } else if path.ends_with(".css") {
                "text/css"
            } else if path.ends_with(".svg") {
                "image/svg+xml"
            } else {
                "application/octet-stream"
            };
            let _ = request.respond(
                tiny_http::Response::from_data(data).with_header(
                    format!("Content-Type: {content_type}")
                        .parse::<tiny_http::Header>()
                        .unwrap(),
                ),
            );
        }
        Err(_) => {
            let _ = request
                .respond(tiny_http::Response::from_string("not found").with_status_code(404));
        }
    }
}

pub(crate) fn run_native_host(cwd: &Path, args: NativeHostArgs) -> Result<()> {
    let workspace_root = args.workspace_root.unwrap_or_else(|| cwd.to_path_buf());
    let handler = codingbuddy_jsonrpc::IdeRpcHandler::new(&workspace_root)?;
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());

    while let Some(raw) = read_native_message(&mut reader)? {
        let (response, should_shutdown) = handle_native_request(&handler, &raw);
        let encoded = serde_json::to_vec(&response)?;
        write_native_message(&mut writer, &encoded)?;
        if should_shutdown {
            break;
        }
    }

    Ok(())
}

fn handle_native_request(handler: &impl RpcHandler, raw: &[u8]) -> (JsonRpcResponse, bool) {
    let request = match parse_native_request(raw) {
        Ok(request) => request,
        Err(err) => {
            return (
                JsonRpcResponse::error(
                    Value::Null,
                    codingbuddy_jsonrpc::ERR_PARSE,
                    err.to_string(),
                ),
                false,
            );
        }
    };

    let should_shutdown = request.method == "shutdown";
    if should_shutdown {
        return (
            JsonRpcResponse::success(request.id, json!({"ok": true, "transport": "native"})),
            true,
        );
    }

    match handler.handle(&request.method, request.params) {
        Ok(result) => (JsonRpcResponse::success(request.id, result), false),
        Err(err) => (
            JsonRpcResponse::error(
                request.id,
                codingbuddy_jsonrpc::ERR_INTERNAL,
                err.to_string(),
            ),
            false,
        ),
    }
}

fn parse_native_request(raw: &[u8]) -> Result<JsonRpcRequest> {
    let value: Value = serde_json::from_slice(raw)?;
    if value.get("jsonrpc").is_none() && value.get("method").is_some() {
        let method = value
            .get("method")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("invalid request: 'method' must be a string"))?
            .to_string();
        let id = value.get("id").cloned().unwrap_or(Value::Null);
        let params = value.get("params").cloned().unwrap_or_else(|| json!({}));
        return Ok(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id,
            method,
            params,
        });
    }
    Ok(serde_json::from_value(value)?)
}

fn read_native_message(reader: &mut impl Read) -> Result<Option<Vec<u8>>> {
    let mut len_buf = [0_u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err.into()),
    }

    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 {
        return Err(anyhow!("invalid native message length: 0"));
    }
    if len > MAX_NATIVE_MESSAGE_BYTES {
        return Err(anyhow!(
            "native message too large: {} bytes (max {})",
            len,
            MAX_NATIVE_MESSAGE_BYTES
        ));
    }

    let mut payload = vec![0_u8; len];
    reader.read_exact(&mut payload)?;
    Ok(Some(payload))
}

fn write_native_message(writer: &mut impl Write, payload: &[u8]) -> Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| anyhow!("native message too large"))?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoHandler;

    impl RpcHandler for EchoHandler {
        fn handle(&self, method: &str, params: Value) -> Result<Value> {
            Ok(json!({"method": method, "params": params}))
        }
    }

    #[test]
    fn parse_native_request_supports_simplified_shape() {
        let request = parse_native_request(br#"{"id":1,"method":"status","params":{"ok":true}}"#)
            .expect("request");
        assert_eq!(request.jsonrpc, "2.0");
        assert_eq!(request.method, "status");
        assert_eq!(request.id, json!(1));
        assert_eq!(request.params["ok"], true);
    }

    #[test]
    fn native_message_round_trip() {
        let payload = br#"{"jsonrpc":"2.0","id":1,"method":"status","params":{}}"#;
        let mut out = Vec::new();
        write_native_message(&mut out, payload).expect("write");
        let mut cursor = io::Cursor::new(out);
        let read_back = read_native_message(&mut cursor)
            .expect("read")
            .expect("payload");
        assert_eq!(read_back, payload);
    }

    #[test]
    fn handle_native_request_shutdown_short_circuits() {
        let (response, should_shutdown) = handle_native_request(
            &EchoHandler,
            br#"{"jsonrpc":"2.0","id":"x","method":"shutdown","params":{}}"#,
        );
        assert!(should_shutdown);
        assert!(response.error.is_none());
    }
}
