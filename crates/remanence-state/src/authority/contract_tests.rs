//! Backend-independent behavior contracts. Each implementation supplies an
//! exclusive writer plus semantic commit/plan fixtures; assertions are shared.
use super::AuthorityFinalization;
use super::{TapeAuthoritySnapshot, TapeAuthorityWriter};
use crate::{StateError, TerminalFinalizationProgress};
use remanence_parity::CommitCursor;

pub(crate) fn view_writer_contract<W>(
    writer: &mut W,
    mut first: super::AuthorityCommit,
    second: super::AuthorityCommit,
) where
    W: TapeAuthorityWriter<
        Error = StateError,
        Report = super::AuthorityCommit,
        Commit = super::AuthorityCommit,
        Finalization = AuthorityFinalization,
        Progress = TerminalFinalizationProgress,
    >,
    W::Snapshot: TapeAuthoritySnapshot<Payload = crate::authority::CatalogPayload>,
{
    let object_id = first.payloads[0].projection.object.object_id.clone();
    first.payloads[0]
        .files
        .push(crate::NativeObjectFileProjectionInput {
            object_id,
            file_id: "member".into(),
            path: "payload.txt".into(),
            size_bytes: 1,
            file_sha256: vec![0x77; 32],
            first_chunk_lba: Some(0),
            chunk_count: 1,
            mtime: Some("1.25".into()),
            executable: Some(true),
        });
    assert_eq!(writer.authority_summary().unwrap().cursor, None);
    writer.publish_report(&first).unwrap();
    assert_eq!(writer.latest_commit().unwrap(), Some(first.clone()));
    let frozen = writer.freeze().unwrap();
    let summary = writer.authority_summary().unwrap();
    assert_eq!(summary.object_count, 1);
    assert_eq!(summary.checkpoint_ordinal, 1);
    assert_eq!(
        summary.cursor,
        Some(CommitCursor {
            partition: 0,
            lba: 6
        })
    );
    assert_eq!(
        (summary.next_file, summary.protected, summary.total),
        (2, 0, 3)
    );
    let mut inconsistent = second.clone();
    inconsistent.total += 1;
    assert!(writer.publish_report(&inconsistent).is_err());
    assert_eq!(writer.latest_commit().unwrap(), Some(first.clone()));
    writer.publish_report(&second).unwrap();
    assert_eq!(frozen.summary().object_count, 1);
    assert!(frozen.finalization().is_none());
    let mut rows = Vec::new();
    frozen
        .visit_file_rows(&mut |row| {
            rows.push(row.clone());
            Ok(())
        })
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].physical_start_hint, Some(2));
    let mut objects = Vec::new();
    frozen
        .visit_object_rows(&mut |row| {
            objects.push(row.clone());
            Ok(())
        })
        .unwrap();
    assert_eq!(
        objects,
        vec![first.payloads[0].projection.object_recovery_row.clone()]
    );
    let mut payloads = Vec::new();
    frozen
        .visit_payloads(&mut |object| {
            payloads.push(object.clone());
            Ok(())
        })
        .unwrap();
    assert_eq!(
        payloads,
        vec![
            crate::authority::CatalogPayload::Object(Box::new(
                first.payloads[0].projection.clone()
            )),
            crate::authority::CatalogPayload::Member(first.payloads[0].files[0].clone()),
            crate::authority::CatalogPayload::ObjectEnd
        ]
    );
    assert_eq!(writer.authority_summary().unwrap().object_count, 2);
    assert_eq!(
        writer.replay_commits().unwrap(),
        vec![first.clone(), second.clone()]
    );
    let mut commits = 0;
    writer
        .visit_commits(&mut |_| {
            commits += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(commits, 2);
    frozen
        .visit_payloads(&mut |_| {
            let mut count = 0;
            frozen.visit_object_rows(&mut |_| {
                count += 1;
                Ok(())
            })?;
            assert_eq!(count, 1);
            Ok(())
        })
        .unwrap();
    assert!(frozen
        .visit_payloads(&mut |_| Err(StateError::JournalReplayFailed("visitor refusal".into())))
        .unwrap_err()
        .to_string()
        .contains("visitor refusal"));
    assert!(
        writer.publish_report(&first).is_err(),
        "regression must fail"
    );
    assert_eq!(writer.authority_summary().unwrap().object_count, 2);
}

pub(crate) fn finalization_contract<W>(writer: &mut W, plan: AuthorityFinalization)
where
    W: TapeAuthorityWriter<
        Error = StateError,
        Finalization = AuthorityFinalization,
        Progress = TerminalFinalizationProgress,
    >,
{
    assert_eq!(writer.finalization_status().unwrap(), None);
    assert_eq!(writer.pending_finalization().unwrap(), None);
    assert!(writer.recover_finalization().is_err());
    assert_eq!(writer.begin_finalization(&plan).unwrap(), plan);
    assert_eq!(writer.begin_finalization(&plan).unwrap(), plan);
    let before = TerminalFinalizationProgress::BeforeReplicaA;
    let after = TerminalFinalizationProgress::AfterReplicaA;
    assert!(writer
        .advance_finalization(before, TerminalFinalizationProgress::AfterReplicaB)
        .is_err());
    assert!(
        writer
            .require_finalization_recovery()
            .unwrap()
            .recovery_required
    );
    let progressed = writer.advance_finalization(before, after).unwrap();
    assert_eq!(progressed.progress, after);
    assert!(!progressed.recovery_required);
    assert_eq!(
        writer.advance_finalization(before, after).unwrap(),
        progressed
    );
    assert_eq!(writer.finalization_status().unwrap(), Some(progressed));
}

/// Shared structural writer contract: explicit operations reject ambiguous
/// dispatch before writing, and retries report the units actually appended.
pub(crate) fn structural_writer_contract<W: TapeAuthorityWriter>(writer: &mut W) {
    use remanence_parity::authority::{FilePublication, FilePurpose, PublicationDisposition};
    let prefix = FilePublication {
        purpose: FilePurpose::TerminalPrefix,
        files: Vec::new(),
        protected: 0,
        total: 0,
        cursor: None,
        check: None,
        seal_due: None,
    };
    assert!(writer.publish_commit(&prefix).is_err());
    let component = FilePublication {
        purpose: FilePurpose::TerminalComponent,
        ..prefix.clone()
    };
    assert!(writer.publish_commit(&component).is_err());
    let error = writer.publish_terminal_component(&prefix).unwrap_err();
    assert!(error
        .to_string()
        .contains("invalid terminal-component/checkpoint transition"));
    assert!(writer.publish_terminal_prefix(&component).is_err());
    let receipt = writer.publish_terminal_prefix(&prefix).unwrap();
    assert_eq!(receipt.items.len(), 1);
    assert!(receipt
        .items
        .iter()
        .all(|unit| unit.disposition == PublicationDisposition::Appended
            && unit.durability == PublicationDisposition::Appended));
    assert_eq!(receipt.items[0].publication, prefix);
    let retry = writer.publish_terminal_prefix(&prefix).unwrap();
    assert!(retry.items.iter().all(|unit| unit.disposition
        == PublicationDisposition::AlreadyPresent
        && unit.durability == PublicationDisposition::AlreadyPresent));
}
