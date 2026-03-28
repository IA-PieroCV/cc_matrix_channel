use matrix_sdk::ruma::{OwnedRoomId, OwnedUserId};
use parking_lot::Mutex;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_PENDING: usize = 3;
const MAX_REPLIES_PER_SENDER: u8 = 2;

// --- Config types (persisted in access.json) ---

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DmPolicy {
    Pairing,
    Allowlist,
    Disabled,
}

impl Default for DmPolicy {
    fn default() -> Self {
        Self::Pairing
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ReplyToMode {
    First,
    All,
    Off,
}

impl Default for ReplyToMode {
    fn default() -> Self {
        Self::First
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ChunkMode {
    Length,
    Newline,
}

impl Default for ChunkMode {
    fn default() -> Self {
        Self::Newline
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupConfig {
    #[serde(default = "default_true")]
    pub require_mention: bool,
    #[serde(default)]
    pub allow_from: Vec<String>,
    #[serde(default)]
    pub mention_patterns: Vec<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingEntry {
    pub sender_id: String,
    pub chat_id: String,
    pub created_at: u64,
    pub expires_at: u64,
    #[serde(default = "default_one")]
    pub replies: u8,
}

fn default_one() -> u8 {
    1
}

fn default_ack_reaction() -> String {
    "\u{1F440}".to_string() // 👀
}

fn default_text_chunk_limit() -> usize {
    4096
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessConfig {
    #[serde(default)]
    pub dm_policy: DmPolicy,
    #[serde(default)]
    pub allow_from: Vec<String>,
    #[serde(default)]
    pub groups: HashMap<String, GroupConfig>,
    #[serde(default)]
    pub pending: HashMap<String, PendingEntry>,
    #[serde(default = "default_ack_reaction")]
    pub ack_reaction: String,
    #[serde(default = "default_text_chunk_limit")]
    pub text_chunk_limit: usize,
    #[serde(default)]
    pub chunk_mode: ChunkMode,
    #[serde(default)]
    pub reply_to_mode: ReplyToMode,
}

impl Default for AccessConfig {
    fn default() -> Self {
        Self {
            dm_policy: DmPolicy::default(),
            allow_from: Vec::new(),
            groups: HashMap::new(),
            pending: HashMap::new(),
            ack_reaction: default_ack_reaction(),
            text_chunk_limit: default_text_chunk_limit(),
            chunk_mode: ChunkMode::default(),
            reply_to_mode: ReplyToMode::default(),
        }
    }
}

// --- Error types ---

#[derive(Debug)]
pub enum AccessDenied {
    PairingRequired(String),
    PairingPending(String),
    TooManyPending,
    Denied,
}

impl fmt::Display for AccessDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PairingRequired(code) => write!(f, "Pairing required. Code: {code}"),
            Self::PairingPending(code) => write!(f, "Pairing already pending. Code: {code}"),
            Self::TooManyPending => write!(f, "Too many pending pairings"),
            Self::Denied => write!(f, "Access denied"),
        }
    }
}

#[derive(Debug)]
pub enum PairingError {
    NoPendingPairing,
    Expired,
}

impl fmt::Display for PairingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoPendingPairing => write!(f, "No pending pairing for this user"),
            Self::Expired => write!(f, "Pairing code has expired"),
        }
    }
}

// --- AccessControl ---

pub struct AccessControl {
    config_path: PathBuf,
    last_mtime: Mutex<Option<SystemTime>>,
    cached: Mutex<AccessConfig>,
}

impl AccessControl {
    pub fn new(config_path: PathBuf) -> Self {
        let cached = if config_path.exists() {
            match Self::read_config_file(&config_path) {
                Ok(config) => {
                    tracing::info!("Loaded access config from {}", config_path.display());
                    config
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to load access config from {}: {e} — using defaults",
                        config_path.display()
                    );
                    AccessConfig::default()
                }
            }
        } else {
            tracing::info!(
                "No access.json at {} — using defaults. Configure via /matrix:access",
                config_path.display()
            );
            AccessConfig::default()
        };

        Self {
            config_path,
            last_mtime: Mutex::new(None),
            cached: Mutex::new(cached),
        }
    }

    fn read_config_file(path: &PathBuf) -> Result<AccessConfig, String> {
        let data = std::fs::read_to_string(path).map_err(|e| format!("read failed: {e}"))?;
        serde_json::from_str(&data).map_err(|e| format!("parse failed: {e}"))
    }

    /// Load config, using mtime-based caching to avoid unnecessary re-parses.
    pub fn load_config(&self) -> AccessConfig {
        if !self.config_path.exists() {
            return AccessConfig::default();
        }

        let current_mtime = std::fs::metadata(&self.config_path)
            .ok()
            .and_then(|m| m.modified().ok());

        let last = *self.last_mtime.lock();
        if current_mtime == last && last.is_some() {
            return self.cached.lock().clone();
        }

        match Self::read_config_file(&self.config_path) {
            Ok(config) => {
                *self.last_mtime.lock() = current_mtime;
                *self.cached.lock() = config.clone();
                config
            }
            Err(e) => {
                tracing::error!("Failed to reload access config: {e} — using cached");
                self.cached.lock().clone()
            }
        }
    }

    /// Save config to disk (atomic write).
    pub fn save_config(&self, config: &AccessConfig) {
        if let Some(parent) = self.config_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let data = match serde_json::to_string_pretty(config) {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("Failed to serialize access config: {e}");
                return;
            }
        };
        let tmp_path = self.config_path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp_path, &data) {
            tracing::error!("Failed to write access config: {e}");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp_path, &self.config_path) {
            tracing::error!("Failed to rename access config: {e}");
            return;
        }
        // Update cache
        *self.last_mtime.lock() = std::fs::metadata(&self.config_path)
            .ok()
            .and_then(|m| m.modified().ok());
        *self.cached.lock() = config.clone();
    }

    pub fn check_sender(
        &self,
        user_id: &OwnedUserId,
        room_id: &OwnedRoomId,
    ) -> Result<(), AccessDenied> {
        let mut config = self.load_config();
        let now = now_millis();

        // Disabled policy → access control off, allow everyone
        if config.dm_policy == DmPolicy::Disabled {
            return Ok(());
        }

        // Check allowFrom (includes "*" for allow-all)
        let user_str = user_id.as_str();
        if config.allow_from.iter().any(|u| u == "*" || u == user_str) {
            return Ok(());
        }

        // Check group-specific allowFrom
        let room_str = room_id.as_str();
        if let Some(group) = config.groups.get(room_str) {
            if group.allow_from.iter().any(|u| u == "*" || u == user_str) {
                return Ok(());
            }
        }

        // Allowlist policy with no match → deny (no pairing)
        if config.dm_policy == DmPolicy::Allowlist {
            return Err(AccessDenied::Denied);
        }

        // Pairing flow
        // Prune expired entries
        config.pending.retain(|_, entry| entry.expires_at > now);

        // Check for existing pending pairing for this sender
        let existing_code = config
            .pending
            .iter()
            .find(|(_, entry)| entry.sender_id == user_str)
            .map(|(code, entry)| (code.clone(), entry.replies));

        if let Some((code, replies)) = existing_code {
            if replies >= MAX_REPLIES_PER_SENDER {
                self.save_config(&config);
                return Err(AccessDenied::PairingPending(code));
            }
            self.save_config(&config);
            return Err(AccessDenied::PairingRequired(code));
        }

        if config.pending.len() >= MAX_PENDING {
            self.save_config(&config);
            return Err(AccessDenied::TooManyPending);
        }

        // Generate new pairing code
        let code = generate_code();
        config.pending.insert(
            code.clone(),
            PendingEntry {
                sender_id: user_str.to_string(),
                chat_id: room_id.to_string(),
                created_at: now,
                expires_at: now + 3_600_000, // 1 hour
                replies: 0,
            },
        );
        self.save_config(&config);
        Err(AccessDenied::PairingRequired(code))
    }

    pub fn mark_pairing_reply_sent(&self, user_id: &OwnedUserId) {
        let mut config = self.load_config();
        let user_str = user_id.as_str();
        let entry = config
            .pending
            .values_mut()
            .find(|e| e.sender_id == user_str);
        if let Some(entry) = entry {
            entry.replies = entry.replies.saturating_add(1);
            self.save_config(&config);
        }
    }

    /// Returns the room_id where the pairing was requested on success.
    pub fn approve_pairing(
        &self,
        user_id: &OwnedUserId,
        code: &str,
    ) -> Result<OwnedRoomId, PairingError> {
        let mut config = self.load_config();
        let now = now_millis();

        // Prune expired
        config.pending.retain(|_, entry| entry.expires_at > now);

        // Constant-time code lookup: iterate ALL pending entries to avoid
        // timing side-channels that would reveal whether a code exists.
        let matched = config
            .pending
            .iter()
            .find(|(k, _)| constant_time_eq::constant_time_eq(k.as_bytes(), code.as_bytes()));

        let (matched_code, entry) = match matched {
            None => return Err(PairingError::NoPendingPairing),
            Some((k, v)) => (k.clone(), v),
        };

        if entry.sender_id != user_id.as_str() {
            return Err(PairingError::NoPendingPairing);
        }
        if entry.expires_at <= now {
            config.pending.remove(&matched_code);
            self.save_config(&config);
            return Err(PairingError::Expired);
        }

        let chat_id = entry.chat_id.clone();
        let room_id =
            OwnedRoomId::try_from(chat_id.as_str()).map_err(|_| PairingError::NoPendingPairing)?;

        // Move user to allowFrom
        let user_str = user_id.to_string();
        if !config.allow_from.contains(&user_str) {
            config.allow_from.push(user_str);
        }
        config.pending.remove(&matched_code);
        self.save_config(&config);

        tracing::info!("Approved pairing for {user_id}");
        Ok(room_id)
    }

    // --- Delivery setting accessors ---

    pub fn requires_mention(&self, room_id: &matrix_sdk::ruma::RoomId) -> bool {
        let config = self.load_config();
        config
            .groups
            .get(room_id.as_str())
            .is_some_and(|g| g.require_mention)
    }

    pub fn mention_patterns(&self, room_id: &matrix_sdk::ruma::RoomId) -> Vec<String> {
        let config = self.load_config();
        config
            .groups
            .get(room_id.as_str())
            .map(|g| g.mention_patterns.clone())
            .unwrap_or_default()
    }

    pub fn ack_reaction(&self) -> Option<String> {
        let config = self.load_config();
        if config.ack_reaction.is_empty() {
            None
        } else {
            Some(config.ack_reaction)
        }
    }

    pub fn text_chunk_limit(&self) -> usize {
        let config = self.load_config();
        config.text_chunk_limit.max(100) // sane minimum
    }

    pub fn chunk_mode(&self) -> ChunkMode {
        self.load_config().chunk_mode
    }

    pub fn reply_to_mode(&self) -> ReplyToMode {
        self.load_config().reply_to_mode
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn generate_code() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::rng();
    (0..6)
        .map(|_| {
            let idx = rng.random_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_room_id() -> OwnedRoomId {
        OwnedRoomId::try_from("!test:example.com").unwrap()
    }

    fn make_access_control(config: AccessConfig) -> AccessControl {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("access.json");
        let data = serde_json::to_string_pretty(&config).unwrap();
        std::fs::write(&path, &data).unwrap();
        // Leak the dir so the file persists for the test duration
        std::mem::forget(dir);
        AccessControl::new(path)
    }

    fn make_access_control_no_file() -> AccessControl {
        let path = PathBuf::from("/tmp/nonexistent-access-test.json");
        let _ = std::fs::remove_file(&path);
        AccessControl::new(path)
    }

    #[test]
    fn allow_all_passes_everyone() {
        let mut config = AccessConfig::default();
        config.allow_from = vec!["*".to_string()];
        let ac = make_access_control(config);
        let user = OwnedUserId::try_from("@alice:example.com").unwrap();
        assert!(ac.check_sender(&user, &test_room_id()).is_ok());
    }

    #[test]
    fn allowlist_passes_listed_user() {
        let mut config = AccessConfig::default();
        config.allow_from = vec!["@alice:example.com".to_string()];
        let ac = make_access_control(config);
        let user = OwnedUserId::try_from("@alice:example.com").unwrap();
        assert!(ac.check_sender(&user, &test_room_id()).is_ok());
    }

    #[test]
    fn allowlist_blocks_unlisted_user() {
        let mut config = AccessConfig::default();
        config.allow_from = vec!["@alice:example.com".to_string()];
        let ac = make_access_control(config);
        let blocked = OwnedUserId::try_from("@bob:example.com").unwrap();
        assert!(ac.check_sender(&blocked, &test_room_id()).is_err());
    }

    #[test]
    fn pairing_flow_works() {
        let config = AccessConfig::default(); // dm_policy: Pairing, empty allowFrom
        let ac = make_access_control(config);
        let user = OwnedUserId::try_from("@bob:example.com").unwrap();
        let room = test_room_id();

        let code = match ac.check_sender(&user, &room) {
            Err(AccessDenied::PairingRequired(c)) => c,
            other => panic!("Expected PairingRequired, got {other:?}"),
        };

        // Second check returns PairingRequired again (reply_count still under limit)
        match ac.check_sender(&user, &room) {
            Err(AccessDenied::PairingRequired(c)) => assert_eq!(c, code),
            other => panic!("Expected PairingRequired, got {other:?}"),
        }

        assert!(ac.approve_pairing(&user, "WRONG1").is_err());

        let returned_room = ac.approve_pairing(&user, &code).unwrap();
        assert_eq!(returned_room, room);

        assert!(ac.check_sender(&user, &room).is_ok());
    }

    #[test]
    fn disabled_policy_allows_everyone() {
        let mut config = AccessConfig::default();
        config.dm_policy = DmPolicy::Disabled;
        let ac = make_access_control(config);

        // Even unlisted users pass through when access control is disabled
        let unlisted = OwnedUserId::try_from("@stranger:example.com").unwrap();
        assert!(ac.check_sender(&unlisted, &test_room_id()).is_ok());

        let listed = OwnedUserId::try_from("@alice:example.com").unwrap();
        assert!(ac.check_sender(&listed, &test_room_id()).is_ok());
    }

    #[test]
    fn allowlist_policy_denies_without_pairing() {
        let mut config = AccessConfig::default();
        config.dm_policy = DmPolicy::Allowlist;
        let ac = make_access_control(config);
        let user = OwnedUserId::try_from("@bob:example.com").unwrap();

        match ac.check_sender(&user, &test_room_id()) {
            Err(AccessDenied::Denied) => {}
            other => panic!("Expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn max_pending_limit() {
        let config = AccessConfig::default();
        let ac = make_access_control(config);
        let room = test_room_id();

        for i in 0..MAX_PENDING {
            let user = OwnedUserId::try_from(format!("@user{i}:example.com")).unwrap();
            assert!(matches!(
                ac.check_sender(&user, &room),
                Err(AccessDenied::PairingRequired(_))
            ));
        }

        let extra = OwnedUserId::try_from("@extra:example.com").unwrap();
        match ac.check_sender(&extra, &room) {
            Err(AccessDenied::TooManyPending) => {}
            other => panic!("Expected TooManyPending, got {other:?}"),
        }
    }

    #[test]
    fn reply_rate_limiting() {
        let config = AccessConfig::default();
        let ac = make_access_control(config);
        let user = OwnedUserId::try_from("@bob:example.com").unwrap();
        let room = test_room_id();

        let _code = match ac.check_sender(&user, &room) {
            Err(AccessDenied::PairingRequired(c)) => c,
            other => panic!("Expected PairingRequired, got {other:?}"),
        };

        for _ in 0..MAX_REPLIES_PER_SENDER {
            ac.mark_pairing_reply_sent(&user);
        }

        match ac.check_sender(&user, &room) {
            Err(AccessDenied::PairingPending(_)) => {}
            other => panic!("Expected PairingPending, got {other:?}"),
        }
    }

    #[test]
    fn defaults_used_when_no_file() {
        let ac = make_access_control_no_file();
        // Default is pairing mode with empty allowFrom — all users get pairing required
        let user = OwnedUserId::try_from("@bob:example.com").unwrap();
        assert!(matches!(
            ac.check_sender(&user, &test_room_id()),
            Err(AccessDenied::PairingRequired(_))
        ));
    }

    #[test]
    fn delivery_settings_accessible() {
        let mut config = AccessConfig::default();
        config.ack_reaction = "✨".to_string();
        config.text_chunk_limit = 2000;
        config.chunk_mode = ChunkMode::Length;
        config.reply_to_mode = ReplyToMode::All;
        let ac = make_access_control(config);

        assert_eq!(ac.ack_reaction(), Some("✨".to_string()));
        assert_eq!(ac.text_chunk_limit(), 2000);
        assert_eq!(ac.chunk_mode(), ChunkMode::Length);
        assert_eq!(ac.reply_to_mode(), ReplyToMode::All);
    }

    #[test]
    fn group_mention_check() {
        let mut config = AccessConfig::default();
        config.groups.insert(
            "!room:example.com".to_string(),
            GroupConfig {
                require_mention: true,
                allow_from: vec![],
                mention_patterns: vec![],
            },
        );
        let ac = make_access_control(config);

        let room = OwnedRoomId::try_from("!room:example.com").unwrap();
        let other = OwnedRoomId::try_from("!other:example.com").unwrap();
        assert!(ac.requires_mention(&room));
        assert!(!ac.requires_mention(&other));
    }
}
