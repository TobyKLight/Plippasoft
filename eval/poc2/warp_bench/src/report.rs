//! Console tables and the plain-text report file `warp_bench_<date>.txt`.
//!
//! Every sub-command prints a table, then appends a section `== name ==` whose body is the table plus
//! machine-readable `key = value` lines. `suggest` reads only those `key = value` lines (last one wins),
//! so the owner can also edit a number by hand before running `warp_bench suggest`.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fmt::Display;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub const LISTENING_HEADER: &str = "== listening ==";

/// Column-aligned text table.
pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    pub fn new(headers: &[&str]) -> Self {
        Table {
            headers: headers.iter().map(|s| s.to_string()).collect(),
            rows: Vec::new(),
        }
    }

    pub fn row<S: Into<String>>(&mut self, cells: Vec<S>) {
        self.rows.push(cells.into_iter().map(Into::into).collect());
    }

    pub fn render(&self) -> String {
        let cols = self.headers.len();
        let mut widths: Vec<usize> = self.headers.iter().map(|h| h.chars().count()).collect();
        for r in &self.rows {
            for (i, c) in r.iter().enumerate().take(cols) {
                widths[i] = widths[i].max(c.chars().count());
            }
        }
        let line = |cells: &[String]| -> String {
            let mut s = String::new();
            for (i, w) in widths.iter().enumerate() {
                let c = cells.get(i).map(String::as_str).unwrap_or("");
                s.push_str(c);
                if i + 1 < cols {
                    for _ in c.chars().count()..*w + 2 {
                        s.push(' ');
                    }
                }
            }
            s.trim_end().to_string()
        };
        let mut out = String::new();
        out.push_str(&line(&self.headers));
        out.push('\n');
        let rule: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
        out.push_str(&line(&rule));
        out.push('\n');
        for r in &self.rows {
            out.push_str(&line(r));
            out.push('\n');
        }
        out
    }
}

/// Collects `key = value` lines for the machine-readable part of a section.
#[derive(Default)]
pub struct Data(Vec<(String, String)>);

impl Data {
    pub fn push<V: Display>(&mut self, key: &str, value: V) {
        self.0.push((key.to_string(), value.to_string()));
    }

    /// Floats are written with fixed precision so reports diff cleanly.
    pub fn push_f(&mut self, key: &str, value: f64, decimals: usize) {
        self.0
            .push((key.to_string(), format!("{value:.decimals$}")));
    }

    pub fn render(&self) -> String {
        let mut s = String::new();
        for (k, v) in &self.0 {
            s.push_str(&format!("{k} = {v}\n"));
        }
        s
    }
}

/// Sanitise a file name into a key fragment (`My Mix.mp3` -> `My_Mix_mp3`).
pub fn key_fragment(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Shorten a file name for a table cell: keeps the start and the extension (`Bogan Mongrel - Gr...ne.mp3`).
pub fn short_name(name: &str, max: usize) -> String {
    let n = name.chars().count();
    if n <= max || max < 10 {
        return name.to_string();
    }
    let ext_len = name
        .rfind('.')
        .map(|i| name[i..].chars().count())
        .unwrap_or(0)
        .min(6);
    let tail: String = name.chars().skip(n - ext_len - 2).collect();
    let head: String = name.chars().take(max - tail.chars().count() - 3).collect();
    format!("{head}...{tail}")
}

/// Parse every `key = value` line of a results file; later lines win.
pub fn parse_results(text: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with("==") {
            continue;
        }
        let Some((k, v)) = line.split_once(" = ") else {
            continue;
        };
        let k = k.trim();
        if k.is_empty() || k.contains(char::is_whitespace) {
            continue;
        }
        m.insert(k.to_string(), v.trim().to_string());
    }
    m
}

/// `YYYY-MM-DD` (UTC) for the report file name.
pub fn today() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(secs.div_euclid(86400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 to (year, month, day) (Howard Hinnant's algorithm).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub struct Report {
    pub path: PathBuf,
}

impl Report {
    pub fn default_path() -> PathBuf {
        PathBuf::from(format!("warp_bench_{}.txt", today()))
    }

    pub fn open(path: Option<&str>) -> Report {
        Report {
            path: path.map(PathBuf::from).unwrap_or_else(Report::default_path),
        }
    }

    /// Append `== name ==` + body. The `== listening ==` sheet, if present, is kept as the last section.
    pub fn append(&self, name: &str, body: &str) -> Result<()> {
        let existing = std::fs::read_to_string(&self.path).unwrap_or_default();
        let new = append_section(&existing, name, body);
        std::fs::write(&self.path, new)
            .with_context(|| format!("write report {}", self.path.display()))
    }

    /// Make sure the (empty) listening sheet is the trailing section.
    pub fn ensure_listening_sheet(&self) -> Result<()> {
        let existing = std::fs::read_to_string(&self.path).unwrap_or_default();
        if !existing.contains(LISTENING_HEADER) {
            let new = append_section(&existing, "listening", &listening_sheet());
            std::fs::write(&self.path, new)
                .with_context(|| format!("write report {}", self.path.display()))?;
        }
        Ok(())
    }
}

/// Pure part of [`Report::append`].
pub fn append_section(existing: &str, name: &str, body: &str) -> String {
    let (head, listening) = match existing.find(LISTENING_HEADER) {
        Some(i) => (&existing[..i], Some(&existing[i..])),
        None => (existing, None),
    };
    let mut out = head.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if let (true, Some(l)) = (name == "listening", listening) {
        // never duplicate the sheet
        out.push_str(l);
        return out;
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&format!("== {name} ==\n"));
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    if let Some(l) = listening {
        out.push('\n');
        out.push_str(l);
    }
    out
}

/// The blank listening sheet (the same table is in the README). One row per test case.
pub fn listening_sheet() -> String {
    let mut s = String::new();
    s.push_str("# Fill the verdict column: ok / minor / bad plus a few words (what you heard, at what time).\n");
    s.push_str("# Then paste this whole file back. Artefact names are in the README.\n");
    for (test, case) in LISTENING_ROWS {
        s.push_str(&format!("{test:<34} {case:<22} verdict: \n"));
    }
    s
}

/// (test, case) rows of the listening sheet.
pub const LISTENING_ROWS: &[(&str, &str)] = &[
    ("speed shift (varispeed)", "10%"),
    ("speed shift (varispeed)", "25%"),
    ("speed shift (varispeed)", "75%"),
    ("speed shift (varispeed)", "125%"),
    ("speed shift (varispeed)", "150%"),
    ("speed shift (varispeed)", "200%"),
    ("speed correct (stretch)", "10%"),
    ("speed correct (stretch)", "25%"),
    ("speed correct (stretch)", "75%"),
    ("speed correct (stretch)", "125%"),
    ("speed correct (stretch)", "150%"),
    ("speed correct (stretch)", "200%"),
    ("scan shift fwd", "4x then 8x, release"),
    ("scan shift rev", "4x then 8x, release"),
    ("scan correct fwd", "2x"),
    ("scan correct fwd", "3x"),
    ("scan correct fwd", "4x"),
    ("reverse", "-0.25x"),
    ("reverse", "-0.5x"),
    ("reverse", "-1x"),
    ("reverse", "-2x"),
    ("jump shift fwd", "0.1 s"),
    ("jump shift fwd", "1 s"),
    ("jump shift fwd", "5 s"),
    ("jump shift fwd", "10 s"),
    ("jump shift fwd", "60 s"),
    ("jump shift back", "0.1 s"),
    ("jump shift back", "1 s"),
    ("jump shift back", "5 s"),
    ("jump shift back", "10 s"),
    ("jump shift back", "60 s"),
    ("jump correct fwd", "1 s"),
    ("jump correct fwd", "5 s"),
    ("jump correct fwd", "60 s"),
    ("xfade over tone", "5 ms"),
    ("xfade over tone", "10 ms"),
    ("xfade over tone", "20 ms"),
    ("xfade over tone", "40 ms"),
    ("xfade over tone", "60 ms"),
    ("xfade over music", "5 ms"),
    ("xfade over music", "10 ms"),
    ("xfade over music", "20 ms"),
    ("xfade over music", "40 ms"),
    ("xfade over music", "60 ms"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(20_000), (2024, 10, 4));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        let t = today();
        assert_eq!(t.len(), 10);
        assert_eq!(&t[4..5], "-");
    }

    #[test]
    fn table_aligns_columns() {
        let mut t = Table::new(&["name", "x"]);
        t.row(vec!["a".to_string(), "1".to_string()]);
        t.row(vec!["longer".to_string(), "22".to_string()]);
        let s = t.render();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines[0], "name    x");
        assert_eq!(lines[1], "------  --");
        assert_eq!(lines[2], "a       1");
        assert_eq!(lines[3], "longer  22");
    }

    #[test]
    fn results_parse_last_wins_and_ignore_prose() {
        let text = "== decode ==\nfile  x realtime\nfoo.mp3  100.0\ndecode.worst_median_x = 50.0\n# note = ignore\n\ndecode.worst_median_x = 70.5\ninfo.throttled = 0x0\n";
        let m = parse_results(text);
        assert_eq!(m.get("decode.worst_median_x").unwrap(), "70.5");
        assert_eq!(m.get("info.throttled").unwrap(), "0x0");
        assert!(!m.contains_key("note"));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn sections_keep_listening_sheet_last_and_never_duplicate_it() {
        let a = append_section("", "info", "x = 1\n");
        assert_eq!(a, "== info ==\nx = 1\n");
        let b = append_section(&a, "listening", &listening_sheet());
        assert!(b.contains(LISTENING_HEADER));
        let c = append_section(&b, "decode", "y = 2");
        let di = c.find("== decode ==").unwrap();
        let li = c.find(LISTENING_HEADER).unwrap();
        assert!(di < li, "decode must precede the listening sheet");
        let d = append_section(&c, "listening", &listening_sheet());
        assert_eq!(d.matches(LISTENING_HEADER).count(), 1);
        assert_eq!(parse_results(&d).get("y").unwrap(), "2");
    }

    #[test]
    fn long_names_are_shortened_for_tables() {
        assert_eq!(short_name("a.mp3", 30), "a.mp3");
        let s = short_name(
            "Bogan Mongrel - Grooves For Machines To Study - 02 Beat.mp3",
            30,
        );
        assert_eq!(s.chars().count(), 30);
        assert!(s.starts_with("Bogan Mongrel") && s.ends_with("at.mp3") && s.contains("..."));
    }

    #[test]
    fn listening_sheet_has_one_row_per_case() {
        let s = listening_sheet();
        assert_eq!(
            s.lines().filter(|l| l.contains("verdict:")).count(),
            LISTENING_ROWS.len()
        );
        assert_eq!(key_fragment("My Mix (1).mp3"), "My_Mix__1__mp3");
    }
}
