//! Pseudo-harness for a bare terminal session: no agent CLI, no event
//! translation. The PTY modes (rmux/tmux) spawn the user's shell and the
//! operator drives whatever tool they like by hand — e.g. a harness orgasmic
//! does not natively support yet. The adapter exists because mode drivers are
//! constructed around one; for a plain shell it only supplies the harness id
//! and passes captured output through as text.

use async_trait::async_trait;
use serde_json::Value;

use orgasmic_core::{DriverEvent, TextStream};

use crate::r#trait::{
    DriverConfig, DriverContext, DriverError, HarnessEventAdapter, HarnessRequest,
};

#[derive(Default)]
pub struct ShellAdapter {
    seq: u64,
}

impl ShellAdapter {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl HarnessEventAdapter for ShellAdapter {
    fn harness(&self) -> &'static str {
        "custom"
    }

    fn clone_box(&self) -> Box<dyn HarnessEventAdapter> {
        Box::new(Self::new())
    }

    async fn parse_event(&mut self, raw: Value) -> Vec<DriverEvent> {
        // A shell has no structured event stream; surface anything that
        // arrives as plain text so nothing is silently dropped.
        vec![self.text_event(TextStream::Stdout, raw.to_string())]
    }

    fn compose_request(
        &mut self,
        _ctx: &DriverContext,
        _config: &DriverConfig,
    ) -> Result<HarnessRequest, DriverError> {
        // Only the PTY modes make sense for a bare shell; they never call
        // compose_request (they build their own spawn plan).
        Err(DriverError::Unsupported(
            "custom harness runs only under a PTY mode (rmux/tmux)",
        ))
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
}
