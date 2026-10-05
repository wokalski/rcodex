//! Local visit history. Public metadata is serialized; the store owns its path
//! and reloads under a lock for every write so concurrent clients cannot clobber visits.
use crate::sessions::{Conversation, Snapshot};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    env, fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Saved {
    pub host: String,
    pub conversation: Conversation,
    /// Milliseconds since the epoch. Zero means discovered, never visited locally.
    pub visited: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    hosts: Vec<String>,
    sessions: Vec<Saved>,
}

pub struct History {
    path: PathBuf,
    cache: Cache,
}

impl Cache {
    fn read(path: &Path) -> Result<Self> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("read conversation history"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    fn sort(&mut self) {
        self.hosts.sort();
        self.hosts.dedup();
        self.sessions
            .sort_by(|a, b| match (a.visited > 0, b.visited > 0) {
                (true, true) => b.visited.cmp(&a.visited),
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                (false, false) => b.conversation.updated_at.cmp(&a.conversation.updated_at),
            });
    }

    fn remember(&mut self, host: &str) {
        if !self.hosts.iter().any(|h| h == host) {
            self.hosts.push(host.into());
        }
    }

    fn merge(&mut self, host: &str, snapshot: &Snapshot) {
        self.remember(host);
        if snapshot.complete && snapshot.server_running {
            let ids: HashSet<_> = snapshot.sessions.iter().map(|s| s.id.as_str()).collect();
            self.sessions.retain(|row| {
                row.host != host || row.visited > 0 || ids.contains(row.conversation.id.as_str())
            });
            // Codex can omit empty/archived threads. Preserve explicit visits,
            // including those made by another client while this query ran.
            for row in self.sessions.iter_mut().filter(|r| r.host == host) {
                if !ids.contains(row.conversation.id.as_str()) {
                    row.conversation.status = "saved · not listed".into();
                }
            }
        }
        for conversation in &snapshot.sessions {
            if let Some(row) = self
                .sessions
                .iter_mut()
                .find(|r| r.host == host && r.conversation.id == conversation.id)
            {
                row.conversation = conversation.clone(); // retain local visit time
            } else {
                self.sessions.push(Saved {
                    host: host.into(),
                    conversation: conversation.clone(),
                    visited: 0,
                });
            }
        }
    }
}

impl History {
    pub fn load() -> Result<Self> {
        Self::open(default_path()?)
    }

    fn open(path: PathBuf) -> Result<Self> {
        let mut cache = Cache::read(&path)?;
        cache.sort();
        Ok(Self { path, cache })
    }

    pub fn hosts(&self) -> &[String] {
        &self.cache.hosts
    }
    pub fn sessions(&self) -> &[Saved] {
        &self.cache.sessions
    }

    fn transaction(&mut self, update: impl FnOnce(&mut Cache) -> Result<()>) -> Result<()> {
        let parent = self.path.parent().context("history path has no parent")?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
        let lock = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(self.path.with_extension("lock"))?;
        lock.lock()?;
        let mut cache = Cache::read(&self.path)?;
        update(&mut cache)?;
        cache.sort();
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        temp.write_all(&serde_json::to_vec_pretty(&cache)?)?;
        temp.as_file().sync_all()?;
        temp.persist(&self.path).map_err(|e| e.error)?;
        self.cache = cache;
        Ok(())
    }

    pub fn add_host(&mut self, host: &str) -> Result<()> {
        let host = host.trim();
        anyhow::ensure!(
            !host.is_empty() && !host.starts_with('-') && !host.chars().any(char::is_control),
            "invalid host"
        );
        self.transaction(|cache| {
            cache.remember(host);
            Ok(())
        })
    }

    pub fn merge(&mut self, host: &str, snapshot: &Snapshot) -> Result<()> {
        self.transaction(|cache| {
            cache.merge(host, snapshot);
            Ok(())
        })
    }

    pub fn visit(&mut self, host: &str, conversation: Conversation) -> Result<()> {
        self.transaction(|cache| {
            cache.remember(host);
            let prior_max = cache.sessions.iter().map(|r| r.visited).max().unwrap_or(0);
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
            cache
                .sessions
                .retain(|r| !(r.host == host && r.conversation.id == conversation.id));
            cache.sessions.push(Saved {
                host: host.into(),
                conversation,
                visited: now.max(prior_max.saturating_add(1)),
            });
            Ok(())
        })
    }
}

fn default_path() -> Result<PathBuf> {
    if let Some(value) = env::var_os("RCODEX_HISTORY_FILE") {
        let path = PathBuf::from(value);
        anyhow::ensure!(path.is_absolute(), "RCODEX_HISTORY_FILE must be absolute");
        return Ok(path);
    }
    let root = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/state")))
        .context("HOME is unset")?;
    Ok(root.join("rcodex/history.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn convo(id: &str, updated: i64) -> Conversation {
        Conversation {
            id: id.into(),
            title: id.into(),
            updated_at: updated,
            ..Default::default()
        }
    }
    fn complete(sessions: Vec<Conversation>) -> Snapshot {
        Snapshot {
            sessions,
            complete: true,
            server_running: true,
            ..Default::default()
        }
    }

    #[test]
    fn partial_and_stopped_snapshots_cannot_prune_other_hosts_or_visits() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = History::open(dir.path().join("history.json")).unwrap();
        store
            .merge("a", &complete(vec![convo("discovered", 9)]))
            .unwrap();
        store.visit("a", convo("visited", 1)).unwrap();
        store.visit("b", convo("visited", 2)).unwrap();
        for snapshot in [
            Snapshot {
                server_running: true,
                ..Default::default()
            },
            Snapshot {
                complete: true,
                ..Default::default()
            },
        ] {
            store.merge("a", &snapshot).unwrap();
            assert_eq!(store.sessions().len(), 3);
        }
        store.merge("a", &complete(vec![])).unwrap();
        assert_eq!(store.sessions().len(), 2);
        assert_eq!(store.sessions()[0].host, "b");
        assert_eq!(
            store.sessions()[1].conversation.status,
            "saved · not listed"
        );
    }

    #[test]
    fn concurrent_clients_preserve_visits_and_wire_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        let mut writer = History::open(path.clone()).unwrap();
        writer.visit("host", convo("first", 100)).unwrap();
        let first_visit = writer.sessions()[0].visited;
        let mut stale = History::open(path.clone()).unwrap();
        writer.visit("host", convo("second", 1)).unwrap();
        stale
            .merge(
                "host",
                &complete(vec![convo("first", 999), convo("discovered", 9999)]),
            )
            .unwrap();
        assert_eq!(
            stale
                .sessions()
                .iter()
                .map(|r| r.conversation.id.as_str())
                .collect::<Vec<_>>(),
            ["second", "first", "discovered"]
        );
        assert_eq!(stale.sessions()[1].visited, first_visit);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(json["hosts"], serde_json::json!(["host"]));
        assert_eq!(json.as_object().unwrap().len(), 2);
        assert!(json.get("path").is_none());
    }

    #[test]
    fn separate_stores_do_not_share_global_state_and_failed_writes_preserve_memory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.json");
        let mut a = History::open(path.clone()).unwrap();
        let mut b = History::open(dir.path().join("b.json")).unwrap();
        a.add_host("alpha").unwrap();
        b.add_host("beta").unwrap();
        assert_eq!(a.hosts(), &["alpha"]);
        assert_eq!(b.hosts(), &["beta"]);
        fs::write(&path, "broken JSON").unwrap();
        assert!(a.add_host("gamma").is_err());
        assert_eq!(a.hosts(), &["alpha"]);
        assert_eq!(fs::read_to_string(path).unwrap(), "broken JSON");
    }
}
