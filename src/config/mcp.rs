//! Model Context Protocol server settings.
use serde::{Deserialize, Serialize};

/// Largest permitted inline media size (256 MiB).
pub const MAX_MEDIA_BYTES: usize = 256 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Mcp {
    pub enabled: bool,
    /// Largest audio/image payload MCP tools return inline.
    pub max_media_bytes: usize,
}

impl Default for Mcp {
    fn default() -> Self {
        Self {
            enabled: true,
            max_media_bytes: 16 * 1024 * 1024,
        }
    }
}

impl Mcp {
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (1..=MAX_MEDIA_BYTES).contains(&self.max_media_bytes),
            "mcp.max_media_bytes must be between 1 and {MAX_MEDIA_BYTES}"
        );
        Ok(())
    }
}
