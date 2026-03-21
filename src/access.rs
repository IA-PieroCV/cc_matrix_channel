use matrix_sdk::ruma::{OwnedRoomId, OwnedUserId};
use parking_lot::Mutex;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const PAIRING_EXPIRY: Duration = Duration::from_secs(3600); // 1 hour
const MAX_PENDING: usize = 3;
const MAX_REPLIES_PER_SENDER: u8 = 2;

#[derive(Debug, Clone)]
pub enum AllowPolicy {
    All,
    List(HashSet<OwnedUserId>),
}

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
    InvalidCode,
    Expired,
}

impl fmt::Display for PairingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoPendingPairing => write!(f, "No pending pairing for this user"),
            Self::InvalidCode => write!(f, "Invalid pairing code"),
            Self::Expired => write!(f, "Pairing code has expired"),
        }
    }
}

struct PendingPairing {
    code: String,
    created_at: Instant,
    reply_count: u8,
    room_id: OwnedRoomId,
}

struct AccessState {
    pending: HashMap<OwnedUserId, PendingPairing>,
    approved: HashSet<OwnedUserId>,
}

#[derive(Serialize, Deserialize)]
struct PersistedAccessState {
    approved: Vec<String>,
}

pub struct AccessControl {
    policy: AllowPolicy,
    static_mode: bool,
    state: Mutex<AccessState>,
    state_file: Option<PathBuf>,
}

impl AccessControl {
    pub fn new(policy: AllowPolicy, static_mode: bool, store_path: Option<&str>) -> Self {
        let state_file = store_path.map(|p| PathBuf::from(p).join("access_state.json"));

        // Load existing approved users from file
        let approved = state_file
            .as_ref()
            .and_then(|path| {
                std::fs::read_to_string(path)
                    .ok()
                    .and_then(|data| serde_json::from_str::<PersistedAccessState>(&data).ok())
            })
            .map(|persisted| {
                persisted
                    .approved
                    .iter()
                    .filter_map(|s| OwnedUserId::try_from(s.as_str()).ok())
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default();

        if !approved.is_empty() {
            tracing::info!("Loaded {} approved users from state file", approved.len());
        }

        Self {
            policy,
            static_mode,
            state: Mutex::new(AccessState {
                pending: HashMap::new(),
                approved,
            }),
            state_file,
        }
    }

    pub fn check_sender(
        &self,
        user_id: &OwnedUserId,
        room_id: &OwnedRoomId,
    ) -> Result<(), AccessDenied> {
        match &self.policy {
            AllowPolicy::All => return Ok(()),
            AllowPolicy::List(allowed) => {
                if allowed.contains(user_id) {
                    return Ok(());
                }
            }
        }

        let mut state = self.state.lock();

        if state.approved.contains(user_id) {
            return Ok(());
        }

        if self.static_mode {
            return Err(AccessDenied::Denied);
        }

        // Check for existing pending pairing
        if let Some(pending) = state.pending.get_mut(user_id) {
            if pending.created_at.elapsed() > PAIRING_EXPIRY {
                state.pending.remove(user_id);
                // Fall through to generate new code
            } else if pending.reply_count >= MAX_REPLIES_PER_SENDER {
                // Rate limited — silently drop (PairingPending but no more replies)
                return Err(AccessDenied::PairingPending(pending.code.clone()));
            } else {
                // Still within reply limit — return code for matrix handler to reply
                return Err(AccessDenied::PairingRequired(pending.code.clone()));
            }
        }

        if state.pending.len() >= MAX_PENDING {
            return Err(AccessDenied::TooManyPending);
        }

        let code = generate_code();
        state.pending.insert(
            user_id.clone(),
            PendingPairing {
                code: code.clone(),
                created_at: Instant::now(),
                reply_count: 0,
                room_id: room_id.clone(),
            },
        );
        Err(AccessDenied::PairingRequired(code))
    }

    pub fn mark_pairing_reply_sent(&self, user_id: &OwnedUserId) {
        let mut state = self.state.lock();
        if let Some(pending) = state.pending.get_mut(user_id) {
            pending.reply_count = pending.reply_count.saturating_add(1);
        }
    }

    /// Returns the room_id where the pairing was requested on success.
    pub fn approve_pairing(
        &self,
        user_id: &OwnedUserId,
        code: &str,
    ) -> Result<OwnedRoomId, PairingError> {
        let mut state = self.state.lock();
        match state.pending.get(user_id) {
            None => Err(PairingError::NoPendingPairing),
            Some(pending) if pending.created_at.elapsed() > PAIRING_EXPIRY => {
                state.pending.remove(user_id);
                Err(PairingError::Expired)
            }
            Some(pending)
                if !constant_time_eq::constant_time_eq(
                    pending.code.as_bytes(),
                    code.as_bytes(),
                ) =>
            {
                Err(PairingError::InvalidCode)
            }
            Some(_) => {
                let room_id = state.pending.get(user_id).unwrap().room_id.clone();
                state.pending.remove(user_id);
                state.approved.insert(user_id.clone());
                self.save_state(&state);
                tracing::info!("Approved pairing for {user_id}");
                Ok(room_id)
            }
        }
    }

    /// Persist approved users to disk (atomic write).
    fn save_state(&self, state: &AccessState) {
        let Some(ref path) = self.state_file else {
            return;
        };
        let persisted = PersistedAccessState {
            approved: state.approved.iter().map(|u| u.to_string()).collect(),
        };
        let data = match serde_json::to_string_pretty(&persisted) {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("Failed to serialize access state: {e}");
                return;
            }
        };
        let tmp_path = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp_path, &data) {
            tracing::error!("Failed to write access state: {e}");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp_path, path) {
            tracing::error!("Failed to rename access state file: {e}");
        }
    }
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

    #[test]
    fn allow_all_passes_everyone() {
        let ac = AccessControl::new(AllowPolicy::All, false, None);
        let user = OwnedUserId::try_from("@alice:example.com").unwrap();
        assert!(ac.check_sender(&user, &test_room_id()).is_ok());
    }

    #[test]
    fn allowlist_passes_listed_user() {
        let user = OwnedUserId::try_from("@alice:example.com").unwrap();
        let mut set = HashSet::new();
        set.insert(user.clone());
        let ac = AccessControl::new(AllowPolicy::List(set), false, None);
        assert!(ac.check_sender(&user, &test_room_id()).is_ok());
    }

    #[test]
    fn allowlist_blocks_unlisted_user() {
        let allowed = OwnedUserId::try_from("@alice:example.com").unwrap();
        let blocked = OwnedUserId::try_from("@bob:example.com").unwrap();
        let mut set = HashSet::new();
        set.insert(allowed);
        let ac = AccessControl::new(AllowPolicy::List(set), false, None);
        assert!(ac.check_sender(&blocked, &test_room_id()).is_err());
    }

    #[test]
    fn pairing_flow_works() {
        let user = OwnedUserId::try_from("@bob:example.com").unwrap();
        let room = test_room_id();
        let ac = AccessControl::new(AllowPolicy::List(HashSet::new()), false, None);

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
    fn static_mode_denies_without_pairing() {
        let user = OwnedUserId::try_from("@bob:example.com").unwrap();
        let ac = AccessControl::new(AllowPolicy::List(HashSet::new()), true, None);

        match ac.check_sender(&user, &test_room_id()) {
            Err(AccessDenied::Denied) => {}
            other => panic!("Expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn max_pending_limit() {
        let ac = AccessControl::new(AllowPolicy::List(HashSet::new()), false, None);
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
        let user = OwnedUserId::try_from("@bob:example.com").unwrap();
        let room = test_room_id();
        let ac = AccessControl::new(AllowPolicy::List(HashSet::new()), false, None);

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
}
