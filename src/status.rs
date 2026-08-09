//! Agent liveness derived from the Claude Code session transcript.
//!
//! The bridge cannot ask the agent how it is doing: when the agent is stuck, it is stuck.
//! So liveness is read from something the agent does not control — the append-only
//! `.jsonl` transcript Claude Code writes for the session, plus `/proc` for the process
//! itself.
//!
//! This module is deliberately free of Matrix types so it can be unit-tested without a
//! homeserver.
//!
//! # Privacy
//!
//! Everything here is metadata: state names, ages, durations, and tool *names*. Message
//! text, tool inputs and tool outputs are never read out of the transcript and must never
//! be added to [`AgentStatus`] — this data is published to a Matrix room.

use std::fmt;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How much of the transcript tail to parse. The live transcript grows without bound
/// (already >1 MB in a normal session), and this is polled on an interval, so reading the
/// whole file every tick would be a performance bug.
const TAIL_BYTES: u64 = 64 * 1024;

/// An assistant record carrying only `text`/`thinking` is ambiguous: Claude Code writes
/// one record per content block, so a text block is followed by a `tool_use` block a
/// couple of seconds later when the turn continues. Treat a *recent* text block as still
/// working, and only call it `WaitingForUser` once it has gone quiet for this long.
const TEXT_GRACE: Duration = Duration::from_secs(10);

/// Default stall threshold, overridable with `CC_MATRIX_STALL_SECS`.
pub const DEFAULT_STALL_SECS: u64 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    /// A tool is running, or the model is generating between tool calls.
    Working,
    /// The agent finished its turn and is waiting for a human.
    WaitingForUser,
    /// Still nominally working, but nothing has happened for longer than the threshold.
    Stalled,
    /// The Claude Code process is gone.
    Dead,
    /// No transcript found, or it could not be parsed.
    Unknown,
}

impl fmt::Display for AgentState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Working => "working",
            Self::WaitingForUser => "waiting for you",
            Self::Stalled => "stalled",
            Self::Dead => "not running",
            Self::Unknown => "unknown",
        };
        f.write_str(s)
    }
}

impl AgentState {
    /// States that end a working spell. The live status draft is edited to one of these
    /// and then closed.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Stalled | Self::Dead | Self::WaitingForUser)
    }

    /// Terminal states that additionally need a *new* message to raise a push
    /// notification.
    ///
    /// Matrix edits do not push-notify, so a state reached by editing alone is silent.
    /// That is fine for `WaitingForUser`: the agent's actual reply is its own message and
    /// pings the user by itself, so an extra "done" would just double-ping every turn.
    /// `Stalled` and `Dead` are the opposite — nothing else will ever arrive, so without
    /// an alert the user's device stays quiet exactly when the agent is wedged.
    pub fn needs_alert(&self) -> bool {
        matches!(self, Self::Stalled | Self::Dead)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStatus {
    pub state: AgentState,
    /// Age of the newest timestamped transcript record.
    pub last_activity_age: Option<Duration>,
    /// Name only — never the tool's arguments.
    pub last_tool: Option<String>,
    /// How long the current turn has been running, measured from the last human prompt.
    ///
    /// `None` when the prompt that opened the turn has already scrolled out of the
    /// [`TAIL_BYTES`] window — which happens on genuinely long turns, exactly the ones
    /// worth watching. Treated as a nice-to-have: the state itself never depends on it.
    pub turn_elapsed: Option<Duration>,
}

impl AgentStatus {
    fn unknown() -> Self {
        Self {
            state: AgentState::Unknown,
            last_activity_age: None,
            last_tool: None,
            turn_elapsed: None,
        }
    }

    /// Multi-line rendering for `/status` and the live status message.
    ///
    /// Contains metadata only — see the module-level privacy note.
    pub fn render(&self) -> String {
        let mut out = format!("Agent:    {}", self.state);
        if let Some(age) = self.last_activity_age {
            out.push_str(&format!("\nLast activity: {} ago", format_duration(age)));
        }
        if let Some(tool) = &self.last_tool {
            out.push_str(&format!("\nLast tool:     {tool}"));
        }
        if let Some(elapsed) = self.turn_elapsed {
            out.push_str(&format!("\nTurn elapsed:  {}", format_duration(elapsed)));
        }
        out
    }
}

pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// Stall threshold from `CC_MATRIX_STALL_SECS`, so it is tunable without a rebuild.
pub fn stall_threshold() -> Duration {
    let secs = std::env::var("CC_MATRIX_STALL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_STALL_SECS);
    Duration::from_secs(secs)
}

/// Claude Code slugs the working directory to name its project dir: `/workspace` becomes
/// `-workspace`, `/home/node/x` becomes `-home-node-x`.
fn slug_for_cwd(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    s.chars()
        .map(|c| {
            if c == '/' || c == '.' || c == '_' {
                '-'
            } else {
                c
            }
        })
        .collect()
}

/// How [`transcript_path`] resolved the transcript.
///
/// Worth surfacing rather than keeping internal: the fallback can silently point the bridge
/// at a *different session's* transcript, and a bridge reading someone else's liveness looks
/// identical from the outside to one reading its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptSource {
    /// Resolved directly from `CLAUDE_CODE_SESSION_ID` — this session's own transcript.
    SessionId,
    /// `CLAUDE_CODE_SESSION_ID` was unset. This is the most recently modified transcript
    /// in the project directory, which may belong to another session entirely — so it is
    /// only ever used when there is no session id to be more precise with.
    NewestFallback,
}

impl fmt::Display for TranscriptSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SessionId => write!(f, "session-id"),
            Self::NewestFallback => write!(f, "newest-fallback"),
        }
    }
}

/// Locate this session's transcript.
///
/// Uses `CLAUDE_CODE_SESSION_ID`, which Claude Code sets in the environment MCP servers
/// inherit. When that names a transcript that does not exist yet — the normal state of a
/// session nobody has messaged since it started — the answer is `None`, which reads out as
/// `Unknown` and is never broadcast. Only with no session id at all does this fall back to
/// the most recently modified `*.jsonl` in the project directory.
pub fn transcript_path() -> Option<PathBuf> {
    transcript_path_with_source().map(|(p, _)| p)
}

/// As [`transcript_path`], but also reports which branch produced the answer.
pub fn transcript_path_with_source() -> Option<(PathBuf, TranscriptSource)> {
    let home = dirs_next::home_dir()?;
    let cwd = std::env::current_dir().ok()?;
    let project_dir = home.join(".claude/projects").join(slug_for_cwd(&cwd));
    let session_id = std::env::var("CLAUDE_CODE_SESSION_ID").ok();

    resolve_transcript(&project_dir, session_id.as_deref())
}

/// The resolution rule, separated from the environment it normally reads so it can be
/// tested against a fixture directory.
fn resolve_transcript(
    project_dir: &Path,
    session_id: Option<&str>,
) -> Option<(PathBuf, TranscriptSource)> {
    if let Some(id) = session_id {
        let direct = project_dir.join(format!("{id}.jsonl"));
        return direct
            .is_file()
            .then_some((direct, TranscriptSource::SessionId));
    }

    newest_jsonl(project_dir).map(|p| (p, TranscriptSource::NewestFallback))
}

fn newest_jsonl(dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
            best = Some((mtime, path));
        }
    }
    best.map(|(_, p)| p)
}

/// True if the Claude Code process named by `CLAUDE_PID` is still around.
/// `None` when there is no PID to check, so the caller can avoid claiming `Dead` on
/// missing information.
fn claude_alive() -> Option<bool> {
    let pid = std::env::var("CLAUDE_PID").ok()?;
    if pid.chars().any(|c| !c.is_ascii_digit()) {
        return None;
    }
    Some(Path::new(&format!("/proc/{pid}")).is_dir())
}

/// Read the last `TAIL_BYTES` of the file as whole lines, dropping the leading partial
/// line so the first record parsed is never truncated.
fn read_tail(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;

    let mut buf = Vec::with_capacity(TAIL_BYTES as usize);
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();

    if start == 0 {
        return Some(text);
    }
    // Discard everything up to and including the first newline: that fragment began
    // before our seek point.
    match text.find('\n') {
        Some(idx) => Some(text[idx + 1..].to_string()),
        None => Some(String::new()),
    }
}

/// RFC3339 timestamp (`2026-08-08T15:56:53.203Z`) to a `SystemTime`.
///
/// Hand-rolled rather than pulling in `chrono`: the format is fixed, always UTC, and this
/// avoids adding a dependency for one field.
fn parse_timestamp(s: &str) -> Option<SystemTime> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);

    // Days since the Unix epoch — Howard Hinnant's civil-from-days algorithm.
    let y_adj = if mo <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    let total = days * 86_400 + h * 3_600 + mi * 60 + sec;
    if total < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(total as u64))
}

/// What a single transcript line tells us. Records that carry no liveness signal
/// (`mode`, `permission-mode`, `ai-title`, …) collapse to `None`.
enum Record {
    /// Assistant emitted a tool call. Carries the tool name.
    ToolUse(String),
    /// Tool finished and the result went back to the model.
    ToolResult,
    /// Assistant text or thinking — ambiguous, see [`TEXT_GRACE`].
    AssistantText,
    /// A human prompt: the start of a turn.
    UserPrompt,
}

fn classify(v: &serde_json::Value) -> Option<Record> {
    match v.get("type").and_then(|t| t.as_str())? {
        "assistant" => {
            let blocks = v.get("message")?.get("content")?.as_array()?;
            // A tool_use block anywhere in the record means work is in flight.
            for b in blocks {
                if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    let name = b
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("(unnamed)")
                        .to_string();
                    return Some(Record::ToolUse(name));
                }
            }
            Some(Record::AssistantText)
        }
        "user" => {
            if v.get("toolUseResult").is_some() {
                Some(Record::ToolResult)
            } else {
                Some(Record::UserPrompt)
            }
        }
        _ => None,
    }
}

/// Derive agent state from the transcript at `path`.
///
/// `now` is injected so the state machine is testable against fixed fixtures.
pub fn read_status_at(path: &Path, stall_threshold: Duration, now: SystemTime) -> AgentStatus {
    if claude_alive() == Some(false) {
        return AgentStatus {
            state: AgentState::Dead,
            ..AgentStatus::unknown()
        };
    }

    let Some(tail) = read_tail(path) else {
        return AgentStatus::unknown();
    };

    let mut last_ts: Option<SystemTime> = None;
    let mut last_record: Option<Record> = None;
    let mut last_tool: Option<String> = None;
    let mut turn_start: Option<SystemTime> = None;

    for line in tail.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // A truncated or malformed final line is expected on a file being appended to.
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let ts = value
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(parse_timestamp);
        if let Some(ts) = ts {
            last_ts = Some(ts);
        }

        let Some(record) = classify(&value) else {
            continue;
        };
        match &record {
            Record::ToolUse(name) => last_tool = Some(name.clone()),
            Record::UserPrompt => {
                turn_start = ts;
                last_tool = None;
            }
            _ => {}
        }
        last_record = Some(record);
    }

    let Some(last_record) = last_record else {
        return AgentStatus::unknown();
    };

    let age = last_ts.and_then(|t| now.duration_since(t).ok());

    let base = match last_record {
        Record::ToolUse(_) | Record::ToolResult => AgentState::Working,
        Record::AssistantText => match age {
            Some(a) if a < TEXT_GRACE => AgentState::Working,
            _ => AgentState::WaitingForUser,
        },
        Record::UserPrompt => AgentState::Working,
    };

    // Stalling only applies to states that claim work is in progress. An agent that
    // finished its turn an hour ago is waiting, not stuck.
    let state = match (base, age) {
        (AgentState::Working, Some(a)) if a > stall_threshold => AgentState::Stalled,
        (other, _) => other,
    };

    let turn_elapsed = match state {
        AgentState::Working | AgentState::Stalled => {
            turn_start.and_then(|t| now.duration_since(t).ok())
        }
        _ => None,
    };

    AgentStatus {
        state,
        last_activity_age: age,
        last_tool: if matches!(state, AgentState::Working | AgentState::Stalled) {
            last_tool
        } else {
            None
        },
        turn_elapsed,
    }
}

/// Convenience wrapper: locate the transcript and read it as of now.
pub fn read_status(stall_threshold: Duration) -> AgentStatus {
    match transcript_path() {
        Some(path) => read_status_at(&path, stall_threshold, SystemTime::now()),
        None => AgentStatus::unknown(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const STALL: Duration = Duration::from_secs(300);

    fn at(s: &str) -> SystemTime {
        parse_timestamp(s).expect("fixture timestamp should parse")
    }

    /// Write fixture lines to a temp file and read the resulting status.
    fn status_of(lines: &[&str], now: &str) -> AgentStatus {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut f = File::create(&path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        f.flush().unwrap();
        read_status_at(&path, STALL, at(now))
    }

    const PROMPT: &str = r#"{"type":"user","timestamp":"2026-08-08T12:00:00.000Z","message":{"content":[{"type":"text","text":"do the thing"}]}}"#;
    const TOOL_USE: &str = r#"{"type":"assistant","timestamp":"2026-08-08T12:00:05.000Z","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"secret-command"}}]}}"#;
    const TOOL_RESULT: &str = r#"{"type":"user","timestamp":"2026-08-08T12:00:07.000Z","toolUseResult":{"stdout":"secret-output"},"message":{"content":[{"type":"tool_result"}]}}"#;
    const TEXT: &str = r#"{"type":"assistant","timestamp":"2026-08-08T12:00:09.000Z","message":{"content":[{"type":"text","text":"here is my secret answer"}]}}"#;
    // Untimestamped bookkeeping records Claude Code interleaves with real activity.
    const NOISE: &str = r#"{"type":"mode","sessionId":"x","mode":"normal"}"#;

    #[test]
    fn timestamp_parsing_matches_known_epoch() {
        // Expected values cross-checked against `date -u -d <ts> +%s`.
        // 2026-08-08T12:00:00Z == 1786190400
        let t = at("2026-08-08T12:00:00.000Z");
        assert_eq!(
            t.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_786_190_400
        );
        // Epoch itself, and a leap day.
        assert_eq!(
            at("1970-01-01T00:00:00.000Z")
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            0
        );
        assert_eq!(
            at("2024-02-29T00:00:00.000Z")
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            1_709_164_800
        );
        assert!(parse_timestamp("not-a-timestamp").is_none());
    }

    #[test]
    fn tool_use_tail_is_working() {
        let s = status_of(&[PROMPT, TOOL_USE], "2026-08-08T12:00:06.000Z");
        assert_eq!(s.state, AgentState::Working);
        assert_eq!(s.last_tool.as_deref(), Some("Bash"));
        assert_eq!(s.last_activity_age, Some(Duration::from_secs(1)));
        assert_eq!(s.turn_elapsed, Some(Duration::from_secs(6)));
    }

    #[test]
    fn tool_result_tail_is_working() {
        let s = status_of(&[PROMPT, TOOL_USE, TOOL_RESULT], "2026-08-08T12:00:08.000Z");
        assert_eq!(s.state, AgentState::Working);
    }

    /// The reason TEXT_GRACE exists: Claude Code writes one record per content block, so a
    /// text block is routinely followed by a tool_use block seconds later. Calling that
    /// `WaitingForUser` immediately would report "your turn" mid-work.
    #[test]
    fn recent_text_tail_is_still_working() {
        let s = status_of(&[PROMPT, TEXT], "2026-08-08T12:00:11.000Z");
        assert_eq!(s.state, AgentState::Working);
    }

    #[test]
    fn settled_text_tail_is_waiting_for_user() {
        let s = status_of(&[PROMPT, TEXT], "2026-08-08T12:00:30.000Z");
        assert_eq!(s.state, AgentState::WaitingForUser);
        // A finished turn reports no elapsed timer and no lingering tool name.
        assert_eq!(s.turn_elapsed, None);
        assert_eq!(s.last_tool, None);
    }

    #[test]
    fn old_tool_use_is_stalled() {
        let s = status_of(&[PROMPT, TOOL_USE], "2026-08-08T12:10:00.000Z");
        assert_eq!(s.state, AgentState::Stalled);
        assert_eq!(s.last_tool.as_deref(), Some("Bash"));
    }

    /// A finished turn left alone for hours is waiting, not stalled.
    #[test]
    fn old_text_tail_is_not_stalled() {
        let s = status_of(&[PROMPT, TEXT], "2026-08-08T14:00:00.000Z");
        assert_eq!(s.state, AgentState::WaitingForUser);
    }

    #[test]
    fn untimestamped_records_do_not_reset_activity_age() {
        // NOISE arrives after TOOL_USE but carries no timestamp; age must still be
        // measured from the tool call, and the state must still be Working.
        let s = status_of(&[PROMPT, TOOL_USE, NOISE], "2026-08-08T12:00:06.000Z");
        assert_eq!(s.state, AgentState::Working);
        assert_eq!(s.last_activity_age, Some(Duration::from_secs(1)));
    }

    #[test]
    fn malformed_and_truncated_lines_are_skipped() {
        let truncated = r#"{"type":"assistant","timestamp":"2026-08-08T12:00:0"#;
        let s = status_of(
            &[PROMPT, TOOL_USE, "not json at all", truncated],
            "2026-08-08T12:00:06.000Z",
        );
        assert_eq!(s.state, AgentState::Working);
        assert_eq!(s.last_tool.as_deref(), Some("Bash"));
    }

    #[test]
    fn empty_transcript_is_unknown() {
        let s = status_of(&[], "2026-08-08T12:00:06.000Z");
        assert_eq!(s.state, AgentState::Unknown);
    }

    #[test]
    fn missing_transcript_is_unknown() {
        let s = read_status_at(
            Path::new("/nonexistent/session.jsonl"),
            STALL,
            at("2026-08-08T12:00:00.000Z"),
        );
        assert_eq!(s.state, AgentState::Unknown);
    }

    /// Privacy guard: the rendered status must never leak transcript content. The
    /// fixtures deliberately contain marker strings in message text, tool input and tool
    /// output.
    #[test]
    fn render_contains_no_transcript_content() {
        for now in [
            "2026-08-08T12:00:06.000Z",
            "2026-08-08T12:00:30.000Z",
            "2026-08-08T12:10:00.000Z",
        ] {
            let rendered = status_of(&[PROMPT, TOOL_USE, TOOL_RESULT, TEXT], now).render();
            for secret in [
                "secret-command",
                "secret-output",
                "secret answer",
                "do the thing",
            ] {
                assert!(
                    !rendered.contains(secret),
                    "status leaked {secret:?} at {now}: {rendered}"
                );
            }
        }
    }

    /// Tool *names* are metadata and are allowed; arguments are not.
    #[test]
    fn render_includes_tool_name_only() {
        let rendered = status_of(&[PROMPT, TOOL_USE], "2026-08-08T12:00:06.000Z").render();
        assert!(rendered.contains("Bash"));
        assert!(!rendered.contains("command"));
    }

    #[test]
    fn tail_reads_only_the_last_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.jsonl");
        let mut f = File::create(&path).unwrap();
        // Well over TAIL_BYTES of filler, then the records that matter.
        let filler = format!(
            r#"{{"type":"assistant","timestamp":"2026-08-08T11:00:00.000Z","message":{{"content":[{{"type":"text","text":"{}"}}]}}}}"#,
            "x".repeat(500)
        );
        for _ in 0..400 {
            writeln!(f, "{filler}").unwrap();
        }
        writeln!(f, "{PROMPT}").unwrap();
        writeln!(f, "{TOOL_USE}").unwrap();
        f.flush().unwrap();

        assert!(std::fs::metadata(&path).unwrap().len() > TAIL_BYTES);
        let s = read_status_at(&path, STALL, at("2026-08-08T12:00:06.000Z"));
        assert_eq!(s.state, AgentState::Working);
        assert_eq!(s.last_tool.as_deref(), Some("Bash"));
    }

    /// A session that has not been messaged since it started has no transcript yet, and
    /// every session sharing a working directory shares this project directory. Falling
    /// back to "newest" here hands the bridge a *different* session's transcript, and it
    /// then reports a stranger's liveness into the room as if it were its own. No
    /// information is the honest answer.
    #[test]
    fn session_id_with_no_transcript_yet_refuses_to_guess() {
        let dir = tempfile::tempdir().unwrap();
        File::create(dir.path().join("another-session.jsonl")).unwrap();

        let resolved = resolve_transcript(dir.path(), Some("my-session"));

        assert_eq!(resolved, None);
    }

    #[test]
    fn session_id_resolves_to_that_sessions_own_transcript() {
        let dir = tempfile::tempdir().unwrap();
        File::create(dir.path().join("my-session.jsonl")).unwrap();
        File::create(dir.path().join("another-session.jsonl")).unwrap();

        let (path, source) = resolve_transcript(dir.path(), Some("my-session")).unwrap();

        assert_eq!(path, dir.path().join("my-session.jsonl"));
        assert_eq!(source, TranscriptSource::SessionId);
    }

    /// Without a session id there is nothing more precise to go on, so the newest
    /// transcript remains the best available guess.
    #[test]
    fn without_a_session_id_the_newest_transcript_is_still_used() {
        let dir = tempfile::tempdir().unwrap();
        File::create(dir.path().join("only-session.jsonl")).unwrap();

        let (path, source) = resolve_transcript(dir.path(), None).unwrap();

        assert_eq!(path, dir.path().join("only-session.jsonl"));
        assert_eq!(source, TranscriptSource::NewestFallback);
    }

    #[test]
    fn cwd_slugging_matches_claude_code_layout() {
        assert_eq!(slug_for_cwd(Path::new("/workspace")), "-workspace");
        assert_eq!(slug_for_cwd(Path::new("/home/node/x")), "-home-node-x");
    }

    #[test]
    fn terminal_states_are_the_ones_that_need_a_push() {
        assert!(AgentState::Stalled.is_terminal());
        assert!(AgentState::Dead.is_terminal());
        assert!(AgentState::WaitingForUser.is_terminal());
        assert!(!AgentState::Working.is_terminal());
        // Unknown is not terminal: it means "no information", which must never be
        // broadcast as if the agent had finished.
        assert!(!AgentState::Unknown.is_terminal());
    }

    /// Reads the *real* transcript of whatever Claude Code session is running this test.
    /// This is the check that proves the feature end to end — the unit tests above only
    /// prove the state machine agrees with fixtures I wrote myself.
    ///
    /// Skips when not run from inside a Claude Code session. Run it with:
    ///   cargo test live_session -- --ignored --nocapture
    #[test]
    #[ignore = "requires a live Claude Code session"]
    fn live_session_reports_a_real_state() {
        let path = transcript_path().expect("no transcript found — is this a Claude Code session?");
        println!("transcript: {}", path.display());
        let meta = std::fs::metadata(&path).unwrap();
        println!("size: {} bytes", meta.len());
        let mtime_age = SystemTime::now()
            .duration_since(meta.modified().unwrap())
            .unwrap_or_default();
        println!("mtime age: {}", format_duration(mtime_age));

        let status = read_status(STALL);
        println!("--- rendered ---\n{}", status.render());

        // A session that is running this very test is, by definition, working.
        assert_ne!(
            status.state,
            AgentState::Unknown,
            "live transcript should yield a real state"
        );
        assert!(
            matches!(status.state, AgentState::Working),
            "the session running this test should read as Working, got {:?}",
            status.state
        );
    }

    #[test]
    fn duration_formatting() {
        assert_eq!(format_duration(Duration::from_secs(4)), "4s");
        assert_eq!(format_duration(Duration::from_secs(200)), "3m 20s");
        assert_eq!(format_duration(Duration::from_secs(8040)), "2h 14m");
    }
}
