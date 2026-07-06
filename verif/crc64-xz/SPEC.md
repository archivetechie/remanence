# crc64-xz formal specification

Target: `crates/remanence-crc/src/lib.rs`, the shared CRC implementation used
by parity sidecars and the append-only audit log.

CRC-64/XZ parameters, as specified by `specs/rem-parity-1.0-specification.md`
Section 5.1:

- width 64
- polynomial `0x42F0E1EBA9EA3693`
- reflected input and output, implemented with reflected polynomial
  `0xC96C5795D7870F42`
- initial value `0xFFFF_FFFF_FFFF_FFFF`
- final XOR `0xFFFF_FFFF_FFFF_FFFF`

## X1 -- reflected bit step

`crc64_xz_bit_step(crc)` is the reflected CRC recurrence:

- if the low bit is set, return `(crc >> 1) ^ 0xC96C5795D7870F42`
- otherwise return `crc >> 1`

The Lean proof also proves the equivalent branch-free mask form of this step.

## X2 -- byte table entry

`crc64_xz_table_entry(byte)` is exactly eight applications of X1 starting from
`byte` zero-extended to 64 bits.

## X3 -- table update

`crc64_xz_update(crc, byte)` uses the standard reflected table index
`(crc ^ byte) & 0xff`, shifts the previous state by one byte, and XORs the X2
table entry.

## X4 -- public CRC

`crc64_xz(bytes)` folds X3 from the initial all-ones state and applies the
final all-ones XOR.

## X5 -- normative vectors

The Rust tests assert the repository's normative vectors:

```text
crc64("123456789")        = 0x995DC9BBDF1939FA
crc64("")                 = 0
crc64([0x00])             = 0x1FADA17364673F59
crc64([0xFF])             = 0xFF00000000000000
crc64(0x00 x 262144)      = 0x261BDF3D299838FC
crc64(0xFF x 262144)      = 0x55433DD0F38908BA
```

## Trust anchor

The Lean type checker (`lake build` with zero local placeholders) is the trust
anchor for the proof artifacts. The Rust `drift_guard` test ties this extraction
back to `crates/remanence-crc/src/lib.rs`.
