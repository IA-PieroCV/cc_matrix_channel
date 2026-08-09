use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use matrix_sdk::{
    Client, LoopCtrl, Room,
    config::{RequestConfig, SyncSettings},
    ruma::{
        OwnedDeviceId, OwnedRoomId, OwnedUserId,
        events::{
            AnySyncMessageLikeEvent, AnySyncTimelineEvent,
            room::{
                member::StrippedRoomMemberEvent,
                message::{MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent},
            },
        },
        serde::Raw,
    },
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::access::{AccessControl, AccessDenied};
use crate::config::Config;

/// Upper bound on how long the SDK may keep retrying a single failed request.
///
/// `RequestConfig::default()` leaves `retry_timeout` unset, which becomes
/// `ExponentialBackoff { max_elapsed_time: None }` — i.e. retry forever. A
/// transient 5xx on any request then never returns, and if that request was
/// awaited from an event handler it stalls the whole sync loop silently.
const REQUEST_RETRY_TIMEOUT: Duration = Duration::from_secs(60);

/// Outbound side effects (typing notices, reactions) are detached from the sync
/// loop, but still shouldn't pile up forever if the homeserver is unhealthy.
const SIDE_EFFECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for the MCP side to accept a notification before giving up
/// and logging. Bounded so a stalled consumer cannot wedge sync.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the sync loop may go without completing a single sync before the
/// watchdog declares it stalled and restarts it. The SDK's default sync timeout
/// is 30s, so a healthy loop checks in far more often than this.
const SYNC_STALL_TIMEOUT: Duration = Duration::from_secs(120);

/// How often the watchdog compares the sync heartbeat against the wall clock.
const WATCHDOG_TICK: Duration = Duration::from_secs(15);

/// Delay before re-entering the sync loop after it stalled or returned an error.
const SYNC_RESTART_DELAY: Duration = Duration::from_secs(5);

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Persisted session for restore across restarts.
#[derive(Serialize, Deserialize)]
struct SavedSession {
    user_id: String,
    device_id: String,
    access_token: String,
}

/// Metadata about an attachment in a Matrix message.
#[derive(Debug, Clone)]
pub struct AttachmentMeta {
    pub name: String,
    pub mime_type: String,
    pub size: u64,
    pub mxc_uri: String,
}

/// A permission verdict from Matrix to be relayed back to Claude Code.
#[derive(Debug, Clone)]
pub struct PermissionVerdict {
    pub request_id: String,
    pub behavior: String, // "allow" or "deny"
}

/// Parse a permission verdict from a message like "yes abcde" or "no abcde".
/// Strips Matrix reply fallback before parsing.
/// Returns None if the message doesn't match the expected format.
pub fn parse_permission_verdict(text: &str) -> Option<PermissionVerdict> {
    use matrix_sdk::ruma::events::room::message::sanitize;
    let text = sanitize::remove_plain_reply_fallback(text).trim();
    let (word, rest) = text.split_once(|c: char| c.is_whitespace())?;
    let behavior = match word.to_lowercase().as_str() {
        "y" | "yes" => "allow",
        "n" | "no" => "deny",
        _ => return None,
    };
    let id = rest.trim();
    // Must be exactly 5 lowercase letters from a-z minus 'l'
    if id.len() != 5 || !id.chars().all(|c| c.is_ascii_lowercase() && c != 'l') {
        return None;
    }
    Some(PermissionVerdict {
        request_id: id.to_string(),
        behavior: behavior.to_string(),
    })
}

/// A notification from Matrix to be forwarded to Claude Code via MCP.
#[derive(Debug, Clone)]
pub struct ChannelNotification {
    pub content: String,
    pub sender: String,
    pub sender_display_name: String,
    pub room_id: String,
    pub event_id: String,
    pub timestamp: String,
    pub attachments: Vec<AttachmentMeta>,
}

/// Configuration for constructing a [`MatrixBridge`].
pub struct MatrixBridgeConfig {
    pub notification_tx: mpsc::Sender<ChannelNotification>,
    pub permission_verdict_tx: mpsc::Sender<PermissionVerdict>,
    pub access_control: Arc<AccessControl>,
    pub known_rooms: Arc<parking_lot::Mutex<HashSet<OwnedRoomId>>>,
    /// Room that most recently passed the access check — where live status is posted.
    pub last_active_room: Arc<parking_lot::Mutex<Option<OwnedRoomId>>>,
    pub pending_permissions: Arc<parking_lot::Mutex<HashSet<String>>>,
    pub cancel: CancellationToken,
}

/// Shared state captured by the Matrix event handler closure.
#[derive(Clone)]
struct MessageHandlerCtx {
    tx: mpsc::Sender<ChannelNotification>,
    permission_verdict_tx: mpsc::Sender<PermissionVerdict>,
    pending_permissions: Arc<parking_lot::Mutex<HashSet<String>>>,
    access: Arc<AccessControl>,
    own_user_id: OwnedUserId,
    known_rooms: Arc<parking_lot::Mutex<HashSet<OwnedRoomId>>>,
    last_active_room: Arc<parking_lot::Mutex<Option<OwnedRoomId>>>,
    start_time: Instant,
}

/// Why the supervised sync task stopped running.
enum SyncOutcome {
    /// Shutdown was requested.
    Cancelled,
    /// The sync loop returned on its own (cleanly, with an error, or by panic).
    Returned(std::result::Result<matrix_sdk::Result<()>, tokio::task::JoinError>),
    /// The watchdog saw no completed sync for this many seconds.
    Stalled(u64),
}

pub struct MatrixBridge {
    client: Client,
    own_user_id: OwnedUserId,
    notification_tx: mpsc::Sender<ChannelNotification>,
    permission_verdict_tx: mpsc::Sender<PermissionVerdict>,
    access_control: Arc<AccessControl>,
    known_rooms: Arc<parking_lot::Mutex<HashSet<OwnedRoomId>>>,
    last_active_room: Arc<parking_lot::Mutex<Option<OwnedRoomId>>>,
    pending_permissions: Arc<parking_lot::Mutex<HashSet<String>>>,
    start_time: Instant,
    cancel: CancellationToken,
}

impl MatrixBridge {
    pub async fn new(config: &Config, bridge_config: MatrixBridgeConfig) -> Result<Self> {
        let MatrixBridgeConfig {
            notification_tx,
            permission_verdict_tx,
            access_control,
            known_rooms,
            last_active_room,
            pending_permissions,
            cancel,
        } = bridge_config;
        tokio::fs::create_dir_all(&config.store_path).await?;

        let homeserver_url = config
            .homeserver_url
            .as_ref()
            .context("MATRIX_HOMESERVER_URL is required")?;

        // Build client with E2EE settings.
        //
        // `retry_timeout` is set explicitly: without it the SDK retries transient
        // (5xx / 429) failures with unbounded exponential backoff and the request
        // future never resolves. See REQUEST_RETRY_TIMEOUT.
        let client = Client::builder()
            .homeserver_url(homeserver_url)
            .request_config(RequestConfig::default().retry_timeout(REQUEST_RETRY_TIMEOUT))
            .sqlite_store(&config.store_path, config.store_passphrase.as_deref())
            .with_encryption_settings(matrix_sdk::encryption::EncryptionSettings {
                auto_enable_cross_signing: true,
                auto_enable_backups: true,
                backup_download_strategy:
                    matrix_sdk::encryption::BackupDownloadStrategy::AfterDecryptionFailure,
            })
            .build()
            .await
            .context("Failed to build Matrix client")?;

        let session_file = session_file_path(&config.store_path);
        let user_id_str = config
            .user_id
            .as_ref()
            .context("MATRIX_USER_ID is required")?;
        let user_id = OwnedUserId::try_from(user_id_str.as_str())
            .context(format!("Invalid MATRIX_USER_ID: {user_id_str}"))?;

        // Three login paths: saved session > password login > access token fallback
        if session_file.exists() {
            // Path 1: Restore from saved session
            tracing::info!("Restoring session from {}", session_file.display());
            let saved = load_session(&session_file).await?;
            let session = matrix_sdk::matrix_auth::MatrixSession {
                meta: matrix_sdk::SessionMeta {
                    user_id: OwnedUserId::try_from(saved.user_id.as_str())?,
                    device_id: OwnedDeviceId::from(saved.device_id.as_str()),
                },
                tokens: matrix_sdk::matrix_auth::MatrixSessionTokens {
                    access_token: saved.access_token,
                    refresh_token: None,
                },
            };
            client
                .matrix_auth()
                .restore_session(session)
                .await
                .context("Failed to restore saved session")?;
            let resp = client
                .whoami()
                .await
                .context("Session invalid — delete the store and re-login")?;
            tracing::info!(
                "Session restored: {} (device {})",
                resp.user_id,
                saved.device_id
            );
        } else if let Some(ref password) = config.password {
            // Path 2: First-run login with password
            tracing::info!("First-run login for {}", user_id);
            let localpart = config
                .user_localpart()
                .context("MATRIX_USER_ID is required for login")?;

            let mut login_builder = client.matrix_auth().login_username(localpart, password);

            // Use configured device_id if provided, otherwise SDK generates one
            if let Some(ref device_id) = config.device_id {
                login_builder = login_builder.device_id(device_id);
            }

            login_builder
                .initial_device_display_name("cc_matrix_channel")
                .send()
                .await
                .context("Login failed — check username/password")?;

            // Wait for E2EE initialization (cross-signing bootstrap, key upload)
            tracing::info!("Waiting for E2EE initialization...");
            client
                .encryption()
                .wait_for_e2ee_initialization_tasks()
                .await;
            tracing::info!(
                "E2EE initialization complete — cross-signing bootstrapped, device keys uploaded"
            );

            // Save session for future restarts
            let session = client
                .matrix_auth()
                .session()
                .context("No session after login")?;
            let saved = SavedSession {
                user_id: session.meta.user_id.to_string(),
                device_id: session.meta.device_id.to_string(),
                access_token: session.tokens.access_token.clone(),
            };
            save_session(&session_file, &saved).await?;
            tracing::info!(
                "Session saved to {} (device {})",
                session_file.display(),
                saved.device_id
            );
        } else if let Some(ref access_token) = config.access_token {
            // Path 3: Access token fallback (no password, no saved session)
            tracing::warn!(
                "Using access token without password login — E2EE may not work for encrypted media. \
                 Set MATRIX_PASSWORD for full E2EE support."
            );
            let device_id = config.device_id.as_deref().unwrap_or("cc_matrix_channel");
            let session = matrix_sdk::matrix_auth::MatrixSession {
                meta: matrix_sdk::SessionMeta {
                    user_id: user_id.clone(),
                    device_id: OwnedDeviceId::from(device_id),
                },
                tokens: matrix_sdk::matrix_auth::MatrixSessionTokens {
                    access_token: access_token.clone(),
                    refresh_token: None,
                },
            };
            client
                .matrix_auth()
                .restore_session(session)
                .await
                .context("Failed to restore session from access token")?;
            client
                .whoami()
                .await
                .context("whoami failed — is the access token valid?")?;
        } else {
            bail!(
                "No authentication configured. Set MATRIX_PASSWORD for first-run E2EE setup, \
                 or MATRIX_ACCESS_TOKEN for token-based auth (limited E2EE)."
            );
        }

        let own_user_id = client
            .user_id()
            .context("No user_id after login")?
            .to_owned();
        tracing::info!("Bot identity: {own_user_id}");

        Ok(Self {
            client,
            own_user_id,
            notification_tx,
            permission_verdict_tx,
            access_control,
            pending_permissions,
            known_rooms,
            last_active_room,
            start_time: Instant::now(),
            cancel,
        })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub async fn run(&self) -> Result<()> {
        self.client.add_event_handler(Self::handle_invite);

        let ctx = MessageHandlerCtx {
            tx: self.notification_tx.clone(),
            permission_verdict_tx: self.permission_verdict_tx.clone(),
            pending_permissions: self.pending_permissions.clone(),
            access: self.access_control.clone(),
            own_user_id: self.own_user_id.clone(),
            known_rooms: self.known_rooms.clone(),
            last_active_room: self.last_active_room.clone(),
            start_time: self.start_time,
        };

        // Use Raw<AnySyncTimelineEvent> for manual deserialization — more robust than
        // typed OriginalSyncRoomMessageEvent which silently skips on deserialization failure
        // (known issue with encrypted media events in matrix-sdk v0.9)
        self.client
            .add_event_handler(move |raw: Raw<AnySyncTimelineEvent>, room: Room| {
                let ctx = ctx.clone();
                async move {
                    match raw.deserialize() {
                        Ok(AnySyncTimelineEvent::MessageLike(
                            AnySyncMessageLikeEvent::RoomMessage(msg),
                        )) => {
                            if let Some(original) = msg.as_original() {
                                Self::handle_message(original.clone(), room, ctx).await;
                            }
                        }
                        Ok(_) => {} // non-message timeline events
                        Err(e) => {
                            tracing::debug!("Timeline event deserialization skipped: {e}");
                        }
                    }
                }
            });

        tracing::info!(
            "Starting Matrix sync loop (stall watchdog: {}s)",
            SYNC_STALL_TIMEOUT.as_secs()
        );
        self.supervise_sync().await
    }

    /// Runs the sync loop under a liveness watchdog, restarting it if it stalls
    /// or fails.
    ///
    /// The sync loop runs in its own task specifically so a *wedged* sync can be
    /// aborted. A wedged sync returns no error and exits no loop — the process
    /// stays up and the MCP server keeps answering, so from the outside the
    /// bridge looks healthy while never delivering another message. The only
    /// reliable signal is the absence of completed syncs, which is what the
    /// heartbeat below measures.
    async fn supervise_sync(&self) -> Result<()> {
        let heartbeat = Arc::new(AtomicU64::new(now_unix()));
        let mut consecutive_failures: u32 = 0;

        loop {
            if self.cancel.is_cancelled() {
                break;
            }

            heartbeat.store(now_unix(), Ordering::Relaxed);

            let client = self.client.clone();
            let cancel = self.cancel.clone();
            let beat = heartbeat.clone();
            let mut sync_task = tokio::spawn(async move {
                client
                    .sync_with_callback(SyncSettings::default(), move |_response| {
                        let cancel = cancel.clone();
                        let beat = beat.clone();
                        async move {
                            beat.store(now_unix(), Ordering::Relaxed);
                            if cancel.is_cancelled() {
                                LoopCtrl::Break
                            } else {
                                LoopCtrl::Continue
                            }
                        }
                    })
                    .await
            });

            let outcome = loop {
                tokio::select! {
                    joined = &mut sync_task => break SyncOutcome::Returned(joined),
                    _ = self.cancel.cancelled() => {
                        sync_task.abort();
                        break SyncOutcome::Cancelled;
                    }
                    _ = tokio::time::sleep(WATCHDOG_TICK) => {
                        let idle = now_unix().saturating_sub(heartbeat.load(Ordering::Relaxed));
                        if idle >= SYNC_STALL_TIMEOUT.as_secs() {
                            sync_task.abort();
                            break SyncOutcome::Stalled(idle);
                        }
                    }
                }
            };

            match outcome {
                SyncOutcome::Cancelled => break,
                SyncOutcome::Returned(Ok(Ok(()))) => {
                    tracing::info!("Matrix sync loop stopped");
                    break;
                }
                SyncOutcome::Stalled(idle) => {
                    consecutive_failures += 1;
                    tracing::error!(
                        "Matrix sync stalled — no sync completed for {idle}s (failure #{consecutive_failures}); restarting sync loop"
                    );
                }
                SyncOutcome::Returned(Ok(Err(e))) => {
                    consecutive_failures += 1;
                    tracing::error!(
                        "Matrix sync loop failed (failure #{consecutive_failures}): {e:?}; restarting"
                    );
                }
                SyncOutcome::Returned(Err(e)) => {
                    consecutive_failures += 1;
                    tracing::error!(
                        "Matrix sync task terminated unexpectedly (failure #{consecutive_failures}): {e}; restarting"
                    );
                }
            }

            // Escalating backoff so a hard, permanent failure (bad token, for
            // instance) doesn't turn into a hot restart loop.
            let backoff = SYNC_RESTART_DELAY * consecutive_failures.min(12);
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = self.cancel.cancelled() => break,
            }
        }

        Ok(())
    }

    async fn handle_invite(event: StrippedRoomMemberEvent, room: Room) {
        let client = room.client();
        let own_id = client.user_id();
        if own_id.is_some_and(|id| *id == *event.state_key) {
            let room_id = room.room_id().to_owned();
            tracing::info!("Received invite to room {room_id}, joining");
            // Detached: joining is a network round trip and must not run on the
            // sync loop (see spawn_room_send).
            tokio::spawn(async move {
                match tokio::time::timeout(SIDE_EFFECT_TIMEOUT, room.join()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::error!("Failed to join room {room_id}: {e}"),
                    Err(_) => tracing::error!("Timed out joining room {room_id}"),
                }
            });
        }
    }

    async fn handle_message(
        event: OriginalSyncRoomMessageEvent,
        room: Room,
        ctx: MessageHandlerCtx,
    ) {
        let MessageHandlerCtx {
            tx,
            permission_verdict_tx,
            pending_permissions,
            access,
            own_user_id,
            known_rooms,
            last_active_room,
            start_time,
        } = ctx;

        if event.sender == own_user_id {
            return;
        }

        // Log message type for debugging
        tracing::debug!(
            "Received message from {} in {}: type={:?}",
            event.sender,
            room.room_id(),
            std::mem::discriminant(&event.content.msgtype)
        );

        let (text, attachments) = match &event.content.msgtype {
            MessageType::Text(t) => {
                use matrix_sdk::ruma::events::room::message::sanitize;
                let body = sanitize::remove_plain_reply_fallback(&t.body).to_string();
                (body, vec![])
            }
            MessageType::Image(img) => {
                let meta = AttachmentMeta {
                    name: img.body.clone(),
                    mime_type: img
                        .info
                        .as_ref()
                        .and_then(|i| i.mimetype.as_deref())
                        .unwrap_or("image/unknown")
                        .to_string(),
                    size: img
                        .info
                        .as_ref()
                        .and_then(|i| i.size)
                        .map(u64::from)
                        .unwrap_or(0),
                    mxc_uri: extract_mxc_uri(&img.source),
                };
                (format!("[Image: {}]", img.body), vec![meta])
            }
            MessageType::File(file) => {
                let meta = AttachmentMeta {
                    name: file.body.clone(),
                    mime_type: file
                        .info
                        .as_ref()
                        .and_then(|i| i.mimetype.as_deref())
                        .unwrap_or("application/octet-stream")
                        .to_string(),
                    size: file
                        .info
                        .as_ref()
                        .and_then(|i| i.size)
                        .map(u64::from)
                        .unwrap_or(0),
                    mxc_uri: extract_mxc_uri(&file.source),
                };
                (format!("[File: {}]", file.body), vec![meta])
            }
            MessageType::Audio(audio) => {
                let meta = AttachmentMeta {
                    name: audio.body.clone(),
                    mime_type: audio
                        .info
                        .as_ref()
                        .and_then(|i| i.mimetype.as_deref())
                        .unwrap_or("audio/unknown")
                        .to_string(),
                    size: audio
                        .info
                        .as_ref()
                        .and_then(|i| i.size)
                        .map(u64::from)
                        .unwrap_or(0),
                    mxc_uri: extract_mxc_uri(&audio.source),
                };
                (format!("[Audio: {}]", audio.body), vec![meta])
            }
            MessageType::Video(video) => {
                let meta = AttachmentMeta {
                    name: video.body.clone(),
                    mime_type: video
                        .info
                        .as_ref()
                        .and_then(|i| i.mimetype.as_deref())
                        .unwrap_or("video/unknown")
                        .to_string(),
                    size: video
                        .info
                        .as_ref()
                        .and_then(|i| i.size)
                        .map(u64::from)
                        .unwrap_or(0),
                    mxc_uri: extract_mxc_uri(&video.source),
                };
                (format!("[Video: {}]", video.body), vec![meta])
            }
            _ => return,
        };

        // Handle bot commands before access check
        if text.starts_with('/') {
            Self::handle_bot_command(&text, &room, &own_user_id, start_time).await;
            return;
        }

        let sender_id = event.sender.clone();

        // Mention-only room check (from access.json groups config)
        if access.requires_mention(room.room_id()) {
            let own_id_str = own_user_id.as_str();
            if !text.contains(own_id_str) {
                // Bounded: this runs on the sync loop, so it must not be able to
                // hang (see spawn_room_send).
                let own_name = match tokio::time::timeout(
                    SIDE_EFFECT_TIMEOUT,
                    room.client().account().get_display_name(),
                )
                .await
                {
                    Ok(Ok(name)) => name,
                    Ok(Err(e)) => {
                        tracing::warn!("Failed to fetch own display name: {e}");
                        None
                    }
                    Err(_) => {
                        tracing::warn!("Timed out fetching own display name");
                        None
                    }
                };
                let mentioned = own_name
                    .as_ref()
                    .is_some_and(|name| text.contains(name.as_str()));
                if !mentioned {
                    // Check custom mention patterns from config (regex, case-insensitive)
                    let patterns = access.mention_patterns(room.room_id());
                    let pattern_matched =
                        patterns.iter().any(|pat| {
                            match regex::RegexBuilder::new(pat).case_insensitive(true).build() {
                                Ok(re) => re.is_match(&text),
                                Err(e) => {
                                    tracing::warn!("Invalid mention pattern '{pat}': {e}");
                                    false
                                }
                            }
                        });
                    if !pattern_matched {
                        return;
                    }
                }
            }
        }

        // Check access
        let current_room_id = room.room_id().to_owned();
        match access.check_sender(&sender_id, &current_room_id) {
            Ok(()) => {
                // Remember this room, and remember it is the most recent one — live status
                // posts there. Persisted so a bridge restart does not go silent.
                let newly_known = known_rooms.lock().insert(current_room_id.clone());
                let room_changed = {
                    let mut last = last_active_room.lock();
                    let changed = last.as_ref() != Some(&current_room_id);
                    *last = Some(current_room_id.clone());
                    changed
                };
                if newly_known || room_changed {
                    let rooms = known_rooms.lock().clone();
                    crate::rooms::save(&crate::rooms::store_path(), &rooms, Some(&current_room_id));
                }

                // Permission verdict interception — only for pending requests from approved users
                if let Some(verdict) = parse_permission_verdict(&text)
                    && pending_permissions.lock().contains(&verdict.request_id)
                {
                    let _ = permission_verdict_tx.send(verdict.clone()).await;
                    let emoji = if verdict.behavior == "allow" {
                        "✅"
                    } else {
                        "❌"
                    };
                    let annotation = matrix_sdk::ruma::events::relation::Annotation::new(
                        event.event_id.clone(),
                        emoji.to_string(),
                    );
                    let reaction =
                        matrix_sdk::ruma::events::reaction::ReactionEventContent::new(annotation);
                    spawn_room_send("verdict reaction", room.clone(), reaction);
                    return;
                }

                // Typing indicator — only for text messages (media won't get a Claude response)
                if attachments.is_empty() {
                    let typing_room = room.clone();
                    let typing_room_id = room.room_id().to_owned();
                    tokio::spawn(async move {
                        match tokio::time::timeout(
                            SIDE_EFFECT_TIMEOUT,
                            typing_room.typing_notice(true),
                        )
                        .await
                        {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => {
                                tracing::warn!(
                                    "Failed to send typing notice in {typing_room_id}: {e}"
                                )
                            }
                            Err(_) => {
                                tracing::warn!(
                                    "Timed out sending typing notice in {typing_room_id}"
                                )
                            }
                        }
                    });
                }

                let display_name = room
                    .get_member_no_sync(&sender_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|m| m.name().to_string())
                    .unwrap_or_else(|| sender_id.to_string());

                let timestamp = event
                    .origin_server_ts
                    .to_system_time()
                    .map(humanize_timestamp)
                    .unwrap_or_default();

                let notif = ChannelNotification {
                    content: text,
                    sender: sender_id.to_string(),
                    sender_display_name: display_name,
                    room_id: room.room_id().to_string(),
                    event_id: event.event_id.to_string(),
                    timestamp,
                    attachments,
                };
                // Bounded: a stalled MCP consumer must not be able to wedge sync.
                let queued = match tokio::time::timeout(NOTIFY_TIMEOUT, tx.send(notif)).await {
                    Ok(Ok(())) => true,
                    Ok(Err(_)) => {
                        tracing::error!("Failed to send notification to MCP");
                        false
                    }
                    Err(_) => {
                        tracing::error!(
                            "Timed out after {}s queueing notification to MCP — consumer stalled",
                            NOTIFY_TIMEOUT.as_secs()
                        );
                        false
                    }
                };

                if queued {
                    // Ack reaction — confirms message was received
                    if let Some(emoji) = access.ack_reaction() {
                        let annotation = matrix_sdk::ruma::events::relation::Annotation::new(
                            event.event_id.clone(),
                            emoji,
                        );
                        let reaction =
                            matrix_sdk::ruma::events::reaction::ReactionEventContent::new(
                                annotation,
                            );
                        spawn_room_send("ack reaction", room.clone(), reaction);
                    }
                }
            }
            Err(AccessDenied::PairingRequired(code)) => {
                let msg = format!(
                    "Pairing required. Ask the Claude Code operator to approve you with code: {code}"
                );
                let content = RoomMessageEventContent::text_plain(&msg);
                spawn_room_send("pairing message", room.clone(), content);
                access.mark_pairing_reply_sent(&sender_id);
            }
            Err(
                AccessDenied::PairingPending(_)
                | AccessDenied::TooManyPending
                | AccessDenied::Denied,
            ) => {
                // Silent drop
            }
        }
    }

    async fn handle_bot_command(
        text: &str,
        room: &Room,
        _own_user_id: &OwnedUserId,
        start_time: Instant,
    ) {
        let cmd = text.split_whitespace().next().unwrap_or("");
        let response = match cmd {
            "/start" => Some(
                "I'm a Claude Code bridge bot. Messages sent here are forwarded to your active Claude Code session, and Claude's replies appear back in this chat.".to_string()
            ),
            "/help" => Some(
                "Available commands:\n\
                 /start — What this bot does\n\
                 /help — Show this message\n\
                 /status — Bot status\n\n\
                 Send any other message and it will be forwarded to Claude Code (if you have access)."
                    .to_string(),
            ),
            "/status" => {
                let uptime = start_time.elapsed();
                let hours = uptime.as_secs() / 3600;
                let minutes = (uptime.as_secs() % 3600) / 60;
                // Answered by the bridge itself and never forwarded to Claude, so this
                // keeps working when the agent is the thing that is wedged.
                let agent = crate::status::read_status(crate::status::stall_threshold());
                Some(format!(
                    "Bridge:   online, uptime {hours}h {minutes}m\n{}",
                    agent.render()
                ))
            }
            _ => None,
        };

        if let Some(msg) = response {
            let content = RoomMessageEventContent::text_plain(&msg);
            spawn_room_send("bot command response", room.clone(), content);
        }
    }

    #[allow(dead_code)]
    pub async fn send_message(&self, room_id: &OwnedRoomId, text: &str) -> Result<()> {
        let room = self
            .client
            .get_room(room_id)
            .context(format!("Room not found: {room_id}"))?;
        let content = RoomMessageEventContent::text_markdown(text);
        room.send(content).await?;
        Ok(())
    }
}

// --- Outbound side effects ---

/// Send a room event without blocking the sync loop.
///
/// matrix-sdk awaits event-handler futures inline while processing a sync
/// response (`Client::call_event_handlers`), so anything awaited inside a
/// handler delays every subsequent message — and if it never returns, sync stops
/// forever with no error and no exit. Reactions, typing notices and courtesy
/// replies are all fire-and-forget, so they belong on their own task with a
/// hard timeout.
fn spawn_room_send<C>(label: &'static str, room: Room, content: C)
where
    C: matrix_sdk::ruma::events::MessageLikeEventContent + Send + 'static,
{
    let room_id = room.room_id().to_owned();
    tokio::spawn(async move {
        let send = async { room.send(content).await };
        match tokio::time::timeout(SIDE_EFFECT_TIMEOUT, send).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::warn!("Failed to send {label} in {room_id}: {e}"),
            Err(_) => tracing::warn!("Timed out sending {label} in {room_id}"),
        }
    });
}

// --- Session persistence ---

fn session_file_path(store_path: &str) -> PathBuf {
    PathBuf::from(store_path).join("session.json")
}

async fn load_session(path: &PathBuf) -> Result<SavedSession> {
    let data = tokio::fs::read_to_string(path)
        .await
        .context("Failed to read session file")?;
    serde_json::from_str(&data).context("Failed to parse session file")
}

async fn save_session(path: &PathBuf, session: &SavedSession) -> Result<()> {
    let data = serde_json::to_string_pretty(session)?;
    tokio::fs::write(path, &data)
        .await
        .context("Failed to write session file")?;
    // Restrict permissions — session contains access_token
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .await
            .context("Failed to set session file permissions")?;
    }
    Ok(())
}

// --- Helpers ---

pub fn extract_mxc_uri(source: &matrix_sdk::ruma::events::room::MediaSource) -> String {
    match source {
        matrix_sdk::ruma::events::room::MediaSource::Plain(uri) => uri.to_string(),
        matrix_sdk::ruma::events::room::MediaSource::Encrypted(encrypted) => {
            encrypted.url.to_string()
        }
    }
}

fn humanize_timestamp(time: std::time::SystemTime) -> String {
    let duration = time
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();
    let days_since_epoch = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    let mut y = 1970i64;
    let mut remaining = days_since_epoch as i64;
    loop {
        let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let days_in_months: [i64; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0;
    for (i, &dim) in days_in_months.iter().enumerate() {
        if remaining < dim {
            m = i + 1;
            break;
        }
        remaining -= dim;
    }
    let d = remaining + 1;

    format!("{y:04}-{m:02}-{d:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}
