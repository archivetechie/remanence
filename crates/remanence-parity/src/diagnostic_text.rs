//! Validation and output escaping for writer-supplied diagnostic text.
//!
//! Bootstrap keys 3/4 and parity-map keys 6/7 share these exact funnels so
//! their writer and reader behavior cannot drift. The byte escaper is also
//! used by the CLI for untrusted archive member names.

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

pub(crate) const WRITER_VERSION_MAX_BYTES: usize = 128;
pub(crate) const WRITE_TIMESTAMP_MAX_BYTES: usize = 64;

pub(crate) const WRITER_VERSION_LENGTH_BOUND: &str = "maximum 128-byte length";
pub(crate) const WRITER_VERSION_CHARSET_BOUND: &str = "printable US-ASCII";
pub(crate) const WRITE_TIMESTAMP_LENGTH_BOUND: &str = "maximum 64-byte length";
pub(crate) const WRITE_TIMESTAMP_RFC3339_BOUND: &str = "RFC3339 date-time";

/// Validate the shared writer-version length and character-set rule.
pub(crate) fn validate_writer_version(value: &str) -> Result<(), &'static str> {
    if value.len() > WRITER_VERSION_MAX_BYTES {
        return Err(WRITER_VERSION_LENGTH_BOUND);
    }
    if !value.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
        return Err(WRITER_VERSION_CHARSET_BOUND);
    }
    Ok(())
}

/// Validate the shared write-timestamp length and RFC3339 grammar rule.
pub(crate) fn validate_write_timestamp(value: &str) -> Result<(), &'static str> {
    if value.len() > WRITE_TIMESTAMP_MAX_BYTES {
        return Err(WRITE_TIMESTAMP_LENGTH_BOUND);
    }
    let has_rfc3339_separator = value
        .as_bytes()
        .get(10)
        .is_some_and(|byte| matches!(byte, b'T' | b't'));
    if !has_rfc3339_separator || OffsetDateTime::parse(value, &Rfc3339).is_err() {
        return Err(WRITE_TIMESTAMP_RFC3339_BOUND);
    }
    Ok(())
}

/// Record that an on-tape diagnostic value was normalized to absence.
pub(crate) fn log_ignored_diagnostic_text(
    structure: &'static str,
    key: u8,
    field: &'static str,
    violated_bound: &'static str,
) {
    tracing::warn!(
        target: "remanence_parity::diagnostic_text",
        structure,
        key,
        field,
        violated_bound,
        "writer-supplied diagnostic text treated as absent"
    );
}

/// Escape untrusted bytes for a human-readable output channel.
///
/// Valid UTF-8 remains readable, backslashes are doubled, C0/DEL controls are
/// rendered as hexadecimal byte escapes, and invalid UTF-8 bytes are escaped
/// individually. This was originally the CLI's archive-member-name escaper;
/// keeping one implementation avoids divergent safety behavior.
pub fn escape_member_name(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        match std::str::from_utf8(&bytes[cursor..]) {
            Ok(valid) => {
                escape_valid_member_name_chunk(valid, &mut out);
                break;
            }
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                if valid_up_to > 0 {
                    let valid = std::str::from_utf8(&bytes[cursor..cursor + valid_up_to])
                        .expect("valid_up_to returned valid UTF-8 prefix");
                    escape_valid_member_name_chunk(valid, &mut out);
                    cursor += valid_up_to;
                }
                if let Some(error_len) = error.error_len() {
                    for byte in &bytes[cursor..cursor + error_len] {
                        push_hex_escape(&mut out, *byte);
                    }
                    cursor += error_len;
                } else {
                    for byte in &bytes[cursor..] {
                        push_hex_escape(&mut out, *byte);
                    }
                    break;
                }
            }
        }
    }
    out
}

fn escape_valid_member_name_chunk(valid: &str, out: &mut String) {
    for ch in valid.chars() {
        if ch == '\\' {
            out.push_str("\\\\");
        } else if matches!(ch, '\u{0}'..='\u{1f}' | '\u{7f}') {
            for byte in ch.to_string().as_bytes() {
                push_hex_escape(out, *byte);
            }
        } else {
            out.push(ch);
        }
    }
}

fn push_hex_escape(out: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push('\\');
    out.push('x');
    out.push(char::from(HEX[(byte >> 4) as usize]));
    out.push(char::from(HEX[(byte & 0x0f) as usize]));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_validation_accepts_utc_and_numeric_offsets() {
        assert!(validate_write_timestamp("2026-01-01T00:00:00Z").is_ok());
        assert!(validate_write_timestamp("2026-01-01T05:30:00+05:30").is_ok());
    }

    #[test]
    fn rfc3339_validation_rejects_bare_date_and_free_text() {
        assert_eq!(
            validate_write_timestamp("2026-01-01"),
            Err(WRITE_TIMESTAMP_RFC3339_BOUND)
        );
        assert_eq!(
            validate_write_timestamp("sometime tomorrow"),
            Err(WRITE_TIMESTAMP_RFC3339_BOUND)
        );
    }

    #[test]
    fn writer_version_validation_enforces_bytes_and_printable_ascii() {
        assert!(validate_writer_version(&"v".repeat(WRITER_VERSION_MAX_BYTES)).is_ok());
        assert_eq!(
            validate_writer_version(&"v".repeat(WRITER_VERSION_MAX_BYTES + 1)),
            Err(WRITER_VERSION_LENGTH_BOUND)
        );
        assert_eq!(
            validate_writer_version("writer\x1b[2J"),
            Err(WRITER_VERSION_CHARSET_BOUND)
        );
    }

    #[test]
    fn escaping_removes_raw_terminal_escape_bytes() {
        let rendered = escape_member_name(b"writer\x1b[2J");
        assert_eq!(rendered, "writer\\x1b[2J");
        assert!(!rendered.as_bytes().contains(&0x1b));
    }
}
