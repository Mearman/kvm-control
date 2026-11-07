//! Rendering device-reported text safely.

/// Whether a character can change how a terminal or text view renders the text around it: control characters (C0, DEL and C1, which include the CSI introducer U+009B) and the Unicode bidirectional embedding, override and isolate controls (U+202A to U+202E and U+2066 to U+2069).
pub fn is_terminal_unsafe(c: char) -> bool {
    c.is_control() || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Rewrites serialised JSON so that every terminal-unsafe character inside a string is a `\uXXXX` escape, leaving the decoded value unchanged.
///
/// `json` must be JSON text, such as `serde_json` output. Raw tab, line feed and carriage return can only be whitespace between tokens there, because a serialiser escapes them inside strings, so they are kept; every other terminal-unsafe character can only occur inside a string, where JSON permits any character to be written as an escape. All terminal-unsafe characters are in the Basic Multilingual Plane, so one escape represents each.
pub fn escape_json_for_terminal(json: &str) -> String {
    let mut escaped = String::with_capacity(json.len());
    for c in json.chars() {
        if is_terminal_unsafe(c) && !matches!(c, '\t' | '\n' | '\r') {
            escaped.push_str(&format!("\\u{:04x}", u32::from(c)));
        } else {
            escaped.push(c);
        }
    }
    escaped
}
