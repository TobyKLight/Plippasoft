//! Tiny declarative command-line parser (no external crate): `--key value`, `--key=value`, `--flag`.
//! Every sub-command declares the options it accepts, so a typo is an error, not a silent default.

use anyhow::{anyhow, bail, Result};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Default, Clone)]
pub struct Args {
    pub positional: Vec<String>,
    opts: HashMap<String, String>,
    flags: HashSet<String>,
}

impl Args {
    /// `value_opts` take a value, `flag_opts` do not. `--help` / `-h` is always accepted as a flag.
    /// A value may start with `-` (negative rates): the token after a value option is always its value.
    pub fn parse(argv: &[String], value_opts: &[&str], flag_opts: &[&str]) -> Result<Args> {
        let mut a = Args::default();
        let mut i = 0;
        while i < argv.len() {
            let tok = &argv[i];
            i += 1;
            if tok == "-h" || tok == "--help" {
                a.flags.insert("help".into());
                continue;
            }
            let Some(name) = tok.strip_prefix("--") else {
                a.positional.push(tok.clone());
                continue;
            };
            let (name, inline) = match name.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (name, None),
            };
            if flag_opts.contains(&name) {
                if inline.is_some() {
                    bail!("option --{name} does not take a value");
                }
                a.flags.insert(name.to_string());
            } else if value_opts.contains(&name) {
                let v = match inline {
                    Some(v) => v,
                    None => {
                        let v = argv
                            .get(i)
                            .ok_or_else(|| anyhow!("option --{name} needs a value"))?
                            .clone();
                        i += 1;
                        v
                    }
                };
                a.opts.insert(name.to_string(), v);
            } else {
                bail!("unknown option --{name} (try --help)");
            }
        }
        Ok(a)
    }

    pub fn help(&self) -> bool {
        self.flags.contains("help")
    }

    pub fn flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.opts.get(name).map(String::as_str)
    }

    pub fn string(&self, name: &str, default: &str) -> String {
        self.get(name).unwrap_or(default).to_string()
    }

    pub fn f64(&self, name: &str, default: f64) -> Result<f64> {
        match self.get(name) {
            None => Ok(default),
            Some(v) => v
                .trim()
                .parse::<f64>()
                .map_err(|_| anyhow!("--{name}: '{v}' is not a number")),
        }
    }

    pub fn usize(&self, name: &str, default: usize) -> Result<usize> {
        match self.get(name) {
            None => Ok(default),
            Some(v) => v
                .trim()
                .parse::<usize>()
                .map_err(|_| anyhow!("--{name}: '{v}' is not a whole number")),
        }
    }

    pub fn f64_list(&self, name: &str, default: &[f64]) -> Result<Vec<f64>> {
        match self.get(name) {
            None => Ok(default.to_vec()),
            Some(v) => parse_f64_list(v).map_err(|e| anyhow!("--{name}: {e}")),
        }
    }
}

pub fn parse_f64_list(s: &str) -> Result<Vec<f64>> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        out.push(
            p.parse::<f64>()
                .map_err(|_| anyhow!("'{p}' is not a number"))?,
        );
    }
    if out.is_empty() {
        bail!("empty list");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parses_values_flags_and_positionals() {
        let a = Args::parse(
            &v(&[
                "a.mp3",
                "--rates",
                "-0.25,-0.5",
                "--mode=correct",
                "--out",
                "x.wav",
                "b.flac",
                "--quiet",
            ]),
            &["rates", "mode", "out"],
            &["quiet"],
        )
        .unwrap();
        assert_eq!(a.positional, vec!["a.mp3", "b.flac"]);
        assert_eq!(a.f64_list("rates", &[1.0]).unwrap(), vec![-0.25, -0.5]);
        assert_eq!(a.string("mode", "shift"), "correct");
        assert_eq!(a.get("out"), Some("x.wav"));
        assert!(a.flag("quiet"));
        assert!(!a.help());
    }

    #[test]
    fn defaults_and_errors() {
        let a = Args::parse(&v(&[]), &["hold"], &[]).unwrap();
        assert_eq!(a.f64("hold", 6.0).unwrap(), 6.0);
        assert_eq!(a.f64_list("hold", &[1.0, 2.0]).unwrap(), vec![1.0, 2.0]);
        assert!(Args::parse(&v(&["--nope"]), &["hold"], &[]).is_err());
        assert!(Args::parse(&v(&["--hold"]), &["hold"], &[]).is_err());
        let b = Args::parse(&v(&["--hold", "abc"]), &["hold"], &[]).unwrap();
        assert!(b.f64("hold", 1.0).is_err());
        assert!(Args::parse(&v(&["--quiet=1"]), &[], &["quiet"]).is_err());
    }

    #[test]
    fn help_is_always_accepted() {
        assert!(Args::parse(&v(&["--help"]), &[], &[]).unwrap().help());
        assert!(Args::parse(&v(&["-h"]), &[], &[]).unwrap().help());
    }

    #[test]
    fn list_parsing() {
        assert_eq!(parse_f64_list("1, 2.5,3").unwrap(), vec![1.0, 2.5, 3.0]);
        assert!(parse_f64_list("1,x").is_err());
        assert!(parse_f64_list(",").is_err());
    }
}
