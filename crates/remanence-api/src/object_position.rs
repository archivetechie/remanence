//! Object origins have one owner, independent of plaintext/encrypted decoding.
use remanence_library::{DriveHandle, SpaceKind, TapeIoError, TapePosition};
/// A filemark-delimited tape-file ordinal measured from BOT, never a relative skip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BotRelativeTapeFile(pub u64);

/// A borrowed drive and its established bootstrap origin. Raw drives need a
/// rewind; a mounted cartridge may retain the cursor left by BOT verification.
pub struct ObjectPositionOrigin<'a> {
    drive: &'a mut DriveHandle,
    after_bootstrap: bool,
}
impl<'a> From<&'a mut DriveHandle> for ObjectPositionOrigin<'a> {
    fn from(drive: &'a mut DriveHandle) -> Self {
        Self {
            drive,
            after_bootstrap: false,
        }
    }
}
impl<'a> From<&'a mut crate::MountedTape> for ObjectPositionOrigin<'a> {
    fn from(mounted: &'a mut crate::MountedTape) -> Self {
        let after_bootstrap = mounted.identity_generation == mounted.drive.identity_generation()
            && mounted
                .drive
                .known_position()
                .is_some_and(|p| p.partition == 0 && p.lba == 1);
        Self {
            drive: &mut mounted.drive,
            after_bootstrap,
        }
    }
}
/// Establish the object origin exactly once and return SPACE's position proof.
/// Positive file ordinals can reuse the verified bootstrap cursor: no filemark
/// has yet been crossed. File zero and unknown origins require BOT explicitly.
pub fn position_to_object<'a>(
    source: impl Into<ObjectPositionOrigin<'a>>,
    origin: BotRelativeTapeFile,
) -> Result<TapePosition, TapeIoError> {
    let count = i64::try_from(origin.0).map_err(|_| {
        TapeIoError::InvalidRequest(remanence_library::ScsiError::InvalidInput(
            "tape-file number exceeds SPACE range",
        ))
    })?;
    let source = source.into();
    if !source.after_bootstrap || count == 0 {
        source.drive.rewind()?;
    }
    Ok(source
        .drive
        .space(count, SpaceKind::Filemarks)?
        .position_after)
}
