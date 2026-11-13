//! Timestamp-paste import (NB_05): turn a published timestamped tracklist or chapter list
//! into a Noubin `nav` [`CueList`].
//!
//! [`parse_cue_paste`] is a pure function; the preview endpoint and the save path both call
//! it, so what is previewed is exactly what is saved.
//!
//! # Line format
//!
//! ```text
//! 0:00 Intro
//! - 03:45 - Wicked Babylon - Clippers
//! 2. [1:02:10] Closing set
//! ```
//!
//! * Optional leading list markers (`-`, `*`, `•`, `–`, `—`, `[ ]`, `[x]`, `1.`, `1)`), then an
//!   optional `[` / `(`, then the timestamp, an optional closing bracket, then the title.
//! * The timestamp is a run of digits, `:` and `.` parsed with the Noubin Lazy Timestamp
//!   ([`NoubinTimestamp`]). It must contain a `:` or a `.` fraction: a bare `120` is
//!   ambiguous with list numbering and is **not** a cue.
//! * It must be followed by whitespace, a separator or the end of the line (`10:30am` is not a
//!   cue). One separator (`-`, `–`, `—`, `:`, `|`, …) between timestamp and title is
//!   stripped; hyphens inside the title are kept.
//! * Lines without a leading timestamp are *ignored* (reported, not errors); blank lines are
//!   skipped silently.
//! * Cues are sorted by time; a second cue at the same time is dropped (first line wins) with
//!   a warning. A first cue after 0 is allowed (no synthetic cue) and noted as a warning.

use crate::noudata_types::{Cue, CueList, CueListType, CueTextSpan};
use crate::timestamp::NoubinTimestamp;
use serde::Serialize;
use serde_json::Map;

/// Longest cue text kept (bytes; NBSecurityRequirements §3 "cue text ≤ 2 KiB").
pub const MAX_CUE_TEXT_BYTES: usize = 2048;
/// Most cues kept from one paste (the engine publishes at most 1500 list entries).
pub const MAX_PASTE_CUES: usize = 1000;
/// Longest ignored-line excerpt echoed back to clients (chars).
const MAX_IGNORED_ECHO_CHARS: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCue {
    /// 1-based line number in the pasted text.
    pub line: usize,
    pub start: NoubinTimestamp,
    /// Sanitised, single-line title.
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IgnoredLine {
    /// 1-based line number.
    pub line: usize,
    /// Excerpt of the line (sanitised, at most 200 chars).
    pub text: String,
    pub reason: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CueWarningKind {
    /// Two lines had the same time; the later one was dropped.
    DuplicateTime,
    /// Lines were not in time order; they were sorted.
    Unsorted,
    /// The first cue is not at 0:00 (allowed, no cue is invented).
    FirstCueNotAtZero,
    /// Cue starts after the media length; dropped.
    BeyondLength,
    /// Title longer than 2 KiB; truncated.
    TextTruncated,
    /// More than [`MAX_PASTE_CUES`] cues; the latest ones were dropped.
    TooManyCues,
    /// Title contained control / bidi / zero-width characters that were removed.
    TextSanitised,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CueWarning {
    pub kind: CueWarningKind,
    /// 1-based line, when the warning belongs to one line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    pub message: String,
}

/// Result of [`parse_cue_paste`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CuePaste {
    /// Sorted by `start`, strictly increasing.
    pub cues: Vec<ParsedCue>,
    pub ignored_lines: Vec<IgnoredLine>,
    pub warnings: Vec<CueWarning>,
}

impl CuePaste {
    /// Drop cues that start after the real media length (call once the length is known).
    pub fn drop_beyond_length(&mut self, length_ms: u64) {
        let warnings = &mut self.warnings;
        self.cues.retain(|c| {
            let keep = c.start.as_millis() <= length_ms;
            if !keep {
                warnings.push(CueWarning {
                    kind: CueWarningKind::BeyondLength,
                    line: Some(c.line),
                    message: format!(
                        "{} is beyond the media length ({}); cue dropped",
                        c.start,
                        format_hms_ms(length_ms)
                    ),
                });
            }
            keep
        });
    }

    /// The `nav` cue list for these cues: `cueListType: nav`, navigable, no `endTime`s
    /// (each cue ends at the next start / the item end).
    pub fn to_cue_list(&self, title: Option<&str>) -> CueList {
        CueList {
            cue_list_type: Some(CueListType::Nav),
            cue_list_title: title.map(str::to_string),
            cue_list_language: None,
            cue_list_navigable: Some(true),
            cues: self
                .cues
                .iter()
                .map(|c| Cue {
                    start_time: c.start,
                    end_time: None,
                    cue_text_spans: vec![CueTextSpan {
                        text: c.text.clone(),
                        extra: Map::new(),
                    }],
                    cue_url: None,
                    extra: Map::new(),
                })
                .collect(),
            extra: Map::new(),
        }
    }
}

/// `HH:MM:SS.mmm` (always with milliseconds; the preview form).
pub fn format_hms_ms(ms: u64) -> String {
    let s = ms / 1000;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        s / 3600,
        (s / 60) % 60,
        s % 60,
        ms % 1000
    )
}

/// Paste text that re-parses to the same cues: one `timestamp title` line per cue.
pub fn cue_list_to_paste_text(list: &CueList) -> String {
    let mut out = String::new();
    for cue in &list.cues {
        let title = sanitise_cue_text(&cue.text().replace(['\n', '\r'], " "));
        out.push_str(&cue.start_time.to_string());
        // A title that itself starts with a separator needs an explicit one to round-trip.
        if title.chars().next().is_some_and(is_separator) {
            out.push_str(" -");
        }
        out.push(' ');
        out.push_str(&title);
        out.push('\n');
    }
    out
}

fn is_separator(c: char) -> bool {
    matches!(c, '-' | '\u{2010}'..='\u{2015}' | '\u{2212}' | ':' | '|')
}

/// Characters removed from every cue title: C0/C1 controls, bidi overrides/isolates,
/// zero-width/formatting marks, BOM, line/paragraph separators (NBSecurityRequirements §3).
fn is_stripped(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2028}' | '\u{2029}'
                | '\u{2060}'
                | '\u{2066}'..='\u{2069}'
                | '\u{061C}'
                | '\u{FEFF}'
        )
}

/// Make untrusted text safe for a single-line cue title: tabs become spaces, other
/// stripped characters vanish, ends are trimmed, and the result is cut to
/// [`MAX_CUE_TEXT_BYTES`] on a character boundary.
pub fn sanitise_cue_text(s: &str) -> String {
    truncate_bytes(strip_text(s).trim(), MAX_CUE_TEXT_BYTES).to_string()
}

fn strip_text(s: &str) -> String {
    s.chars()
        .filter_map(|c| {
            if c == '\t' {
                Some(' ')
            } else if is_stripped(c) {
                None
            } else {
                Some(c)
            }
        })
        .collect()
}

fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].trim_end()
}

fn excerpt(line: &str) -> String {
    let clean: String = line
        .chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .filter(|c| !is_stripped(*c))
        .collect();
    let clean = clean.trim();
    if clean.chars().count() <= MAX_IGNORED_ECHO_CHARS {
        clean.to_string()
    } else {
        let cut: String = clean.chars().take(MAX_IGNORED_ECHO_CHARS).collect();
        format!("{cut}…")
    }
}

/// Split into lines on `\n`, `\r\n`, `\r`, U+0085, U+2028 and U+2029; drops a leading BOM.
fn split_lines(text: &str) -> Vec<&str> {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '\n' | '\u{85}' | '\u{2028}' | '\u{2029}' => {
                lines.push(&text[start..i]);
                start = i + c.len_utf8();
            }
            '\r' => {
                lines.push(&text[start..i]);
                start = i + 1;
                if let Some(&(_, '\n')) = chars.peek() {
                    chars.next();
                    start += 1;
                }
            }
            _ => {}
        }
    }
    lines.push(&text[start..]);
    lines
}

/// Strip list markers from the start of a line. Returns the remainder.
fn strip_markers(mut s: &str) -> &str {
    loop {
        let t = s.trim_start();
        // `[ ]` / `[x]` checkboxes.
        if let Some(rest) = t
            .strip_prefix("[ ]")
            .or_else(|| t.strip_prefix("[x]").or_else(|| t.strip_prefix("[X]")))
        {
            s = rest;
            continue;
        }
        // Bullets: `-`, `*`, `•`, dashes — only when followed by whitespace, so `-5` stays.
        let mut cs = t.chars();
        if let Some(c) = cs.next() {
            if (is_separator(c) && c != ':' && c != '|' || matches!(c, '*' | '•' | '·' | '▪' | '‣'))
                && cs.as_str().starts_with(char::is_whitespace)
            {
                s = cs.as_str();
                continue;
            }
        }
        // Numbering: `12.` / `12)` followed by whitespace.
        let digits = t.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && digits <= 4 {
            let rest = &t[digits..];
            if let Some(after) = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')')) {
                if after.starts_with(char::is_whitespace) {
                    s = after;
                    continue;
                }
            }
        }
        return t;
    }
}

enum LineResult {
    Cue(NoubinTimestamp, String),
    Ignored(&'static str),
}

fn parse_line(raw: &str) -> LineResult {
    let s = strip_markers(raw);
    let s = s
        .strip_prefix('[')
        .or_else(|| s.strip_prefix('('))
        .unwrap_or(s);
    let tok_len = s
        .bytes()
        .take_while(|b| b.is_ascii_digit() || *b == b':' || *b == b'.')
        .count();
    if tok_len == 0 {
        return LineResult::Ignored("no leading timestamp");
    }
    // A trailing `:` / `.` belongs to the separator ("3:45: Title", "3:45. Title").
    let token = s[..tok_len].trim_end_matches([':', '.']);
    let rest = &s[token.len()..];
    if !token.starts_with(|c: char| c.is_ascii_digit()) {
        return LineResult::Ignored("no leading timestamp");
    }
    if !(token.contains(':') || token.contains('.')) {
        return LineResult::Ignored("number without ':' is not a timestamp");
    }
    let Ok(start) = NoubinTimestamp::parse(token) else {
        return LineResult::Ignored("not a valid timestamp");
    };
    // The timestamp must end at whitespace / separator / bracket / end of line.
    let rest = rest.trim_start_matches([':', '.']);
    let after_bracket = rest
        .strip_prefix(']')
        .or_else(|| rest.strip_prefix(')'))
        .unwrap_or(rest);
    let ok_boundary = after_bracket.is_empty()
        || after_bracket.starts_with(|c: char| c.is_whitespace() || is_separator(c) || c == '|');
    if !ok_boundary {
        return LineResult::Ignored("timestamp not followed by a title");
    }
    let mut title = after_bracket.trim_start();
    if let Some(c) = title.chars().next() {
        if is_separator(c) {
            title = title[c.len_utf8()..].trim_start();
        }
    }
    LineResult::Cue(start, title.to_string())
}

/// Parse pasted timestamps into cues. Pure; never fails (problems are `ignored_lines` /
/// `warnings`).
pub fn parse_cue_paste(text: &str) -> CuePaste {
    let mut out = CuePaste::default();
    let mut found: Vec<ParsedCue> = Vec::new();

    for (i, raw) in split_lines(text).into_iter().enumerate() {
        let line = i + 1;
        if raw.trim().is_empty() {
            continue;
        }
        match parse_line(raw) {
            LineResult::Ignored(reason) => out.ignored_lines.push(IgnoredLine {
                line,
                text: excerpt(raw),
                reason,
            }),
            LineResult::Cue(start, title) => {
                let stripped = strip_text(&title);
                let clean = sanitise_cue_text(&title);
                if clean.is_empty() {
                    out.ignored_lines.push(IgnoredLine {
                        line,
                        text: excerpt(raw),
                        reason: "timestamp without a title",
                    });
                    continue;
                }
                if stripped.trim().len() > MAX_CUE_TEXT_BYTES {
                    out.warnings.push(CueWarning {
                        kind: CueWarningKind::TextTruncated,
                        line: Some(line),
                        message: "title longer than 2 KiB was truncated".into(),
                    });
                } else if stripped.chars().count() != title.chars().count() {
                    out.warnings.push(CueWarning {
                        kind: CueWarningKind::TextSanitised,
                        line: Some(line),
                        message: "control or invisible characters were removed from the title"
                            .into(),
                    });
                }
                found.push(ParsedCue {
                    line,
                    start,
                    text: clean,
                });
            }
        }
    }

    if found.windows(2).any(|w| w[1].start < w[0].start) {
        out.warnings.push(CueWarning {
            kind: CueWarningKind::Unsorted,
            line: None,
            message: "lines were not in time order; cues were sorted".into(),
        });
    }
    found.sort_by_key(|c| c.start); // stable: equal times keep line order
    let mut cues: Vec<ParsedCue> = Vec::with_capacity(found.len());
    for c in found {
        if cues.last().is_some_and(|p| p.start == c.start) {
            out.warnings.push(CueWarning {
                kind: CueWarningKind::DuplicateTime,
                line: Some(c.line),
                message: format!(
                    "another cue already starts at {}; this one was dropped",
                    format_hms_ms(c.start.as_millis())
                ),
            });
            continue;
        }
        cues.push(c);
    }
    if cues.len() > MAX_PASTE_CUES {
        out.warnings.push(CueWarning {
            kind: CueWarningKind::TooManyCues,
            line: None,
            message: format!(
                "{} cues found; only the first {MAX_PASTE_CUES} were kept",
                cues.len()
            ),
        });
        cues.truncate(MAX_PASTE_CUES);
    }
    if let Some(first) = cues.first() {
        if first.start != NoubinTimestamp::ZERO {
            out.warnings.push(CueWarning {
                kind: CueWarningKind::FirstCueNotAtZero,
                line: Some(first.line),
                message: format!(
                    "the first cue starts at {}, not 0:00; nothing is inserted before it",
                    format_hms_ms(first.start.as_millis())
                ),
            });
        }
    }
    out.cues = cues;
    out
}
