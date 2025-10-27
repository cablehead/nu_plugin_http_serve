use nu_plugin::{EngineInterface, EvaluatedCall, PluginCommand};
use nu_protocol::{
    engine::Closure, IntoSpanned, LabeledError, PipelineData, Record, Signature, Span, Spanned,
    SyntaxShape, Type, Value,
};
use std::io::Read;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use crate::HttpServePlugin;

pub struct HttpServe;

impl PluginCommand for HttpServe {
    type Plugin = HttpServePlugin;

    fn name(&self) -> &str {
        "http serve"
    }

    fn description(&self) -> &str {
        "Start an HTTP server that evaluates a closure for each request"
    }

    fn signature(&self) -> Signature {
        Signature::build(PluginCommand::name(self))
            .required(
                "address",
                SyntaxShape::String,
                "Address to bind to: TCP (e.g., ':3000', '127.0.0.1:8080') or Unix socket (e.g., './server.sock')",
            )
            .required(
                "closure",
                SyntaxShape::Closure(Some(vec![SyntaxShape::Record(vec![])])),
                "The closure to evaluate for each HTTP request",
            )
            .input_output_type(Type::Any, Type::Any)
    }

    fn run(
        &self,
        _plugin: &HttpServePlugin,
        engine: &EngineInterface,
        call: &EvaluatedCall,
        _input: PipelineData,
    ) -> Result<PipelineData, LabeledError> {
        let span = call.head;

        // Parse arguments
        let socket_path = call.req::<Value>(0)?.into_string()?;
        let closure = call.req::<Value>(1)?.into_closure()?.into_spanned(span);

        // Register signal handler for Ctrl-C
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let _guard = engine.register_signal_handler(Box::new(move |_| {
            let _ = shutdown_tx.send(());
        }))?;

        // Start the HTTP server
        serve(engine, span, closure, socket_path, shutdown_rx, _guard)?;

        Ok(PipelineData::Value(
            Value::string("Server stopped", span),
            None,
        ))
    }
}

/// Start HTTP server and handle requests
fn serve(
    engine: &EngineInterface,
    span: Span,
    closure: Spanned<Closure>,
    socket_path: String,
    shutdown_rx: mpsc::Receiver<()>,
    _guard: nu_protocol::HandlerGuard,
) -> Result<(), LabeledError> {
    // Detect TCP vs Unix socket
    // TCP: starts with ':' (e.g., ':3000') or contains ':' followed by digits (e.g., '127.0.0.1:8080')
    let is_tcp = socket_path.starts_with(':')
        || socket_path.contains(':')
            && socket_path
                .split(':')
                .next_back()
                .unwrap_or("")
                .parse::<u16>()
                .is_ok();

    eprintln!("DEBUG: Creating server for {}...", socket_path);

    // Resolve Unix socket path relative to caller's working directory
    let resolved_socket_path = if !is_tcp && !socket_path.starts_with('/') {
        let cwd = engine
            .get_current_dir()
            .map_err(|e| LabeledError::new(format!("Failed to get current directory: {}", e)))?;
        let resolved = Path::new(&cwd).join(&socket_path);
        eprintln!(
            "DEBUG: Resolved relative path '{}' to '{}'",
            socket_path,
            resolved.display()
        );
        resolved.to_string_lossy().to_string()
    } else {
        socket_path.clone()
    };

    let server = if is_tcp {
        // TCP socket
        eprintln!("DEBUG: Binding TCP socket...");
        let srv = tiny_http::Server::http(&socket_path).map_err(|e| {
            LabeledError::new(format!("Failed to bind to TCP {}: {}", socket_path, e))
        })?;
        eprintln!("DEBUG: TCP socket bound successfully");
        srv
    } else {
        // Unix socket
        eprintln!("DEBUG: Binding Unix socket...");
        eprintln!("DEBUG: Using path: {}", resolved_socket_path);
        let srv = tiny_http::Server::http_unix(Path::new(&resolved_socket_path)).map_err(|e| {
            LabeledError::new(format!(
                "Failed to bind to Unix socket {}: {}",
                resolved_socket_path, e
            ))
        })?;
        eprintln!("DEBUG: Unix socket bound successfully");
        srv
    };

    if is_tcp {
        eprintln!("Listening on http://{}", socket_path);
    } else {
        eprintln!("Listening on {} (Unix socket)", resolved_socket_path);
    }

    eprintln!("DEBUG: Entering accept loop...");

    // Accept connections in a loop
    loop {
        // Check for shutdown signal (non-blocking)
        if shutdown_rx.try_recv().is_ok() {
            eprintln!("Shutting down server...");
            break;
        }

        // Blocking receive with timeout - responsive to Ctrl-C, zero request latency
        match server.recv_timeout(Duration::from_millis(100)) {
            Ok(Some(request)) => {
                eprintln!("DEBUG: Received request!");

                // Spawn a thread to handle this request
                let engine = engine.clone();
                let closure = closure.clone();

                std::thread::spawn(move || {
                    handle_request(engine, span, closure, request);
                });
            }
            Ok(None) => {
                // Timeout - loop continues, will check shutdown signal
            }
            Err(e) => {
                eprintln!("Error receiving request: {}", e);
                break;
            }
        }
    }

    eprintln!("DEBUG: Exited accept loop");
    Ok(())
}

/// Handle a single HTTP request
fn handle_request(
    engine: EngineInterface,
    span: Span,
    closure: Spanned<Closure>,
    request: tiny_http::Request,
) {
    // Convert HTTP request to Nu Value
    let request_value = request_to_value(&request, span);

    // Evaluate closure with request value
    let result = engine.eval_closure_with_stream(
        &closure,
        vec![request_value],
        PipelineData::Empty,
        true,  // redirect_stdout
        false, // redirect_stderr
    );

    // Handle the result and send HTTP response
    match result {
        Ok(pipeline_data) => {
            let response = pipeline_data_to_response(pipeline_data, span);
            if let Err(e) = request.respond(response) {
                eprintln!("Error sending response: {}", e);
            }
        }
        Err(err) => {
            // Send error response
            eprintln!("Error evaluating closure: {}", err);
            let error_msg = format!("Error: {}", err);
            let response = tiny_http::Response::from_string(error_msg).with_status_code(500);
            if let Err(e) = request.respond(response) {
                eprintln!("Error sending error response: {}", e);
            }
        }
    }
}

/// Convert tiny_http::Request to Nu Value (Record)
fn request_to_value(request: &tiny_http::Request, span: Span) -> Value {
    let mut record = Record::new();

    // Method
    record.push("method", Value::string(request.method().to_string(), span));

    // Path/URL
    record.push("path", Value::string(request.url(), span));

    // Headers
    let mut headers_record = Record::new();
    for header in request.headers() {
        headers_record.push(
            header.field.to_string(),
            Value::string(header.value.to_string(), span),
        );
    }
    record.push("headers", Value::record(headers_record, span));

    // Query parameters (parse from URL)
    let mut query_record = Record::new();
    if let Some(query_start) = request.url().find('?') {
        let query_string = &request.url()[query_start + 1..];
        for param in query_string.split('&') {
            if let Some(eq_pos) = param.find('=') {
                let key = &param[..eq_pos];
                let value = &param[eq_pos + 1..];
                query_record.push(key, Value::string(value, span));
            }
        }
    }
    record.push("query", Value::record(query_record, span));

    // Remote address (None for Unix sockets)
    if let Some(addr) = request.remote_addr() {
        record.push("remote_addr", Value::string(addr.to_string(), span));
    }

    Value::record(record, span)
}

/// Convert PipelineData to tiny_http::Response
fn pipeline_data_to_response(
    pipeline_data: PipelineData,
    _span: Span,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    match pipeline_data {
        // Empty or Nothing -> 204 No Content with empty body
        PipelineData::Empty => tiny_http::Response::from_data(Vec::new()).with_status_code(204),

        // Value -> serialize to bytes
        PipelineData::Value(value, meta) => {
            match value {
                Value::Nothing { .. } => {
                    tiny_http::Response::from_data(Vec::new()).with_status_code(204)
                }
                Value::Record { .. } => {
                    // Records -> JSON with application/json content-type
                    let body = value_to_bytes(value);
                    let content_type = infer_content_type(&meta, Some("application/json"));
                    tiny_http::Response::from_data(body)
                        .with_header(content_type_header(&content_type))
                }
                Value::List { .. } => {
                    // Lists -> JSON with application/json content-type
                    let body = value_to_bytes(value);
                    let content_type = infer_content_type(&meta, Some("application/json"));
                    tiny_http::Response::from_data(body)
                        .with_header(content_type_header(&content_type))
                }
                _ => {
                    // Other values -> text/plain
                    let body = value_to_bytes(value);
                    let content_type = infer_content_type(&meta, Some("text/plain; charset=utf-8"));
                    tiny_http::Response::from_data(body)
                        .with_header(content_type_header(&content_type))
                }
            }
        }

        // ListStream -> collect and serialize to JSON array
        PipelineData::ListStream(stream, meta) => {
            let mut body = Vec::new();
            for value in stream.into_iter() {
                body.extend(value_to_bytes(value));
                body.push(b'\n'); // Separate items with newlines
            }
            let content_type = infer_content_type(&meta, Some("application/json"));
            tiny_http::Response::from_data(body).with_header(content_type_header(&content_type))
        }

        // ByteStream -> stream to response
        PipelineData::ByteStream(stream, meta) => match stream.reader() {
            Some(mut reader) => {
                let mut body = Vec::new();
                if let Err(e) = reader.read_to_end(&mut body) {
                    eprintln!("Error reading ByteStream: {}", e);
                    return tiny_http::Response::from_string(format!("Error: {}", e))
                        .with_status_code(500);
                }
                let content_type = infer_content_type(&meta, Some("application/octet-stream"));
                tiny_http::Response::from_data(body).with_header(content_type_header(&content_type))
            }
            None => {
                eprintln!("ByteStream has no reader");
                tiny_http::Response::from_string("Error: ByteStream has no reader")
                    .with_status_code(500)
            }
        },
    }
}

/// Infer content-type from metadata or use default
fn infer_content_type(
    meta: &Option<nu_protocol::PipelineMetadata>,
    default: Option<&str>,
) -> String {
    meta.as_ref()
        .and_then(|m| m.content_type.as_ref())
        .map(|s| s.to_string())
        .or_else(|| default.map(|s| s.to_string()))
        .unwrap_or_else(|| "text/plain; charset=utf-8".to_string())
}

/// Create Content-Type header
fn content_type_header(content_type: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
        .expect("Invalid Content-Type header")
}

/// Convert Nu Value to bytes for HTTP response body
fn value_to_bytes(value: Value) -> Vec<u8> {
    match value {
        Value::Nothing { .. } => Vec::new(),
        Value::String { val, .. } => val.into_bytes(),
        Value::Int { val, .. } => val.to_string().into_bytes(),
        Value::Float { val, .. } => val.to_string().into_bytes(),
        Value::Binary { val, .. } => val,
        Value::Bool { val, .. } => val.to_string().into_bytes(),

        // Lists and Records -> JSON (following http-nu pattern)
        Value::List { .. } | Value::Record { .. } => serde_json::to_string(&value_to_json(&value))
            .unwrap_or_else(|_| String::new())
            .into_bytes(),

        _ => format!("{:?}", value).into_bytes(),
    }
}

/// Convert Nu Value to serde_json::Value (following http-nu pattern)
fn value_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::Nothing { .. } => serde_json::Value::Null,
        Value::Bool { val, .. } => serde_json::Value::Bool(*val),
        Value::Int { val, .. } => serde_json::Value::Number((*val).into()),
        Value::Float { val, .. } => serde_json::Number::from_f64(*val)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::String { val, .. } => serde_json::Value::String(val.clone()),
        Value::List { vals, .. } => {
            serde_json::Value::Array(vals.iter().map(value_to_json).collect())
        }
        Value::Record { val, .. } => {
            let mut map = serde_json::Map::new();
            for (k, v) in val.iter() {
                map.insert(k.clone(), value_to_json(v));
            }
            serde_json::Value::Object(map)
        }
        _ => serde_json::Value::String(format!("{:?}", value)),
    }
}
