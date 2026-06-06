//! Storage-shape sanitizers.
//!
//! Cross-cutting helpers for projecting free-form strings (client
//! request ids, filenames, log lines) into the shape that is
//! safe to embed in a filesystem path, JSON key, or
//! developer-facing log line.

/// Project a free-form string into the shape that is safe to embed
/// in a storage path, filename, or log line: keep ASCII
/// alphanumerics and `-` `_` `.` verbatim, replace any other
/// printable ASCII character with `_`, drop control characters
/// and non-ASCII, and truncate to `max_chars` codepoints.
///
/// Returns `None` if the projection is empty, so callers can
/// distinguish "input had no usable characters" from "input
/// collapsed to an empty string".
///
/// The policy matches the previous `safe_segment` /
/// `safe_request_id_segment` behavior on alphanumeric, `-_`, `.`,
/// and ASCII control handling; it unifies them so the onair-proxy
/// `inspector_text` consumer can adopt the same shape.
pub fn sanitize_for_storage(value: &str, max_chars: usize) -> Option<String> {
    let segment = value
        .chars()
        .filter_map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                Some(character)
            } else if character.is_ascii() && !character.is_ascii_control() {
                Some('_')
            } else {
                None
            }
        })
        .take(max_chars)
        .collect::<String>();
    (!segment.is_empty()).then_some(segment)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_alphanumerics_and_safe_punctuation() {
        let value = sanitize_for_storage("req-abc_123.v2", 80).unwrap();
        assert_eq!(value, "req-abc_123.v2");
    }

    #[test]
    fn sanitize_replaces_unsafe_ascii_with_underscore() {
        let value = sanitize_for_storage("req/test value", 80).unwrap();
        assert_eq!(value, "req_test_value");
    }

    #[test]
    fn sanitize_drops_control_and_non_ascii() {
        let value = sanitize_for_storage("a\x00b\x07c\u{3042}d", 80).unwrap();
        assert_eq!(value, "abcd");
    }

    #[test]
    fn sanitize_truncates_to_max_chars() {
        let value = sanitize_for_storage("abcdefghij", 4).unwrap();
        assert_eq!(value, "abcd");
    }

    #[test]
    fn sanitize_returns_none_for_all_dropped_input() {
        assert!(sanitize_for_storage("\x00\x07", 80).is_none());
    }
}
