//! gRPC server scaffolding. Phase 7 of the plan fills in the full
//! Microgrid trait — for now this module just exposes the placeholder
//! types so `lib.rs` and the binary compile while the real impl lands.

use crate::lisp::Config;

pub struct MicrogridServer {
    pub config: Config,
}

impl MicrogridServer {
    pub fn new(config: Config) -> Self {
        Self { config }
    }
}
