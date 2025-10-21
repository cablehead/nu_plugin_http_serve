pub struct HttpServePlugin;

impl HttpServePlugin {
    pub fn new() -> Self {
        HttpServePlugin
    }
}

impl Default for HttpServePlugin {
    fn default() -> Self {
        Self::new()
    }
}
