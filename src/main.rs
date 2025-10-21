use nu_plugin::{serve_plugin, JsonSerializer};

fn main() {
    let plugin = nu_plugin_http_serve::HttpServePlugin::new();
    serve_plugin(&plugin, JsonSerializer)
}
