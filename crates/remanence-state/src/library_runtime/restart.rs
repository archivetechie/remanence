//! Restart bookkeeping derives open work from authenticated sources. Native
//! write sessions stay native and unresolved until physical recovery; a broad
//! native end reason never invents a library quarantine cause or endpoint.
use super::*;
use std::collections::BTreeMap;

impl LibraryRuntime {
    /// Mint the identity of actual autonomous work at its source boundary.
    pub fn start_owned_operation(
        &self,
        kind: lib::OperationKind,
        actor: lib::AuditActor,
    ) -> Result<lib::Id, RuntimeError> {
        let mut operation = None;
        self.append(|a, v| {
            let (id, receipt) = a.start_operation(kind, actor, lib::SourceLayer::Layer5, v)?;
            operation = Some(id);
            Ok(receipt)
        })?;
        operation.ok_or_else(|| {
            StateError::JournalReplayFailed("operation writer returned no identity".into()).into()
        })
    }

    /// Inspect a request's canonical acceptance and current lifecycle without
    /// minting work. Missing SQL never participates in this retry lookup.
    pub fn operation_request(
        &self,
        kind: lib::OperationKind,
        actor: &str,
        key: lib::Id,
    ) -> Result<Option<(lib::OperationRecorded, lib::Accepted)>, RuntimeError> {
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        let view = self
            .0
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut found = None;
        let replay = self
            .0
            .authority
            .snapshot()?
            .replay(&view, &mut |applied| {
                if let lib::BodyView::OperationRecorded(value) = applied.record().body_view() {
                    if value.kind == kind {
                        if let lib::OperationEvent::Accepted(accepted) = &value.event {
                            if accepted.actor_fingerprint.as_str() == actor
                                && accepted.idempotency_key == Some(key)
                            {
                                found = Some((value.clone(), accepted.clone()));
                            }
                        }
                        if let Some((latest, _)) = &mut found {
                            if latest.operation == value.operation {
                                *latest = value.clone();
                            }
                        }
                    }
                }
                Ok(())
            })
            .map_err(AuthorityError::Store)?;
        if replay.refusal.is_some() || !replay.fold.pending.is_empty() {
            return Err(StateError::JournalReplayFailed(
                "operation lookup needs resolved authority".into(),
            )
            .into());
        }
        Ok(found)
    }

    /// Bind a real accepted request and return its authenticated current event.
    /// The caller decides whether a closed reply is read-only; this never reopens it.
    pub fn accept_operation(
        &self,
        kind: lib::OperationKind,
        accepted: lib::Accepted,
        actor: lib::AuditActor,
    ) -> Result<(lib::Id, lib::OperationEvent), RuntimeError> {
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        let view = self
            .0
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut found = None;
        let mut latest = None;
        let replay = self
            .0
            .authority
            .snapshot()?
            .replay(&view, &mut |applied| {
                if let lib::BodyView::OperationRecorded(value) = applied.record().body_view() {
                    if value.kind == kind {
                        if let lib::OperationEvent::Accepted(prior) = &value.event {
                            if accepted.idempotency_key.is_some()
                                && prior.idempotency_key == accepted.idempotency_key
                                && prior.actor_fingerprint == accepted.actor_fingerprint
                            {
                                if prior != &accepted {
                                    return Err(lib::FoldError::ConflictingDuplicate.into());
                                }
                                found = Some(value.operation);
                            }
                        }
                        if found == Some(value.operation) {
                            latest = Some(value.event.clone())
                        }
                    }
                }
                Ok(())
            })
            .map_err(AuthorityError::Store)?;
        if replay.refusal.is_some() || !replay.fold.pending.is_empty() {
            return Err(StateError::JournalReplayFailed(
                "request acceptance needs resolved source".into(),
            )
            .into());
        }
        if let Some(operation) = found {
            self.catch_up_locked()?;
            return Ok((operation, latest.expect("accepted source event")));
        }
        let mut operation = None;
        let event = lib::OperationEvent::Accepted(accepted.clone());
        self.append_locked(|a, v| {
            let (id, receipt) =
                a.begin_operation(kind, accepted, actor, lib::SourceLayer::Layer5, v)?;
            operation = Some(id);
            Ok(receipt)
        })?;
        Ok((
            operation.ok_or_else(|| {
                StateError::JournalReplayFailed("operation writer returned no identity".into())
            })?,
            event,
        ))
    }

    /// Publish a producer's actual operation event using its authenticated head.
    /// Exact terminal retries return the original state without appending again.
    pub fn record_operation_event(
        &self,
        operation: lib::Id,
        kind: lib::OperationKind,
        event: lib::OperationEvent,
        actor: lib::AuditActor,
    ) -> Result<(), RuntimeError> {
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        let view = self
            .0
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut head = None;
        let replay = self
            .0
            .authority
            .snapshot()?
            .replay(&view, &mut |applied| {
                if let lib::BodyView::OperationRecorded(value) = applied.record().body_view() {
                    if value.operation == operation {
                        head = Some((
                            value.clone(),
                            applied.record().reference(applied.sequence()),
                        ));
                    }
                }
                Ok(())
            })
            .map_err(AuthorityError::Store)?;
        if replay.refusal.is_some() || !replay.fold.pending.is_empty() {
            return Err(StateError::JournalReplayFailed(
                "operation transition requires resolved source".into(),
            )
            .into());
        }
        let (prior, reference) = head.ok_or_else(|| {
            StateError::JournalReplayFailed("operation has no writer-minted source identity".into())
        })?;
        if prior.kind != kind {
            return Err(StateError::JournalReplayFailed("operation kind changed".into()).into());
        }
        if prior.event == event {
            self.catch_up_locked()?;
            return Ok(());
        }
        self.append_locked(|a, v| {
            a.operation_recorded(
                lib::OperationRecorded {
                    operation,
                    kind,
                    previous: Some(reference),
                    event,
                },
                actor,
                lib::SourceLayer::Layer5,
                v,
            )
        })?;
        Ok(())
    }
    /// Publish a non-write session selected by its owner. The immutable source
    /// head and tape incarnation are resolved under the same dispatch lease.
    #[allow(clippy::too_many_arguments)]
    pub fn observe_session(
        &self,
        session_id: lib::Id,
        kind: lib::SessionKind,
        endpoint: Option<&remanence_library::DriveKey>,
        tape_uuid: Option<lib::Id>,
        event: lib::SessionEvent,
        reason: Option<String>,
        actor: lib::AuditActor,
        drive_identity: (Option<lib::Id>, Option<String>),
    ) -> Result<(), RuntimeError> {
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        let view = self
            .0
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut head = None;
        let endpoint = endpoint.map(super::motion::endpoint);
        let mut drives = BTreeMap::<lib::Id, (Option<String>, bool)>::new();
        let mut locations = BTreeMap::<lib::DriveKey, lib::Id>::new();
        let mut original_drive = None;
        let select = |drives: &BTreeMap<lib::Id, (Option<String>, bool)>,
                      locations: &BTreeMap<lib::DriveKey, lib::Id>| {
            let key = endpoint.as_ref()?;
            if let Some(id) = locations.get(key) {
                return drives
                    .get(id)
                    .filter(|(_, retired)| !retired)
                    .map(|(serial, _)| (*id, serial.clone()));
            }
            if let lib::DriveKey::Standalone(key) = key {
                let mut candidates = drives.iter().filter(|(_, (serial, retired))| {
                    !retired && serial.as_deref() == Some(key.drive_serial.as_str())
                });
                let first = candidates
                    .next()
                    .map(|(id, (serial, _))| (*id, serial.clone()));
                return if candidates.next().is_none() {
                    first
                } else {
                    None
                };
            }
            None
        };
        let replay = self
            .0
            .authority
            .snapshot()?
            .replay(&view, &mut |applied| {
                match applied.record().body_view() {
                    lib::BodyView::DriveObserved(value) => {
                        let retired = drives
                            .get(&value.drive_uuid)
                            .is_some_and(|(_, retired)| *retired);
                        drives.insert(
                            value.drive_uuid,
                            (
                                value.serial.as_ref().map(|s| s.as_str().to_owned()),
                                retired,
                            ),
                        );
                        locations.retain(|_, id| *id != value.drive_uuid);
                        if let Some(endpoint) = &value.endpoint {
                            locations.insert(endpoint.clone(), value.drive_uuid);
                        }
                    }
                    lib::BodyView::DriveDecision(value) => {
                        if let Some((_, retired)) = drives.get_mut(&value.drive_uuid) {
                            match value.decision {
                                lib::Stewardship::Retire(_) => *retired = true,
                                lib::Stewardship::Reinstate(_) => *retired = false,
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                if let lib::BodyView::SessionObserved(value) = applied.record().body_view() {
                    if value.session_id == session_id {
                        if original_drive.is_none() {
                            original_drive = Some(select(&drives, &locations));
                        }
                        head = Some((
                            value.clone(),
                            applied.record().reference(applied.sequence()),
                        ));
                    }
                }
                Ok(())
            })
            .map_err(AuthorityError::Store)?;
        if replay.refusal.is_some() || !replay.fold.pending.is_empty() {
            return Err(StateError::JournalReplayFailed(
                "session transition requires resolved source".into(),
            )
            .into());
        }
        let proved_drive = original_drive.unwrap_or_else(|| select(&drives, &locations));
        let proved_serial = proved_drive
            .as_ref()
            .and_then(|(_, serial)| serial.as_deref())
            .or_else(|| match endpoint.as_ref() {
                Some(lib::DriveKey::Standalone(key)) => Some(key.drive_serial.as_str()),
                _ => None,
            });
        if drive_identity
            .0
            .is_some_and(|id| proved_drive.as_ref().map(|(known, _)| *known) != Some(id))
            || drive_identity
                .1
                .as_deref()
                .is_some_and(|serial| proved_serial != Some(serial))
        {
            return Err(StateError::JournalReplayFailed(
                "read session drive identity differs from its authenticated creation source".into(),
            )
            .into());
        }
        let tape = if let Some((prior, _)) = &head {
            if prior.tape.map(|tape| tape.uuid) != tape_uuid {
                return Err(StateError::JournalReplayFailed(
                    "read session tape differs from its authenticated creation source".into(),
                )
                .into());
            }
            // An identical historical reply remains read-only after retirement.
            // A new transition still passes the fold's live-identity checks.
            prior.tape
        } else {
            tape_uuid
                .map(|uuid| {
                    replay
                        .fold
                        .tapes
                        .get(&uuid)
                        .filter(|v| !v.retired)
                        .map(|v| v.tape)
                        .ok_or_else(|| {
                            StateError::JournalReplayFailed(
                                "session requires Reprovision identity".into(),
                            )
                        })
                })
                .transpose()?
        };
        let mut value = lib::SessionObserved {
            session_id,
            kind,
            endpoint,
            tape,
            event,
            previous: head.as_ref().map(|v| v.1),
            reason: reason.as_deref().map(Into::into),
        };
        if let Some((prior, reference)) = head {
            value.previous = prior.previous;
            if prior == value {
                self.catch_up_locked()?;
                return Ok(());
            }
            value.previous = Some(reference);
        }
        self.append_locked(|a, v| a.session_observed(value, actor, lib::SourceLayer::Layer5, v))?;
        Ok(())
    }

    /// Close operations known by typed source, retaining their real chain head.
    pub(crate) fn mark_operations_lost(&self) -> Result<u64, RuntimeError> {
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        let view = self
            .0
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut open = BTreeMap::new();
        let replay = self
            .0
            .authority
            .snapshot()?
            .replay(&view, &mut |applied| {
                if let lib::BodyView::OperationRecorded(value) = applied.record().body_view() {
                    if matches!(
                        value.event,
                        lib::OperationEvent::Accepted(_)
                            | lib::OperationEvent::Started(_)
                            | lib::OperationEvent::CancelRequested(_)
                            | lib::OperationEvent::CancellationRejected(_)
                    ) {
                        open.insert(
                            value.operation,
                            (
                                value.clone(),
                                applied.record().reference(applied.sequence()),
                            ),
                        );
                    } else {
                        open.remove(&value.operation);
                    }
                }
                Ok(())
            })
            .map_err(AuthorityError::Store)?;
        if replay.refusal.is_some() || !replay.fold.pending.is_empty() {
            return Err(StateError::JournalReplayFailed(
                "restart operation recovery requires resolved source".into(),
            )
            .into());
        }
        let count = open.len() as u64;
        for (_, (mut value, reference)) in open {
            value.previous = Some(reference);
            value.event = lib::OperationEvent::Failed(lib::Failed {
                retry: None,
                refusal_code: None,
                summary: "daemon_restart".into(),
            });
            self.append_locked(|a, v| {
                a.operation_recorded(value, lib::AuditActor::System, lib::SourceLayer::Layer4, v)
            })?;
        }
        Ok(count)
    }

    /// Open typed non-write sessions and their immutable chain references.
    pub(crate) fn open_sessions(
        &self,
    ) -> Result<Vec<(lib::SessionObserved, lib::DecisionRef)>, RuntimeError> {
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        let view = self
            .0
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut open = BTreeMap::new();
        let replay = self
            .0
            .authority
            .snapshot()?
            .replay(&view, &mut |applied| {
                if let lib::BodyView::SessionObserved(value) = applied.record().body_view() {
                    if value.event == lib::SessionEvent::Opened {
                        open.insert(
                            value.session_id,
                            (
                                value.clone(),
                                applied.record().reference(applied.sequence()),
                            ),
                        );
                    } else {
                        open.remove(&value.session_id);
                    }
                }
                Ok(())
            })
            .map_err(AuthorityError::Store)?;
        if replay.refusal.is_some() || !replay.fold.pending.is_empty() {
            return Err(StateError::JournalReplayFailed(
                "restart session recovery requires resolved source".into(),
            )
            .into());
        }
        Ok(open.into_values().collect())
    }

    /// Native open sessions remain physically unresolved; callers may perform
    /// their existing conservative calibration invalidation and quarantine probe.
    pub(crate) fn open_native_sessions(&self) -> Vec<crate::RestartSession> {
        self.0
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshots()
            .filter_map(|snapshot| {
                let session = snapshot.summary().open_session.as_ref()?;
                Some(crate::RestartSession {
                    session_id: uuid::Uuid::from_bytes(session.session_id),
                    session_kind: "write".into(),
                    tape_uuid: Some(snapshot.header().tape_uuid.to_vec()),
                    library_serial: None,
                    drive_bay: None,
                    drive_uuid: None,
                    drive_key: None,
                })
            })
            .collect()
    }
}

/// Presentation of existing typed endpoint/tape facts for restart callbacks.
pub(crate) fn session_projection(value: &lib::SessionObserved) -> crate::RestartSession {
    let key = value.endpoint.as_ref().map(|endpoint| match endpoint {
        lib::DriveKey::Bay(v) => crate::DriveKey::new(v.library_serial.as_str(), v.bay),
        lib::DriveKey::Standalone(v) => crate::DriveKey::Standalone {
            drive_serial: v.drive_serial.as_str().into(),
        },
    });
    crate::RestartSession {
        session_id: uuid::Uuid::from_bytes(value.session_id),
        session_kind: match value.kind {
            lib::SessionKind::Read => "read",
            lib::SessionKind::Operation => "operation",
        }
        .into(),
        tape_uuid: value.tape.map(|v| v.uuid.to_vec()),
        library_serial: key
            .as_ref()
            .and_then(|v| v.library_serial().map(str::to_owned)),
        drive_bay: key.as_ref().and_then(|v| v.bay().map(i64::from)),
        drive_uuid: None,
        drive_key: key,
    }
}

#[cfg(test)]
mod session_identity_tests {
    use super::*;

    /// Read-session identity is proved at creation, remains historical after a
    /// bay replacement, and is reconstructed without importing catalog rows.
    #[test]
    fn session_drive_identity_is_source_bound_and_frozen() {
        let root = tempfile::tempdir().unwrap();
        let paths = crate::StatePaths {
            config_path: root.path().join("config"),
            state_dir: root.path().join("state"),
            audit_dir: root.path().join("library"),
            journal_dir: root.path().join("native"),
            sqlite_path: root.path().join("catalog.sqlite"),
            tape_cache_dir: root.path().join("cache"),
            calibration_dir: root.path().join("calibration"),
        };
        let runtime = LibraryRuntime::open(
            &paths,
            true,
            Arc::new(crate::StateLockGuard::acquire(&paths.state_dir).unwrap()),
        )
        .unwrap();
        let observe = |serial: &str, bay: Option<u16>| {
            let result = runtime
                .observe_drives(vec![crate::DriveObservationInput {
                    serial: serial.into(),
                    identity_source: "DvcidInline".into(),
                    vendor: Some("MODEL".into()),
                    product: Some("DRIVE".into()),
                    firmware_rev: None,
                    managed: "rem".into(),
                    library_serial: bay.map(|_| "LIB".into()),
                    element_address: bay.map(i64::from),
                    observed_at_utc: None,
                }])
                .unwrap();
            <lib::Id>::try_from(result[0].drive_uuid.as_slice()).unwrap()
        };
        let original = observe("FIRST", Some(0x100));
        let bay = crate::DriveKey::new("LIB", 0x100);
        let session = [0x51; 16];
        let publish = |session, endpoint: &crate::DriveKey, event, id, serial: &str| {
            runtime.observe_session(
                session,
                lib::SessionKind::Read,
                Some(endpoint),
                None,
                event,
                None,
                lib::AuditActor::User("reader".into()),
                (Some(id), Some(serial.into())),
            )
        };
        let before = runtime.test_source_records().len();
        assert!(publish(
            session,
            &bay,
            lib::SessionEvent::Opened,
            original,
            "CONTRADICTION"
        )
        .is_err());
        assert!(publish(
            session,
            &bay,
            lib::SessionEvent::Opened,
            [0xff; 16],
            "FIRST"
        )
        .is_err());
        assert_eq!(runtime.test_source_records().len(), before);
        publish(session, &bay, lib::SessionEvent::Opened, original, "FIRST").unwrap();
        let replacement = observe("SECOND", Some(0x100));
        assert_ne!(original, replacement);
        let before = runtime.test_source_records().len();
        assert!(publish(
            session,
            &bay,
            lib::SessionEvent::Closed,
            replacement,
            "SECOND"
        )
        .is_err());
        assert_eq!(runtime.test_source_records().len(), before);
        publish(session, &bay, lib::SessionEvent::Closed, original, "FIRST").unwrap();
        let standalone = observe("STANDALONE", None);
        let standalone_key = crate::DriveKey::Standalone {
            drive_serial: "STANDALONE".into(),
        };
        publish(
            [0x52; 16],
            &standalone_key,
            lib::SessionEvent::Opened,
            standalone,
            "STANDALONE",
        )
        .unwrap();
        publish(
            [0x52; 16],
            &standalone_key,
            lib::SessionEvent::LostByRestart,
            standalone,
            "STANDALONE",
        )
        .unwrap();
        let assert_rows = || {
            let conn = rusqlite::Connection::open(&paths.sqlite_path).unwrap();
            for (id, drive, state) in [
                (session, original, "closed"),
                ([0x52; 16], standalone, "lost_restart"),
            ] {
                let row: (Vec<u8>, String) = conn
                    .query_row(
                        "select drive_uuid,state from sessions where session_id=?1",
                        [uuid::Uuid::from_bytes(id).to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .unwrap();
                assert_eq!(row, (drive.to_vec(), state.into()));
            }
        };
        assert_rows();
        let mut index = crate::projector::open(&paths.sqlite_path)
            .unwrap()
            .with_library_runtime(runtime.clone())
            .write_handle();
        runtime.rebuild_projection(&mut index).unwrap();
        assert_rows();
        drop(index);
        drop(runtime);
        let runtime = LibraryRuntime::open(
            &paths,
            true,
            Arc::new(crate::StateLockGuard::acquire(&paths.state_dir).unwrap()),
        )
        .unwrap();
        assert_rows();
        drop(runtime);
    }
}
