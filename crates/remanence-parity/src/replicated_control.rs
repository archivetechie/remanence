//! Shared checked geometry for replicated control tape files.

use crate::error::ParityError;

/// Physical `copy 1 + copy 2 + footer` layout used by large control files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReplicatedControlLayout {
    pub(crate) copy_block_count: u64,
    pub(crate) total_block_count: u64,
    pub(crate) primary_copy_start_block: u64,
    pub(crate) tail_copy_start_block: u64,
    pub(crate) footer_block_index: u64,
}

/// Calculate the exact `2M + 1` layout without converting through `usize`.
pub(crate) fn checked_replicated_control_layout(
    block_size: u64,
    header_len: u64,
    payload_len: u64,
    structure: &'static str,
) -> Result<ReplicatedControlLayout, ParityError> {
    if block_size == 0 {
        return Err(layout_error(structure, "block size must be non-zero"));
    }
    if header_len > block_size {
        return Err(layout_error(
            structure,
            "fixed header does not fit in one tape block",
        ));
    }
    let copy_bytes = header_len
        .checked_add(payload_len)
        .ok_or_else(|| layout_error(structure, "header plus payload length overflows u64"))?;
    let copy_block_count = copy_bytes.div_ceil(block_size);
    let tail_copy_start_block = copy_block_count;
    let footer_block_index = copy_block_count
        .checked_mul(2)
        .ok_or_else(|| layout_error(structure, "footer block index overflows u64"))?;
    let total_block_count = footer_block_index
        .checked_add(1)
        .ok_or_else(|| layout_error(structure, "total block count overflows u64"))?;

    Ok(ReplicatedControlLayout {
        copy_block_count,
        total_block_count,
        primary_copy_start_block: 0,
        tail_copy_start_block,
        footer_block_index,
    })
}

/// Validate persisted locator fields against the one shared layout function.
#[allow(clippy::too_many_arguments)]
pub(crate) fn validate_replicated_control_layout(
    block_size: u64,
    header_len: u64,
    payload_len: u64,
    copy_block_count: u64,
    total_block_count: u64,
    primary_copy_start_block: u64,
    tail_copy_start_block: u64,
    footer_block_index: u64,
    structure: &'static str,
) -> Result<(), ParityError> {
    let expected =
        checked_replicated_control_layout(block_size, header_len, payload_len, structure)?;
    if copy_block_count != expected.copy_block_count
        || total_block_count != expected.total_block_count
        || primary_copy_start_block != expected.primary_copy_start_block
        || tail_copy_start_block != expected.tail_copy_start_block
        || footer_block_index != expected.footer_block_index
    {
        return Err(layout_error(
            structure,
            format!(
                "locator geometry does not match 2M+1 layout: got copy={copy_block_count}, total={total_block_count}, primary={primary_copy_start_block}, tail={tail_copy_start_block}, footer={footer_block_index}; expected copy={}, total={}, primary={}, tail={}, footer={}",
                expected.copy_block_count,
                expected.total_block_count,
                expected.primary_copy_start_block,
                expected.tail_copy_start_block,
                expected.footer_block_index,
            ),
        ));
    }
    Ok(())
}

fn layout_error(structure: &'static str, message: impl Into<String>) -> ParityError {
    ParityError::ReplicatedControlLayout {
        structure,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_exact_at_and_across_a_block_boundary() {
        let exact = checked_replicated_control_layout(256, 64, 192, "test").unwrap();
        assert_eq!(exact.copy_block_count, 1);
        assert_eq!(exact.total_block_count, 3);

        let crossed = checked_replicated_control_layout(256, 64, 193, "test").unwrap();
        assert_eq!(crossed.copy_block_count, 2);
        assert_eq!(crossed.tail_copy_start_block, 2);
        assert_eq!(crossed.footer_block_index, 4);
        assert_eq!(crossed.total_block_count, 5);
    }

    #[test]
    fn layout_rejects_every_overflow_boundary() {
        let header_add = checked_replicated_control_layout(512, 512, u64::MAX, "test")
            .expect_err("header plus payload must not wrap");
        assert!(header_add.to_string().contains("overflows u64"));

        let doubled = checked_replicated_control_layout(1, 1, u64::MAX - 1, "test")
            .expect_err("replicated copy count must not wrap");
        assert!(doubled.to_string().contains("footer block index"));
    }
}
