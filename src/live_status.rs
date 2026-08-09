//! Live agent-status message: the draft/edit pattern.
//!
//! One message per working spell, edited in place as the agent works, following the
//! OpenClaw/Hermes "draft that keeps updating" behaviour. The terminal state is edited
//! into that same message, so a normal turn leaves exactly one status message behind
//! rather than a running commentary.
//!
//! Matrix edits do not raise push notifications, which is the right default here: when the
//! agent finishes it sends its actual reply, and that reply is its own message and pings
//! the user by itself — a separate "done" would double-ping every single turn.
//!
//! The exception is [`AgentState::needs_alert`] (stalled, dead). Nothing else will ever
//! arrive in those cases, so they additionally get a short **new** message. Otherwise the
//! user's device stays silent exactly when the agent is wedged and they have walked away.
//!
//! Read [`crate::status`] for how the state itself is derived. This module only decides
//! what to put on the wire and when.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use matrix_sdk::Client;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};
use tokio_util::sync::CancellationToken;

use crate::status::{AgentState, AgentStatus, read_status, stall_threshold};

/// Poll cadence. Also the floor on edit frequency: every edit is a `room.send`, and
/// homeservers rate-limit sends, so this must stay comfortably above one-per-second.
const TICK: Duration = Duration::from_secs(3);

/// Matches the cap `mcp::edit_message` enforces on message bodies.
const MAX_TOTAL_LENGTH: usize = 50_000;

/// How long the agent must be working before a status message is worth posting at all.
///
/// Most turns finish quickly, and for those the agent's own reply is the only message the
/// room needs — a status draft for a six-second turn is pure clutter. Only once a turn
/// runs long enough that the user might reasonably wonder what is happening does the draft
/// appear. Tunable via `CC_MATRIX_DRAFT_DELAY_SECS`.
const DEFAULT_DRAFT_DELAY_SECS: u64 = 20;

fn draft_delay() -> Duration {
    let secs = std::env::var("CC_MATRIX_DRAFT_DELAY_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_DRAFT_DELAY_SECS);
    Duration::from_secs(secs)
}

/// Tracks the draft message for the current working spell.
struct Draft {
    room_id: OwnedRoomId,
    event_id: OwnedEventId,
    /// Last body actually sent, so an unchanged render does not burn homeserver quota.
    rendered: String,
}

/// The decision the state machine makes on each tick, kept separate from the Matrix calls
/// so it can be unit-tested without a homeserver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// Nothing changed worth spending a send on.
    Nothing,
    /// Start a new working spell: send a message and remember its event id.
    StartDraft,
    /// Update the existing draft in place.
    EditDraft,
    /// End the spell: edit the draft to its final state and close it. A separate alert
    /// message follows only for states that would otherwise be silent — see
    /// [`AgentState::needs_alert`].
    CloseDraft,
    /// Something went wrong before any draft existed — a stall or death inside the
    /// draft delay. Send the alert on its own, because a silent failure with no message
    /// at all is the one outcome this whole feature exists to prevent.
    AlertOnly,
}

/// Decide what to do this tick.
///
/// `previous` is the state observed last tick; `has_draft` is whether a draft message is
/// currently open; `changed` is whether the rendered body differs from what was last sent;
/// `working_for` is how long the agent has been continuously working, or `None` if it is
/// not working.
fn decide(
    previous: Option<AgentState>,
    current: AgentState,
    has_draft: bool,
    changed: bool,
    working_for: Option<Duration>,
    draft_delay: Duration,
) -> Action {
    match current {
        // "No information" must never be broadcast as if it were a result.
        AgentState::Unknown => Action::Nothing,

        AgentState::Working => {
            if has_draft {
                // Suppress no-op edits: re-rendering an identical body every tick would
                // spend homeserver quota for nothing.
                if changed {
                    Action::EditDraft
                } else {
                    Action::Nothing
                }
            } else if working_for.is_some_and(|d| d >= draft_delay) {
                Action::StartDraft
            } else {
                // Still inside the delay — a quick turn should leave no status behind.
                Action::Nothing
            }
        }

        // Debounce: act once on entering the state, not every tick it persists.
        s if s.is_terminal() => {
            if previous == Some(s) {
                Action::Nothing
            } else if has_draft {
                // Only close a spell we actually opened — otherwise a bridge starting up
                // beside an idle agent would declare "waiting for you" unprompted.
                Action::CloseDraft
            } else if s.needs_alert() {
                // No draft, because the turn was short — but a short turn that ends in a
                // stall or a dead process still has to be reported.
                Action::AlertOnly
            } else {
                // A quick turn that simply finished. The agent's own reply is the message.
                Action::Nothing
            }
        }

        _ => Action::Nothing,
    }
}

fn render_working(status: &AgentStatus) -> String {
    let mut body = format!("⏳ **Claude is {}**", status.state);
    let detail = status.render();
    // Drop the leading "Agent:" line — the heading above already says it.
    if let Some(rest) = detail.split_once('\n').map(|(_, r)| r) {
        body.push('\n');
        body.push_str(rest);
    }
    truncate(body)
}

fn render_terminal(status: &AgentStatus) -> String {
    let icon = match status.state {
        AgentState::Stalled => "⚠️",
        AgentState::Dead => "❌",
        _ => "✅",
    };
    let mut body = format!("{icon} **Claude is {}**", status.state);
    if let Some(elapsed) = status.turn_elapsed {
        body.push_str(&format!(
            "\nTurn ran for {}",
            crate::status::format_duration(elapsed)
        ));
    }
    if let Some(age) = status.last_activity_age
        && matches!(status.state, AgentState::Stalled)
    {
        body.push_str(&format!(
            "\nNo activity for {}",
            crate::status::format_duration(age)
        ));
    }
    truncate(body)
}

/// Short alert body, sent as a *new* message so it actually push-notifies.
///
/// Deliberately terse: the edited draft above it already carries the detail, and this
/// exists to make a phone buzz, not to be read in full on a lock screen.
fn render_alert(status: &AgentStatus) -> String {
    let body = match status.state {
        AgentState::Dead => "❌ **Claude is not running** — the session ended".to_string(),
        _ => match status.last_activity_age {
            Some(age) => format!(
                "⚠️ **Claude looks stuck** — no activity for {}",
                crate::status::format_duration(age)
            ),
            None => "⚠️ **Claude looks stuck**".to_string(),
        },
    };
    truncate(body)
}

fn truncate(mut s: String) -> String {
    if s.len() > MAX_TOTAL_LENGTH {
        s.truncate(MAX_TOTAL_LENGTH);
    }
    s
}

/// Pick the room to post status into: the one that most recently talked to the bridge.
///
/// Falls back to any known room only when there is no recorded last-active room — and
/// that fallback is genuinely arbitrary, because `known_rooms` is a `HashSet` with no
/// ordering. `last_active_room` exists precisely so the normal path is deterministic.
///
/// Both are populated by inbound traffic, so status can only ever go to a room that has
/// already talked to the bridge — the same containment property `check_outbound_gate`
/// gives the MCP tools.
fn target_room(
    known_rooms: &Arc<parking_lot::Mutex<HashSet<OwnedRoomId>>>,
    last_active_room: &Arc<parking_lot::Mutex<Option<OwnedRoomId>>>,
) -> Option<OwnedRoomId> {
    if let Some(room) = last_active_room.lock().clone() {
        return Some(room);
    }
    known_rooms.lock().iter().next().cloned()
}

/// Post a new status message and return its event id.
///
/// Split out of the loop so the Matrix write path can be exercised against a real
/// homeserver by `live_draft_cycle_against_real_homeserver` below.
async fn send_status(client: &Client, room_id: &OwnedRoomId, body: &str) -> Option<OwnedEventId> {
    let room = client.get_room(room_id)?;
    match room
        .send(RoomMessageEventContent::text_markdown(body))
        .await
    {
        Ok(resp) => Some(resp.event_id),
        Err(e) => {
            tracing::warn!("Failed to send status message: {e}");
            None
        }
    }
}

/// Edit an existing status message in place. Returns whether the edit landed.
async fn edit_status(
    client: &Client,
    room_id: &OwnedRoomId,
    event_id: &OwnedEventId,
    body: &str,
) -> bool {
    let Some(room) = client.get_room(room_id) else {
        return false;
    };
    let content = RoomMessageEventContent::text_markdown(body);
    let edited = match room
        .make_edit_event(
            event_id,
            matrix_sdk::room::edit::EditedContent::RoomMessage(content.into()),
        )
        .await
    {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("Failed to build status edit: {e}");
            return false;
        }
    };
    match room.send(edited).await {
        Ok(_) => true,
        Err(e) => {
            // Caller keeps the draft open and retries next tick — a transient send
            // failure should not orphan the message.
            tracing::warn!("Failed to send status edit: {e}");
            false
        }
    }
}

/// Spawn the live-status loop. Returns immediately.
pub fn spawn(
    client: Arc<Client>,
    known_rooms: Arc<parking_lot::Mutex<HashSet<OwnedRoomId>>>,
    last_active_room: Arc<parking_lot::Mutex<Option<OwnedRoomId>>>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let threshold = stall_threshold();
        let delay = draft_delay();
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut previous: Option<AgentState> = None;
        let mut draft: Option<Draft> = None;
        // Measured locally rather than from `turn_elapsed`, which is None once the opening
        // prompt scrolls out of the transcript tail — exactly on the long turns that need
        // a draft most.
        let mut working_since: Option<std::time::Instant> = None;

        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = cancel.cancelled() => break,
            }

            let status = read_status(threshold);

            if matches!(status.state, AgentState::Working) {
                working_since.get_or_insert_with(std::time::Instant::now);
            } else {
                working_since = None;
            }
            let working_for = working_since.map(|t| t.elapsed());
            let body = if matches!(status.state, AgentState::Working) {
                render_working(&status)
            } else {
                render_terminal(&status)
            };
            let changed = draft.as_ref().is_none_or(|d| d.rendered != body);

            match decide(
                previous,
                status.state,
                draft.is_some(),
                changed,
                working_for,
                delay,
            ) {
                Action::Nothing => {}

                Action::StartDraft => {
                    let Some(room_id) = target_room(&known_rooms, &last_active_room) else {
                        // No room has talked to us yet; nothing to update.
                        previous = Some(status.state);
                        continue;
                    };
                    if let Some(event_id) = send_status(&client, &room_id, &body).await {
                        draft = Some(Draft {
                            room_id,
                            event_id,
                            rendered: body,
                        });
                    }
                }

                Action::EditDraft => {
                    if let Some(d) = draft.as_mut()
                        && edit_status(&client, &d.room_id, &d.event_id, &body).await
                    {
                        d.rendered = body;
                    }
                }

                Action::CloseDraft => {
                    if let Some(d) = draft.take() {
                        // Fold the final state into the draft, so a normal turn leaves one
                        // status message rather than a running commentary.
                        edit_status(&client, &d.room_id, &d.event_id, &body).await;

                        // That edit is silent. For states where nothing else will ever
                        // arrive, follow it with a short new message that actually pings.
                        if status.state.needs_alert() {
                            send_status(&client, &d.room_id, &render_alert(&status)).await;
                        }
                    }
                }

                Action::AlertOnly => {
                    if let Some(room_id) = target_room(&known_rooms, &last_active_room) {
                        send_status(&client, &room_id, &render_alert(&status)).await;
                    }
                }
            }

            previous = Some(status.state);
        }

        tracing::debug!("Live status loop stopped");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const DELAY: Duration = Duration::from_secs(20);
    /// Comfortably past `DELAY` — "this turn has been running a while".
    const LONG: Duration = Duration::from_secs(60);
    /// Comfortably inside `DELAY` — "this turn just started".
    const SHORT: Duration = Duration::from_secs(2);

    fn status(state: AgentState) -> AgentStatus {
        AgentStatus {
            state,
            last_activity_age: Some(Duration::from_secs(4)),
            last_tool: Some("Bash".to_string()),
            turn_elapsed: Some(Duration::from_secs(200)),
        }
    }

    #[test]
    fn first_working_tick_opens_a_draft() {
        assert_eq!(
            decide(None, AgentState::Working, false, true, Some(LONG), DELAY),
            Action::StartDraft
        );
    }

    #[test]
    fn subsequent_working_ticks_edit_rather_than_send() {
        assert_eq!(
            decide(
                Some(AgentState::Working),
                AgentState::Working,
                true,
                true,
                Some(LONG),
                DELAY
            ),
            Action::EditDraft
        );
    }

    /// No-op suppression: an unchanged render must not cost a send.
    #[test]
    fn unchanged_render_sends_nothing() {
        assert_eq!(
            decide(
                Some(AgentState::Working),
                AgentState::Working,
                true,
                false,
                Some(LONG),
                DELAY
            ),
            Action::Nothing
        );
    }

    /// The point of the draft delay: a turn that finishes quickly must leave the room
    /// exactly as it found it. The agent's own reply is the only message such a turn needs.
    #[test]
    fn a_quick_turn_posts_nothing_at_all() {
        // Working, but not for long enough to be worth mentioning.
        assert_eq!(
            decide(None, AgentState::Working, false, true, Some(SHORT), DELAY),
            Action::Nothing
        );
        // ...and then it finishes. Still nothing: no draft was ever opened.
        assert_eq!(
            decide(
                Some(AgentState::Working),
                AgentState::WaitingForUser,
                false,
                true,
                None,
                DELAY
            ),
            Action::Nothing
        );
    }

    #[test]
    fn draft_opens_once_the_turn_runs_long() {
        assert_eq!(
            decide(None, AgentState::Working, false, true, Some(LONG), DELAY),
            Action::StartDraft
        );
    }

    /// A short turn that stalls or dies still has to be reported, even though the draft
    /// delay meant no draft existed to fold the news into. Silence here is the one
    /// outcome the whole feature exists to prevent.
    #[test]
    fn short_turn_that_fails_still_alerts() {
        for bad in [AgentState::Stalled, AgentState::Dead] {
            assert_eq!(
                decide(Some(AgentState::Working), bad, false, true, None, DELAY),
                Action::AlertOnly,
                "{bad:?} must be reported even with no draft open"
            );
        }
    }

    /// Every terminal state folds back into the draft rather than adding a message.
    #[test]
    fn terminal_transition_closes_the_draft() {
        for terminal in [
            AgentState::Stalled,
            AgentState::WaitingForUser,
            AgentState::Dead,
        ] {
            assert_eq!(
                decide(Some(AgentState::Working), terminal, true, true, None, DELAY),
                Action::CloseDraft,
                "{terminal:?} should close the draft in place"
            );
        }
    }

    /// The push-notification rule, which is what decides whether an extra message is sent.
    ///
    /// Finishing normally must NOT alert: the agent's own reply is a separate message and
    /// already pings, so alerting here would double-ping every turn — the redundancy that
    /// prompted this design. Stalled and dead must alert, because a silent edit is the
    /// only thing that would ever arrive.
    #[test]
    fn only_silent_failures_raise_an_alert() {
        assert!(!AgentState::WaitingForUser.needs_alert());
        assert!(AgentState::Stalled.needs_alert());
        assert!(AgentState::Dead.needs_alert());
        assert!(!AgentState::Working.needs_alert());
        assert!(!AgentState::Unknown.needs_alert());
    }

    /// The alert is a standalone message, so it must stand alone — the detail lives in
    /// the edited draft above it, but this line has to say what happened by itself.
    #[test]
    fn alert_body_is_self_contained() {
        let stalled = render_alert(&status(AgentState::Stalled));
        assert!(stalled.contains("stuck"));
        assert!(stalled.contains("4s"), "should carry the age: {stalled}");

        let dead = render_alert(&status(AgentState::Dead));
        assert!(dead.contains("not running"));

        // Metadata only, same rule as everywhere else in this module.
        for body in [stalled, dead] {
            assert!(!body.contains("Bash"));
        }
    }

    /// Debounce: closing fires once on entry, not on every tick that follows.
    #[test]
    fn terminal_state_is_announced_once() {
        // Enters Stalled: close the draft, which consumes it.
        assert_eq!(
            decide(
                Some(AgentState::Working),
                AgentState::Stalled,
                true,
                true,
                None,
                DELAY
            ),
            Action::CloseDraft
        );
        // Still stalled next tick, draft now closed: silence.
        assert_eq!(
            decide(
                Some(AgentState::Stalled),
                AgentState::Stalled,
                false,
                true,
                None,
                DELAY
            ),
            Action::Nothing
        );
    }

    /// stall → recover → stall must produce exactly one announcement per entry.
    #[test]
    fn stall_recover_stall_fires_once_per_transition() {
        let mut has_draft = false;
        let mut previous: Option<AgentState> = None;
        let mut terminals = 0;
        let mut starts = 0;

        let sequence = [
            AgentState::Working,
            AgentState::Working,
            AgentState::Stalled,
            AgentState::Stalled,
            AgentState::Working,
            AgentState::Stalled,
            AgentState::Stalled,
        ];
        for state in sequence {
            match decide(previous, state, has_draft, true, Some(LONG), DELAY) {
                Action::StartDraft => {
                    has_draft = true;
                    starts += 1;
                }
                Action::CloseDraft | Action::AlertOnly => {
                    has_draft = false;
                    terminals += 1;
                }
                Action::EditDraft | Action::Nothing => {}
            }
            previous = Some(state);
        }

        assert_eq!(starts, 2, "one draft per working spell");
        assert_eq!(terminals, 2, "one announcement per stall entry");
    }

    /// A bridge that starts up while the agent is idle must not volunteer a status
    /// message into the room unprompted.
    #[test]
    fn no_announcement_without_a_preceding_working_spell() {
        assert_eq!(
            decide(None, AgentState::WaitingForUser, false, true, None, DELAY),
            Action::Nothing
        );
    }

    #[test]
    fn unknown_state_is_never_broadcast() {
        assert_eq!(
            decide(
                Some(AgentState::Working),
                AgentState::Unknown,
                true,
                true,
                Some(LONG),
                DELAY
            ),
            Action::Nothing
        );
    }

    /// Privacy guard on the wire format, mirroring the one in `status`.
    #[test]
    fn rendered_bodies_are_metadata_only() {
        let working = render_working(&status(AgentState::Working));
        assert!(working.contains("Bash"));
        assert!(working.contains("working"));

        let terminal = render_terminal(&status(AgentState::Stalled));
        assert!(terminal.contains("stalled"));
        assert!(terminal.contains("3m 20s"));
    }

    #[test]
    fn bodies_respect_the_length_cap() {
        let long = truncate("x".repeat(MAX_TOTAL_LENGTH + 5_000));
        assert_eq!(long.len(), MAX_TOTAL_LENGTH);
    }

    /// Exercises the real Matrix write path against a live homeserver: send a draft, edit
    /// it in place twice, then close with a *new* message. The unit tests above only prove
    /// `decide()` picks the right action — this proves the actions actually work.
    ///
    /// Requires a throwaway account. Credentials come from the environment and are never
    /// written to disk; the store goes to a temp dir, never the live one.
    ///
    ///   MATRIX_TEST_HOMESERVER=... MATRIX_TEST_USER=... MATRIX_TEST_PASSWORD=... \
    ///     cargo test live_draft_cycle -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "requires a throwaway Matrix account"]
    async fn live_draft_cycle_against_real_homeserver() {
        let (Ok(hs), Ok(user), Ok(pass)) = (
            std::env::var("MATRIX_TEST_HOMESERVER"),
            std::env::var("MATRIX_TEST_USER"),
            std::env::var("MATRIX_TEST_PASSWORD"),
        ) else {
            panic!("set MATRIX_TEST_HOMESERVER / _USER / _PASSWORD");
        };

        let store = tempfile::tempdir().unwrap();
        let client = Client::builder()
            .homeserver_url(&hs)
            .sqlite_store(store.path(), None)
            .build()
            .await
            .expect("client build");

        let localpart = user.trim_start_matches('@').split(':').next().unwrap();
        client
            .matrix_auth()
            .login_username(localpart, &pass)
            .initial_device_display_name("cc-matrix-channel-livetest")
            .await
            .expect("login");
        println!("logged in as {}", client.user_id().unwrap());

        client.sync_once(Default::default()).await.expect("sync");

        let room = client
            .create_room(matrix_sdk::ruma::api::client::room::create_room::v3::Request::new())
            .await
            .expect("create room");
        let room_id = room.room_id().to_owned();
        println!("test room: {room_id}");

        // 1. Open the draft.
        let working = render_working(&status(AgentState::Working));
        let event_id = send_status(&client, &room_id, &working)
            .await
            .expect("draft send should return an event id");
        println!("draft event: {event_id}");

        // 2. Edit it in place. This is the OpenClaw/Hermes behaviour Sky asked for.
        for n in 1..=2 {
            let body = format!("{working}\nEdit #{n}");
            assert!(
                edit_status(&client, &room_id, &event_id, &body).await,
                "edit #{n} should be accepted by the homeserver"
            );
            println!("edit #{n} accepted");
        }

        // 3. Close with a NEW message, because Matrix edits do not push-notify.
        let terminal = render_terminal(&status(AgentState::Stalled));
        let terminal_id = send_status(&client, &room_id, &terminal)
            .await
            .expect("terminal send should return an event id");
        println!("terminal event: {terminal_id}");

        assert_ne!(
            event_id, terminal_id,
            "the terminal message must be a new event, not an edit of the draft"
        );

        client.matrix_auth().logout().await.ok();
    }
}
