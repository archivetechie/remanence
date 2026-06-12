//! Minimal tar header encoding used by `rao-v1`.

use crate::error::FormatError;
use crate::model::TAR_RECORD_SIZE;

/// Tar typeflag for a regular file.
pub const TYPE_REGULAR: u8 = b'0';
/// Tar typeflag for a symbolic link.
pub const TYPE_SYMLINK: u8 = b'2';
/// Tar typeflag for a directory.
pub const TYPE_DIRECTORY: u8 = b'5';
/// Tar typeflag for a pax extended header attached to the next entry.
pub const TYPE_PAX_EXTENDED: u8 = b'x';
/// Tar typeflag for a global pax header.
pub const TYPE_PAX_GLOBAL: u8 = b'g';

/// Largest value representable in the POSIX ustar 12-octet size field.
pub const USTAR_SIZE_FIELD_MAX: u64 = 0o77777777777;

const PAX_BACKED_PATH_PLACEHOLDER: &str = "remanence/pax-path";
const PAX_BACKED_LINK_PLACEHOLDER: &str = "remanence/pax-linkpath";

/// Encode a POSIX ustar header record.
pub fn encode_header(
    path: &str,
    size: u64,
    typeflag: u8,
    mode: u32,
) -> Result<[u8; 512], FormatError> {
    encode_header_with_link(path, size, typeflag, mode, "")
}

fn encode_header_with_link(
    path: &str,
    size: u64,
    typeflag: u8,
    mode: u32,
    linkname: &str,
) -> Result<[u8; 512], FormatError> {
    validate_header_path(path)?;
    validate_link_name(linkname)?;
    let mut block = [0u8; TAR_RECORD_SIZE];
    write_name(&mut block, path);
    write_octal(&mut block[100..108], mode as u64)?;
    write_octal(&mut block[108..116], 0)?;
    write_octal(&mut block[116..124], 0)?;
    write_octal(&mut block[124..136], size)?;
    write_octal(&mut block[136..148], 0)?;
    for byte in &mut block[148..156] {
        *byte = b' ';
    }
    block[156] = typeflag;
    write_name(&mut block[157..257], linkname);
    block[257..263].copy_from_slice(b"ustar\0");
    block[263..265].copy_from_slice(b"00");
    write_name_field(&mut block[265..297], b"remanence");
    write_name_field(&mut block[297..329], b"remanence");
    let checksum: u64 = block.iter().map(|&b| b as u64).sum();
    write_checksum(&mut block[148..156], checksum)?;
    Ok(block)
}

/// Encode a regular-file ustar header whose path/size are authoritative in
/// the immediately preceding pax extended header.
///
/// POSIX pax readers use the `path` and `size` extended header records, but
/// the following ustar header still needs to be a well-formed ISO-646 header.
/// Non-ASCII/long paths therefore get an ASCII placeholder here, and sizes
/// beyond the ustar octal field get a valid zero placeholder.
pub fn encode_pax_backed_regular_header(
    path: &str,
    size: u64,
    mode: u32,
) -> Result<[u8; 512], FormatError> {
    let header_path = if is_portable_ustar_name(path) {
        path
    } else {
        PAX_BACKED_PATH_PLACEHOLDER
    };
    let header_size = if size <= USTAR_SIZE_FIELD_MAX {
        size
    } else {
        0
    };
    encode_header(header_path, header_size, TYPE_REGULAR, mode)
}

/// Encode a pax-backed symbolic-link header.
pub fn encode_pax_backed_symlink_header(
    path: &str,
    target: &str,
    target_in_pax: bool,
) -> Result<[u8; 512], FormatError> {
    let header_path = if is_portable_ustar_name(path) {
        path
    } else {
        PAX_BACKED_PATH_PLACEHOLDER
    };
    let linkname = if target_in_pax {
        PAX_BACKED_LINK_PLACEHOLDER
    } else {
        target
    };
    encode_header_with_link(header_path, 0, TYPE_SYMLINK, 0o777, linkname)
}

/// Encode a pax-backed directory header.
pub fn encode_pax_backed_directory_header(path: &str) -> Result<[u8; 512], FormatError> {
    let header_path = if is_portable_ustar_name(path) {
        path
    } else {
        PAX_BACKED_PATH_PLACEHOLDER
    };
    encode_header(header_path, 0, TYPE_DIRECTORY, 0o755)
}

/// Return true when a symlink target can fit directly in ustar `linkname`.
pub(crate) fn is_portable_ustar_linkname(target: &str) -> bool {
    let bytes = target.as_bytes();
    bytes.len() <= 100 && bytes.iter().all(|&byte| byte != 0 && byte >= 0x20)
}

fn validate_header_path(path: &str) -> Result<(), FormatError> {
    if path.is_empty() {
        return Err(FormatError::invalid("tar path must not be empty"));
    }
    if path.as_bytes().contains(&0) {
        return Err(FormatError::invalid("tar path must not contain NUL"));
    }
    Ok(())
}

fn validate_link_name(linkname: &str) -> Result<(), FormatError> {
    if linkname.as_bytes().contains(&0) {
        return Err(FormatError::invalid("tar linkname must not contain NUL"));
    }
    if linkname.bytes().any(|byte| byte < 0x20) {
        return Err(FormatError::invalid(
            "tar linkname must not contain ASCII control characters",
        ));
    }
    Ok(())
}

fn write_name(block: &mut [u8], path: &str) {
    let bytes = path.as_bytes();
    if bytes.len() <= block.len() {
        block[..bytes.len()].copy_from_slice(bytes);
        return;
    }

    let marker = b"remanence/pax-path";
    block[..marker.len()].copy_from_slice(marker);
}

fn is_portable_ustar_name(path: &str) -> bool {
    let bytes = path.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 100
        && bytes
            .iter()
            .all(|&byte| byte != 0 && byte.is_ascii() && !byte.is_ascii_control())
}

fn write_name_field(field: &mut [u8], value: &[u8]) {
    let len = value.len().min(field.len());
    field[..len].copy_from_slice(&value[..len]);
}

fn write_octal(field: &mut [u8], value: u64) -> Result<(), FormatError> {
    let digits = field.len().saturating_sub(1);
    let encoded = format!("{value:0digits$o}");
    if encoded.len() > digits {
        return Err(FormatError::layout(format!(
            "tar octal field overflow for value {value}"
        )));
    }
    field.fill(0);
    field[..encoded.len()].copy_from_slice(encoded.as_bytes());
    Ok(())
}

fn write_checksum(field: &mut [u8], value: u64) -> Result<(), FormatError> {
    let encoded = format!("{value:06o}\0 ");
    if encoded.len() != field.len() {
        return Err(FormatError::layout("tar checksum encoding length mismatch"));
    }
    field.copy_from_slice(encoded.as_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_header_with_valid_checksum() {
        let header = encode_header("hello.txt", 12, TYPE_REGULAR, 0o644).unwrap();
        let mut checksum_header = header;
        for byte in &mut checksum_header[148..156] {
            *byte = b' ';
        }
        let expected: u64 = checksum_header.iter().map(|&b| b as u64).sum();
        let stored = std::str::from_utf8(&header[148..154]).unwrap();
        assert_eq!(u64::from_str_radix(stored.trim(), 8).unwrap(), expected);
        assert_eq!(&header[257..263], b"ustar\0");
    }

    #[test]
    fn pax_backed_header_uses_ascii_placeholder_for_non_ascii_path() {
        let header = encode_pax_backed_regular_header("vidéo/clip.mov", 12, 0o644).unwrap();
        assert_eq!(
            &header[..PAX_BACKED_PATH_PLACEHOLDER.len()],
            PAX_BACKED_PATH_PLACEHOLDER.as_bytes()
        );
        assert!(header[..100].iter().all(u8::is_ascii));
    }

    #[test]
    fn pax_backed_header_keeps_large_size_field_valid() {
        let header =
            encode_pax_backed_regular_header("large.bin", USTAR_SIZE_FIELD_MAX + 1, 0o644).unwrap();
        let stored = std::str::from_utf8(&header[124..135]).unwrap();
        assert_eq!(stored, "00000000000");
    }
}
