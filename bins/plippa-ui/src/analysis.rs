//! Per-track analysis status (TRACK_01): what the server has analysed for one media file, and
//! the screen state that lets the person re-analyse it.
//!
//! The screen is a list of analysis *kinds*, not a waveform flag. Today the server answers for
//! the waveform only; BPM and downbeat are declared here so their rows exist (as "Not available
//! yet", not selectable) and a server that starts answering for them fills the row without any
//! UI rework. Kinds fail independently, so each row carries its own state.

use serde_json::Value;

use crate::store_api::StoreApiError;
use crate::store_tap::STORE_TIMEOUT_MS;

/// How often the screen asks again while something is `Queued` or `Analysing`. Slow on
/// purpose: the worker decodes a track in seconds to minutes, and every poll is an HTTP round
/// trip on the shared store thread.
pub const ANALYSIS_POLL_MS: u64 = 1_500;

/// `poll_at_ms` while no poll is wanted.
const POLL_NEVER: u64 = u64::MAX;

/// One thing the server can learn about a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnalysisKind {
    Waveform,
    Bpm,
    Downbeat,
}

impl AnalysisKind {
    /// Every kind the screen knows, in display order.
    pub const ALL: [AnalysisKind; 3] = [Self::Waveform, Self::Bpm, Self::Downbeat];

    /// The name the server uses (`kinds` in the status and in the re-analysis body).
    pub fn api_name(self) -> &'static str {
        match self {
            Self::Waveform => "waveform",
            Self::Bpm => "bpm",
            Self::Downbeat => "downbeat",
        }
    }

    /// `None` for a kind a newer server knows and this build does not.
    pub fn from_api(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.api_name() == name)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Waveform => "Waveform",
            Self::Bpm => "BPM",
            Self::Downbeat => "Downbeat",
        }
    }
}

/// The server's per-item vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalysisState {
    Analysed,
    Analysing,
    Queued,
    Failed,
    /// The media file is missing: re-analysis can never help.
    Unavailable,
}

impl AnalysisState {
    /// An unknown or missing word reads as `Queued`: a newer server must never panic or blank
    /// the screen, and "waiting" is the one state that invites another look.
    pub fn from_api(word: Option<&str>) -> Self {
        match word {
            Some("analysed") => Self::Analysed,
            Some("analysing") => Self::Analysing,
            Some("failed") => Self::Failed,
            Some("unavailable") => Self::Unavailable,
            _ => Self::Queued,
        }
    }

    /// Still moving: worth polling for.
    pub fn is_pending(self) -> bool {
        matches!(self, Self::Queued | Self::Analysing)
    }
}

/// One answered kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisKindStatus {
    pub kind: AnalysisKind,
    pub state: AnalysisState,
    /// Malformed frames skipped to finish this analysis (0 = none, or not known).
    pub skipped_frames: u64,
}

impl AnalysisKindStatus {
    /// The value column of the row: the state in words, plus the skip count on a success.
    pub fn text(&self) -> String {
        match (self.state, self.skipped_frames) {
            (AnalysisState::Analysed, 0) => "Analysed".into(),
            (AnalysisState::Analysed, 1) => "Analysed, 1 bad frame skipped".into(),
            (AnalysisState::Analysed, n) => format!("Analysed, {n} bad frames skipped"),
            (AnalysisState::Analysing, _) => "Analysing\u{2026}".into(),
            (AnalysisState::Queued, _) => "Queued".into(),
            (AnalysisState::Failed, _) => "Failed".into(),
            (AnalysisState::Unavailable, _) => "Unavailable".into(),
        }
    }
}

/// Read one media item's kinds out of a `GET /api/waveforms/:id` answer.
///
/// `kinds` is the list the server sends per item; an item without one (an older server) is read
/// as a waveform-only answer from its `state`. Kinds this build does not know are ignored.
pub fn parse_analysis(value: &Value, item: u32) -> Result<Vec<AnalysisKindStatus>, StoreApiError> {
    let items = value
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| StoreApiError::Malformed("the analysis answer has no items".into()))?;
    let entry = items
        .iter()
        .find(|i| i.get("item").and_then(Value::as_u64) == Some(u64::from(item)))
        .ok_or_else(|| StoreApiError::Malformed("the analysis answer lacks this track".into()))?;
    let state_of = |v: &Value| AnalysisState::from_api(v.get("state").and_then(Value::as_str));
    let Some(kinds) = entry.get("kinds").and_then(Value::as_array) else {
        return Ok(vec![AnalysisKindStatus {
            kind: AnalysisKind::Waveform,
            state: state_of(entry),
            skipped_frames: 0,
        }]);
    };
    let mut out: Vec<AnalysisKindStatus> = Vec::new();
    for k in kinds {
        let Some(kind) = k
            .get("kind")
            .and_then(Value::as_str)
            .and_then(AnalysisKind::from_api)
        else {
            continue;
        };
        if out.iter().any(|s| s.kind == kind) {
            continue;
        }
        out.push(AnalysisKindStatus {
            kind,
            state: state_of(k),
            skipped_frames: k.get("skipped_frames").and_then(Value::as_u64).unwrap_or(0),
        });
    }
    Ok(out)
}

/// What a selectable row does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalysisAction {
    /// Re-analyse every kind of this track.
    ReanalyseAll,
    /// Re-analyse one kind the server answered for.
    ReanalyseKind(AnalysisKind),
    /// The status could not be read: ask again.
    Retry,
}

/// Where the screen is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnalysisPhase {
    /// Waiting for the first answer.
    Loading,
    Ready,
    /// The status could not be read.
    Failed(String),
}

/// The analysis screen for one track (`LibraryTrackAnalysis`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisView {
    /// Namespaced playable id, as the server's API takes it.
    pub playable_id: u64,
    /// `mediaList` position of the track inside that playable.
    pub item: u32,
    pub title: String,
    pub phase: AnalysisPhase,
    /// What the server answered for. A kind missing here reads "Not available yet".
    pub kinds: Vec<AnalysisKindStatus>,
    /// A line under the rows: why a request was refused.
    pub note: Option<String>,
    /// A call is out (status or re-analysis); `requested_at_ms` says since when.
    pub waiting: bool,
    pub requested_at_ms: u64,
    /// When the next status poll is due.
    pub poll_at_ms: u64,
}

/// One displayed row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisRow {
    pub label: &'static str,
    pub value: String,
    /// Reachable with the encoder. Only answered kinds and the re-analyse row are.
    pub selectable: bool,
    pub action: Option<AnalysisAction>,
}

impl AnalysisView {
    /// A screen whose first status read is about to be sent at `now_ms`.
    pub fn new(playable_id: u64, item: u32, title: String, now_ms: u64) -> Self {
        Self {
            playable_id,
            item,
            title,
            phase: AnalysisPhase::Loading,
            kinds: Vec::new(),
            note: None,
            waiting: true,
            requested_at_ms: now_ms,
            poll_at_ms: POLL_NEVER,
        }
    }

    fn status_of(&self, kind: AnalysisKind) -> Option<&AnalysisKindStatus> {
        self.kinds.iter().find(|s| s.kind == kind)
    }

    /// The waveform's state, when answered.
    pub fn waveform_state(&self) -> Option<AnalysisState> {
        self.status_of(AnalysisKind::Waveform).map(|s| s.state)
    }

    /// The rows in display order: every known kind, then the re-analyse row (or Try again).
    pub fn rows(&self) -> Vec<AnalysisRow> {
        let mut rows = Vec::new();
        for kind in AnalysisKind::ALL {
            let status = self.status_of(kind);
            let value = match (&self.phase, status) {
                (_, Some(s)) => s.text(),
                (AnalysisPhase::Loading, None) => "\u{2026}".to_string(),
                (_, None) => "Not available yet".to_string(),
            };
            rows.push(AnalysisRow {
                label: kind.label(),
                value,
                selectable: status.is_some(),
                action: status.map(|_| AnalysisAction::ReanalyseKind(kind)),
            });
        }
        match self.phase {
            AnalysisPhase::Failed(_) => rows.push(AnalysisRow {
                label: "Try again",
                value: String::new(),
                selectable: true,
                action: Some(AnalysisAction::Retry),
            }),
            _ => rows.push(AnalysisRow {
                label: "Re-analyse",
                value: String::new(),
                selectable: self.phase == AnalysisPhase::Ready,
                action: Some(AnalysisAction::ReanalyseAll),
            }),
        }
        rows
    }

    /// The rows the encoder can land on, in order. `nav.focus` indexes this list.
    pub fn actions(&self) -> Vec<AnalysisAction> {
        self.rows()
            .into_iter()
            .filter(|r| r.selectable)
            .filter_map(|r| r.action)
            .collect()
    }

    /// The action under the encoder.
    pub fn action_at(&self, focus: usize) -> Option<AnalysisAction> {
        let actions = self.actions();
        actions.get(focus.min(actions.len().saturating_sub(1))).copied()
    }

    /// The full sentence under the rows, when the state calls for one.
    pub fn explanation(&self) -> Option<String> {
        if let AnalysisPhase::Failed(why) = &self.phase {
            return Some(format!("Could not read the analysis: {why}"));
        }
        if self.waveform_state() == Some(AnalysisState::Unavailable) {
            return Some("The media file is missing, so it cannot be analysed.".into());
        }
        self.note.clone()
    }

    /// A call goes out now.
    pub fn begin_call(&mut self, now_ms: u64) {
        self.waiting = true;
        self.requested_at_ms = now_ms;
    }

    /// A call nobody answered is reported rather than waited on for ever.
    pub fn timed_out(&self, now_ms: u64) -> bool {
        self.waiting && now_ms.saturating_sub(self.requested_at_ms) >= STORE_TIMEOUT_MS
    }

    /// The answer to a status read.
    pub fn apply_status(
        &mut self,
        result: Result<Vec<AnalysisKindStatus>, StoreApiError>,
        now_ms: u64,
    ) {
        self.waiting = false;
        match result {
            Ok(kinds) => {
                self.phase = AnalysisPhase::Ready;
                self.kinds = kinds;
                self.note = None;
                self.schedule_poll(now_ms);
            }
            Err(error) => {
                self.phase = AnalysisPhase::Failed(error.message());
                self.poll_at_ms = POLL_NEVER;
            }
        }
    }

    /// The answer to a re-analysis request for `kind` (`None` = every kind).
    pub fn apply_reanalyse(
        &mut self,
        kind: Option<AnalysisKind>,
        result: Result<(), StoreApiError>,
        now_ms: u64,
    ) {
        self.waiting = false;
        match result {
            Ok(()) => {
                // Queued now, whatever it read a moment ago; the next poll says `Analysing`.
                for status in self
                    .kinds
                    .iter_mut()
                    .filter(|s| kind.is_none_or(|k| k == s.kind))
                {
                    status.state = AnalysisState::Queued;
                    status.skipped_frames = 0;
                }
                self.note = None;
                self.schedule_poll(now_ms);
            }
            Err(error) if error.code() == Some("unavailable") => {
                // The server refuses because the file is gone: say that, not "queued".
                for status in self
                    .kinds
                    .iter_mut()
                    .filter(|s| kind.is_none_or(|k| k == s.kind))
                {
                    status.state = AnalysisState::Unavailable;
                }
                self.note = None;
            }
            Err(error) => self.note = Some(format!("Not queued: {}", error.message())),
        }
    }

    /// Poll again soon while anything is still moving, never otherwise.
    fn schedule_poll(&mut self, now_ms: u64) {
        self.poll_at_ms = if self.kinds.iter().any(|s| s.state.is_pending()) {
            now_ms.saturating_add(ANALYSIS_POLL_MS)
        } else {
            POLL_NEVER
        };
    }

    /// A status read is due: ready, something pending, no call out, and the interval is up.
    pub fn poll_due(&self, now_ms: u64) -> bool {
        self.phase == AnalysisPhase::Ready
            && !self.waiting
            && self.poll_at_ms != POLL_NEVER
            && now_ms >= self.poll_at_ms
    }

    /// The poll goes out now; the next one is not due until its answer arrives.
    pub fn begin_poll(&mut self, now_ms: u64) {
        self.begin_call(now_ms);
        self.poll_at_ms = POLL_NEVER;
    }

    /// Back to waiting for a first answer (Try again).
    pub fn restart(&mut self, now_ms: u64) {
        self.phase = AnalysisPhase::Loading;
        self.note = None;
        self.begin_call(now_ms);
        self.poll_at_ms = POLL_NEVER;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn waveform(state: AnalysisState) -> AnalysisKindStatus {
        AnalysisKindStatus {
            kind: AnalysisKind::Waveform,
            state,
            skipped_frames: 0,
        }
    }

    fn ready(kinds: Vec<AnalysisKindStatus>) -> AnalysisView {
        let mut view = AnalysisView::new(7, 0, "T".into(), 0);
        view.apply_status(Ok(kinds), 0);
        view
    }

    #[test]
    fn the_status_json_fills_the_waveform_and_leaves_bpm_and_downbeat_unanswered() {
        let answer = json!({"id": 7, "state": "partial", "items": [
            {"item": 0, "state": "queued", "kinds": [{"kind": "waveform", "state": "queued"}]},
            {"item": 1, "state": "analysed", "kinds": [
                {"kind": "waveform", "state": "analysed", "skipped_frames": 2}]},
        ]});
        let kinds = parse_analysis(&answer, 1).unwrap();
        assert_eq!(
            kinds,
            vec![AnalysisKindStatus {
                kind: AnalysisKind::Waveform,
                state: AnalysisState::Analysed,
                skipped_frames: 2
            }]
        );
        let view = ready(kinds);
        let rows = view.rows();
        assert_eq!(rows[0].value, "Analysed, 2 bad frames skipped");
        assert!(rows[0].selectable);
        for row in &rows[1..3] {
            assert_eq!(row.value, "Not available yet");
            assert!(!row.selectable, "{} must not be selectable", row.label);
        }
        assert_eq!(rows[3].label, "Re-analyse");
        assert_eq!(
            view.actions(),
            vec![
                AnalysisAction::ReanalyseKind(AnalysisKind::Waveform),
                AnalysisAction::ReanalyseAll
            ]
        );
    }

    #[test]
    fn an_unknown_or_missing_state_reads_queued_and_unknown_kinds_are_ignored() {
        let answer = json!({"items": [{"item": 0, "kinds": [
            {"kind": "waveform", "state": "reticulating"},
            {"kind": "tempo-map", "state": "analysed"},
            {"kind": "bpm"},
        ]}]});
        let kinds = parse_analysis(&answer, 0).unwrap();
        assert_eq!(
            kinds.iter().map(|k| (k.kind, k.state)).collect::<Vec<_>>(),
            vec![
                (AnalysisKind::Waveform, AnalysisState::Queued),
                (AnalysisKind::Bpm, AnalysisState::Queued)
            ],
            "the unmapped kind is dropped, the unknown word waits"
        );
        // A newer server that answers for BPM makes that row selectable.
        let view = ready(kinds);
        assert!(view.rows()[1].selectable);
        assert_eq!(view.rows()[2].value, "Not available yet");
    }

    #[test]
    fn an_item_without_a_kinds_list_reads_as_a_waveform_only_answer() {
        let answer = json!({"items": [{"item": 0, "state": "failed"}, {"item": 1}]});
        assert_eq!(
            parse_analysis(&answer, 0).unwrap()[0].state,
            AnalysisState::Failed
        );
        assert_eq!(
            parse_analysis(&answer, 1).unwrap()[0].state,
            AnalysisState::Queued,
            "no state at all waits rather than panics"
        );
    }

    #[test]
    fn an_answer_that_is_not_a_status_is_an_error_not_a_panic() {
        assert!(parse_analysis(&json!({}), 0).is_err());
        assert!(parse_analysis(&json!({"items": []}), 0).is_err());
        assert!(parse_analysis(&Value::Null, 0).is_err());
    }

    #[test]
    fn the_skip_count_reads_as_a_sentence() {
        let mut s = waveform(AnalysisState::Analysed);
        assert_eq!(s.text(), "Analysed");
        s.skipped_frames = 1;
        assert_eq!(s.text(), "Analysed, 1 bad frame skipped");
        s.state = AnalysisState::Failed;
        assert_eq!(s.text(), "Failed", "a count only decorates a success");
    }

    #[test]
    fn polling_runs_only_while_something_is_moving() {
        let mut view = ready(vec![waveform(AnalysisState::Queued)]);
        assert!(!view.poll_due(ANALYSIS_POLL_MS - 1));
        assert!(view.poll_due(ANALYSIS_POLL_MS));
        view.begin_poll(ANALYSIS_POLL_MS);
        assert!(!view.poll_due(u64::MAX), "one call at a time");
        view.apply_status(Ok(vec![waveform(AnalysisState::Analysing)]), 2_000);
        assert!(view.poll_due(2_000 + ANALYSIS_POLL_MS));
        view.begin_poll(4_000);
        view.apply_status(Ok(vec![waveform(AnalysisState::Analysed)]), 4_100);
        assert!(!view.poll_due(u64::MAX), "analysed: nothing left to wait for");
        let mut failed = ready(vec![waveform(AnalysisState::Failed)]);
        assert!(!failed.poll_due(u64::MAX));
        failed.apply_status(Ok(vec![waveform(AnalysisState::Unavailable)]), 0);
        assert!(!failed.poll_due(u64::MAX));
    }

    #[test]
    fn a_reanalysis_reads_queued_until_the_next_poll_and_resumes_polling() {
        let mut view = ready(vec![waveform(AnalysisState::Failed)]);
        assert!(!view.poll_due(u64::MAX));
        view.begin_call(10);
        view.apply_reanalyse(None, Ok(()), 20);
        assert_eq!(view.waveform_state(), Some(AnalysisState::Queued));
        assert!(view.poll_due(20 + ANALYSIS_POLL_MS));
    }

    #[test]
    fn a_missing_file_is_reported_as_missing_not_queued() {
        let mut view = ready(vec![waveform(AnalysisState::Analysed)]);
        let refusal = StoreApiError::Refused {
            status: 409,
            code: Some("unavailable".into()),
            message: "the media file is missing".into(),
        };
        view.apply_reanalyse(None, Err(refusal), 5);
        assert_eq!(view.waveform_state(), Some(AnalysisState::Unavailable));
        assert!(view.explanation().unwrap().contains("missing"));
        assert!(!view.poll_due(u64::MAX));

        let mut other = ready(vec![waveform(AnalysisState::Failed)]);
        other.apply_reanalyse(None, Err(StoreApiError::Offline("no route".into())), 5);
        assert_eq!(other.waveform_state(), Some(AnalysisState::Failed), "unchanged");
        assert!(other.note.as_deref().unwrap().contains("no route"));
    }

    #[test]
    fn an_unreadable_status_offers_try_again_and_a_late_answer_is_timed_out() {
        let mut view = AnalysisView::new(7, 0, "T".into(), 100);
        assert!(!view.timed_out(100 + STORE_TIMEOUT_MS - 1));
        assert!(view.timed_out(100 + STORE_TIMEOUT_MS));
        view.apply_status(Err(StoreApiError::Offline("down".into())), 200);
        assert_eq!(view.actions(), vec![AnalysisAction::Retry]);
        assert!(view.explanation().unwrap().contains("down"));
        view.restart(300);
        assert_eq!(view.phase, AnalysisPhase::Loading);
        assert!(view.waiting);
        assert!(view.actions().is_empty(), "nothing to press while loading");
    }
}
