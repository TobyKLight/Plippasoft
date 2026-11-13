/// Extract the media lookup key from NDEF text/URI (e.g. `https://play1` → `play1`).
pub fn media_key_from_ndef(raw: &str) -> String {
    let s = raw.trim();
    let after_scheme = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    let key = after_scheme
        .split(&['/', '?', '#'][..])
        .next()
        .unwrap_or(after_scheme)
        .trim();
    key.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_play1() {
        assert_eq!(media_key_from_ndef("https://play1"), "play1");
    }

    #[test]
    fn plain_play1() {
        assert_eq!(media_key_from_ndef("play1"), "play1");
    }
}
