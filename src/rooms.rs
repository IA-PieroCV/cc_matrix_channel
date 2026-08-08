//! Durable memory of which rooms have talked to the bridge.
//!
//! `known_rooms` gates outbound traffic (`check_outbound_gate`) and tells the live status
//! loop where to post. Held only in memory it is lost on restart, which makes the status
//! feature silently mute until someone messages the bridge again — precisely when you
//! would most want to hear that the agent is stuck. So it is persisted next to
//! `access.json`.
//!
//! `last_active` is tracked separately and deliberately: `known_rooms` is a `HashSet`, so
//! picking "a" room from it is arbitrary once more than one room is known. Status belongs
//! in the room that most recently spoke to the bridge.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use matrix_sdk::ruma::OwnedRoomId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RoomMemoryFile {
    #[serde(default)]
    rooms: Vec<String>,
    #[serde(default)]
    last_active: Option<String>,
}

/// `~/.claude/channels/matrix/known_rooms.json`, alongside `access.json`.
pub fn store_path() -> PathBuf {
    dirs_next::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("channels")
        .join("matrix")
        .join("known_rooms.json")
}

/// Load remembered rooms. A missing or corrupt file is not an error — the bridge simply
/// starts with no memory, exactly as it did before this existed.
pub fn load(path: &Path) -> (HashSet<OwnedRoomId>, Option<OwnedRoomId>) {
    let Ok(data) = std::fs::read_to_string(path) else {
        return (HashSet::new(), None);
    };
    let parsed: RoomMemoryFile = match serde_json::from_str(&data) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("Ignoring unreadable known_rooms.json: {e}");
            return (HashSet::new(), None);
        }
    };

    let rooms: HashSet<OwnedRoomId> = parsed
        .rooms
        .iter()
        .filter_map(|s| OwnedRoomId::try_from(s.as_str()).ok())
        .collect();
    let last_active = parsed
        .last_active
        .and_then(|s| OwnedRoomId::try_from(s.as_str()).ok())
        // Never hand back a last_active that is not itself a known room.
        .filter(|r| rooms.contains(r));

    if !rooms.is_empty() {
        tracing::info!("Restored {} known room(s) from {}", rooms.len(), path.display());
    }
    (rooms, last_active)
}

/// Persist atomically, mirroring how `AccessControl::save_config` writes.
pub fn save(path: &Path, rooms: &HashSet<OwnedRoomId>, last_active: Option<&OwnedRoomId>) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = RoomMemoryFile {
        rooms: rooms.iter().map(|r| r.to_string()).collect(),
        last_active: last_active.map(|r| r.to_string()),
    };
    let Ok(data) = serde_json::to_string_pretty(&file) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, &data).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room(s: &str) -> OwnedRoomId {
        OwnedRoomId::try_from(s).unwrap()
    }

    #[test]
    fn round_trips_rooms_and_last_active() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_rooms.json");

        let mut rooms = HashSet::new();
        rooms.insert(room("!a:example.com"));
        rooms.insert(room("!b:example.com"));
        save(&path, &rooms, Some(&room("!b:example.com")));

        let (loaded, last) = load(&path);
        assert_eq!(loaded, rooms);
        assert_eq!(last, Some(room("!b:example.com")));
    }

    /// The whole point of the change: a restart must not forget where to post.
    #[test]
    fn survives_a_simulated_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_rooms.json");
        let mut rooms = HashSet::new();
        rooms.insert(room("!dm:example.com"));
        save(&path, &rooms, Some(&room("!dm:example.com")));

        // Fresh process: nothing in memory, everything from disk.
        let (loaded, last) = load(&path);
        assert!(loaded.contains(&room("!dm:example.com")));
        assert_eq!(last, Some(room("!dm:example.com")));
    }

    #[test]
    fn missing_file_is_empty_not_an_error() {
        let (rooms, last) = load(Path::new("/nonexistent/known_rooms.json"));
        assert!(rooms.is_empty());
        assert!(last.is_none());
    }

    #[test]
    fn corrupt_file_degrades_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_rooms.json");
        std::fs::write(&path, "{ not json").unwrap();
        let (rooms, last) = load(&path);
        assert!(rooms.is_empty());
        assert!(last.is_none());
    }

    /// A stale `lastActive` pointing at a room no longer known must not be handed back,
    /// or status would target a room the outbound gate would reject.
    #[test]
    fn last_active_must_be_a_known_room() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_rooms.json");
        std::fs::write(
            &path,
            r#"{"rooms":["!a:example.com"],"lastActive":"!gone:example.com"}"#,
        )
        .unwrap();
        let (rooms, last) = load(&path);
        assert_eq!(rooms.len(), 1);
        assert_eq!(last, None);
    }

    #[test]
    fn unparseable_room_ids_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_rooms.json");
        std::fs::write(
            &path,
            r#"{"rooms":["!good:example.com","not-a-room-id"],"lastActive":null}"#,
        )
        .unwrap();
        let (rooms, _) = load(&path);
        assert_eq!(rooms.len(), 1);
        assert!(rooms.contains(&room("!good:example.com")));
    }
}
