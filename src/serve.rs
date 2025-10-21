use nu_plugin::{EngineInterface, EvaluatedCall, PluginCommand};
use nu_protocol::{
    engine::Closure, IntoSpanned, LabeledError, PipelineData, Record, Signature, Span, Spanned,
    SyntaxShape, Type, Value,
};
use std::path::Path;
use std::sync::mpsc;

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
                "path",
                SyntaxShape::String,
                "Unix socket path to bind to (e.g., ./server.sock)",
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
    // Create Unix socket server
    let server = tiny_http::Server::http_unix(Path::new(&socket_path))
        .map_err(|e| LabeledError::new(format!("Failed to bind to socket: {}", e)))?;

    eprintln!("Listening on {}", socket_path);

    // Accept connections in a loop
    loop {
        // Check for shutdown signal (non-blocking)
        if shutdown_rx.try_recv().is_ok() {
            eprintln!("Shutting down server...");
            break;
        }

        // Try to receive a request (with timeout to allow checking shutdown signal)
        match server.try_recv() {
            Ok(Some(request)) => {
                // Spawn a thread to handle this request
                let engine = engine.clone();
                let closure = closure.clone();

                std::thread::spawn(move || {
                    handle_request(engine, span, closure, request);
                });
            }
            Ok(None) => {
                // No request available, sleep briefly
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => {
                eprintln!("Error receiving request: {}", e);
            }
        }
    }

    Ok(())
}

/// Handle a single HTTP request
fn handle_request(
    _engine: EngineInterface,
    span: Span,
    _closure: Spanned<Closure>,
    request: tiny_http::Request,
) {
    // Convert HTTP request to Nu Value
    let _request_value = request_to_value(&request, span);

    // TODO (nushell-8): Evaluate closure with request and send response
    // For now, just send a placeholder response
    let response = tiny_http::Response::from_string("HTTP server is running");
    if let Err(e) = request.respond(response) {
        eprintln!("Error sending response: {}", e);
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
