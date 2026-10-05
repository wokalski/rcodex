use serde::{Deserialize, Serialize};

/// Public conversation metadata only. Never place app-server credentials here.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub status: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub server_running: bool,
    pub sessions: Vec<Conversation>,
    /// True only if discovery completed; incomplete snapshots must not purge cache.
    pub complete: bool,
    pub warnings: Vec<String>,
}

impl Snapshot {
    /// A partial refresh can be displayed, but cannot determine the latest thread.
    pub fn require_complete(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.complete,
            "conversation discovery is incomplete: {}",
            self.warnings.join("; ")
        );
        Ok(())
    }
}
