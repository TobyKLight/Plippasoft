//! Cheap structural guard run on untrusted JSON **before** it is parsed
//! (`NBSecurityRequirements.md` §2.1): the root must be an object, nesting depth <= 32 and
//! at most 10 000 array elements + object keys in total. The scan is string-aware, allocates
//! nothing and never recurses, so it is safe on hostile input of any shape.

/// Deepest nesting accepted.
pub const MAX_JSON_DEPTH: usize = 32;
/// Most array elements + object keys accepted in one document.
pub const MAX_JSON_ELEMENTS: usize = 10_000;

/// Why a document was refused by [`check_json_object_limits`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonLimitError {
    Empty,
    /// First significant byte is not `{` (HTML, XML, an array, a bare value ...).
    NotAnObject,
    TooDeep,
    TooManyElements,
}

impl std::fmt::Display for JsonLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            JsonLimitError::Empty => "empty document",
            JsonLimitError::NotAnObject => "not a JSON object",
            JsonLimitError::TooDeep => "JSON nested too deeply",
            JsonLimitError::TooManyElements => "JSON has too many elements",
        })
    }
}

impl std::error::Error for JsonLimitError {}

/// First byte that is not whitespace / a UTF-8 BOM, for content sniffing.
pub fn first_significant_byte(bytes: &[u8]) -> Option<u8> {
    let b = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    b.iter()
        .copied()
        .find(|c| !matches!(c, b' ' | b'\t' | b'\r' | b'\n'))
}

/// Structural limits check; does **not** validate JSON (the parser does that afterwards).
pub fn check_json_object_limits(bytes: &[u8]) -> Result<(), JsonLimitError> {
    check_json_object_limits_with(bytes, MAX_JSON_DEPTH, MAX_JSON_ELEMENTS)
}

/// [`check_json_object_limits`] with explicit limits: the local `.noudata` guard allows far
/// more elements than a fetched document (a long audiobook is legitimately big) but the same
/// depth.
pub fn check_json_object_limits_with(
    bytes: &[u8],
    max_depth: usize,
    max_elements: usize,
) -> Result<(), JsonLimitError> {
    match first_significant_byte(bytes) {
        None => return Err(JsonLimitError::Empty),
        Some(b'{') => {}
        Some(_) => return Err(JsonLimitError::NotAnObject),
    }
    let mut depth = 0usize;
    let mut elements = 0usize;
    // Kind of each open container: true = array.
    let mut stack: Vec<bool> = Vec::with_capacity(max_depth.min(256) + 1);
    let mut in_string = false;
    let mut escaped = false;
    // Just after `[`: the next non-space byte starts the first element (unless `]`).
    let mut expect_first = false;
    for &b in bytes {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        if expect_first && !matches!(b, b' ' | b'\t' | b'\r' | b'\n') {
            expect_first = false;
            if b != b']' {
                elements += 1;
            }
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > max_depth {
                    return Err(JsonLimitError::TooDeep);
                }
                stack.push(b == b'[');
                expect_first = b == b'[';
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                stack.pop();
            }
            b':' => elements += 1,
            b',' if stack.last() == Some(&true) => elements += 1,
            _ => {}
        }
        if elements > max_elements {
            return Err(JsonLimitError::TooManyElements);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_normal_and_bom() {
        assert_eq!(
            check_json_object_limits(br#"{"a":[1,2,{"b":"x"}]}"#),
            Ok(())
        );
        assert_eq!(check_json_object_limits(b"\xEF\xBB\xBF \n{}"), Ok(()));
    }

    #[test]
    fn refuses_non_objects() {
        for b in [&b"<html>"[..], b"[1]", b"\"x\"", b"42", b"<?xml"] {
            assert_eq!(
                check_json_object_limits(b),
                Err(JsonLimitError::NotAnObject)
            );
        }
        assert_eq!(
            check_json_object_limits(b"  \n"),
            Err(JsonLimitError::Empty)
        );
    }

    #[test]
    fn depth_limit_ignores_brackets_in_strings() {
        let ok = format!("{{\"a\":{}1{}}}", "[".repeat(30), "]".repeat(30));
        assert_eq!(check_json_object_limits(ok.as_bytes()), Ok(()));
        let bad = format!("{{\"a\":{}1{}}}", "[".repeat(40), "]".repeat(40));
        assert_eq!(
            check_json_object_limits(bad.as_bytes()),
            Err(JsonLimitError::TooDeep)
        );
        let quoted = format!("{{\"a\":\"{}\"}}", "[".repeat(500));
        assert_eq!(check_json_object_limits(quoted.as_bytes()), Ok(()));
        let escaped = "{\"a\":\"\\\"[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[\"}";
        assert_eq!(check_json_object_limits(escaped.as_bytes()), Ok(()));
    }

    #[test]
    fn element_limit() {
        let arr = vec!["1"; MAX_JSON_ELEMENTS + 5].join(",");
        let bad = format!("{{\"a\":[{arr}]}}");
        assert_eq!(
            check_json_object_limits(bad.as_bytes()),
            Err(JsonLimitError::TooManyElements)
        );
        let arr = vec!["1"; 5000].join(",");
        let ok = format!("{{\"a\":[{arr}]}}");
        assert_eq!(check_json_object_limits(ok.as_bytes()), Ok(()));
        let keys = (0..MAX_JSON_ELEMENTS + 1)
            .map(|i| format!("\"k{i}\":1"))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(
            check_json_object_limits(format!("{{{keys}}}").as_bytes()),
            Err(JsonLimitError::TooManyElements)
        );
    }
}
