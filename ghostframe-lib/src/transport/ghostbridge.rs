//! Rust FFI to the ghostbridge Go C archive. Replaced in Task 3.

pub struct GhostbridgeConfig {
    pub hostname: String,
    pub authkey: String,
    pub state_dir: String,
    pub control_url: String,
}

pub struct GhostbridgeHandle;

impl GhostbridgeHandle {
    pub fn connect(_config: &GhostbridgeConfig) -> Result<Self, String> {
        Err("not implemented".into())
    }
}
