//! Bind actual discovery inputs to durable drive identities before dependent
//! health/session observations. Identity matching reads authenticated history.
//! Unsupported collision/reappearance alarms refuse the whole batch and fence
//! motion; no partial inventory or guessed alarm kind is published.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone)]
struct Seen {
    value: lib::DriveObserved,
    first: u64,
    last: u64,
    retired: bool,
}
impl LibraryRuntime {
    /// Replay the same identity and stewardship facts for observation and
    /// session attribution. The caller holds the runtime dispatch lock.
    fn replay_drive_history_locked(&self) -> Result<(BTreeMap<lib::Id, Seen>, u64), RuntimeError> {
        let view = self
            .0
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut known = BTreeMap::<lib::Id, Seen>::new();
        let mut generation = 0;
        let replay = self
            .0
            .authority
            .snapshot()?
            .replay(&view, &mut |applied| {
                let at = applied.record().envelope.timestamp_unix_ns;
                match applied.record().body_view() {
                    lib::BodyView::DriveObserved(value) => {
                        generation = generation.max(value.inventory_generation);
                        known
                            .entry(value.drive_uuid)
                            .and_modify(|v| {
                                v.value = value.clone();
                                v.last = at;
                            })
                            .or_insert(Seen {
                                value: value.clone(),
                                first: at,
                                last: at,
                                retired: false,
                            });
                    }
                    lib::BodyView::DriveDecision(value) => {
                        if let Some(seen) = known.get_mut(&value.drive_uuid) {
                            match value.decision {
                                lib::Stewardship::Retire(_) => seen.retired = true,
                                lib::Stewardship::Reinstate(_) => seen.retired = false,
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                Ok(())
            })
            .map_err(AuthorityError::Store)?;
        if replay.refusal.is_some() || !replay.fold.pending.is_empty() {
            return Err(StateError::JournalReplayFailed(
                "drive observation needs resolved authority".into(),
            )
            .into());
        }
        Ok((known, generation))
    }

    /// Resolve a trimmed physical serial to its unique active observed identity.
    /// This reads authenticated history under dispatch and appends nothing;
    /// absent and ambiguous active holders both return None.
    pub fn active_drive_uuid_for_serial(
        &self,
        serial: &str,
    ) -> Result<Option<lib::Id>, RuntimeError> {
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        let (known, _) = self.replay_drive_history_locked()?;
        let serial = serial.trim();
        let mut active = known.values().filter(|seen| {
            !seen.retired
                && seen
                    .value
                    .serial
                    .as_ref()
                    .is_some_and(|value| value.as_str() == serial)
        });
        let first = active.next().map(|seen| seen.value.drive_uuid);
        if active.next().is_some() {
            Ok(None)
        } else {
            Ok(first)
        }
    }

    /// Observe one complete owner-supplied batch. A missing drive is not retired.
    /// UUIDs are minted only for unmatched observations and become source facts.
    pub fn observe_drives(
        &self,
        inputs: Vec<crate::DriveObservationInput>,
    ) -> Result<Vec<crate::DriveObservationOutcome>, RuntimeError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        let (known, generation) = self.replay_drive_history_locked()?;
        let refusal = |message: &str| {
            self.0.motion_fenced.store(true, Ordering::Release);
            RuntimeError::State(StateError::JournalReplayFailed(format!("{message}; drive discovery cannot preserve the required identity alarm; motion is fenced")))
        };
        // The known drive that holds a serial: the earliest-seen active one, else the earliest-seen.
        let holder_of = |serial: &str| {
            known
                .values()
                .filter(|v| {
                    v.value
                        .serial
                        .as_ref()
                        .is_some_and(|v| v.as_str() == serial)
                })
                .min_by_key(|v| (v.retired, v.first, v.value.drive_uuid))
                .map(|v| v.value.drive_uuid)
        };
        // A serial or identity collision is counted in the library log as one `RefusalRecorded`,
        // after the latch and the refusal's own text: it names the known drive involved, or the
        // library when no known drive matches (a duplicate of a serial never seen before).
        let conflict = |message: &str, drive: Option<lib::Id>| {
            let error = refusal(message);
            self.record_refusal_locked(
                lib::RefusalCode::DriveIdentityConflictLatch,
                drive.map_or(RefusalSubject::Library, RefusalSubject::Drive),
                lib::AuditActor::System,
            );
            error
        };
        let mut serials = BTreeSet::new();
        for input in &inputs {
            // A blank serial is a malformed observation, not a collision: refused, not counted.
            if input.serial.trim().is_empty() {
                return Err(refusal("blank drive serial"));
            }
            if !serials.insert(input.serial.trim()) {
                return Err(conflict(
                    "duplicate serial in discovery snapshot",
                    holder_of(input.serial.trim()),
                ));
            }
        }
        let generation = generation.checked_add(1).ok_or_else(|| {
            StateError::JournalReplayFailed("drive inventory generation overflow".into())
        })?;
        let mut prepared = Vec::new();
        for input in inputs {
            let serial = input.serial.trim();
            let active = known
                .values()
                .filter(|v| {
                    !v.retired
                        && v.value
                            .serial
                            .as_ref()
                            .is_some_and(|v| v.as_str() == serial)
                })
                .count();
            let holder = holder_of(serial);
            if active > 1 {
                return Err(conflict("historical active serial collision", holder));
            }
            let norm = |v: Option<String>| v.map(|v| lib::Text::from(v.trim()));
            let vendor = norm(input.vendor);
            let product = norm(input.product);
            let as_str = |v: &Option<lib::Text>| v.as_ref().map_or("", |v| v.as_str()).to_owned();
            let prior = known
                .values()
                .filter(|v| {
                    v.value
                        .serial
                        .as_ref()
                        .is_some_and(|v| v.as_str() == serial)
                        && as_str(&v.value.vendor) == as_str(&vendor)
                        && as_str(&v.value.product) == as_str(&product)
                })
                .min_by_key(|v| (v.retired, v.first, v.value.drive_uuid));
            if let Some(retired) = prior.filter(|v| v.retired) {
                return Err(conflict(
                    "retired drive reappeared",
                    Some(retired.value.drive_uuid),
                ));
            }
            if prior.is_none() && active > 0 {
                return Err(conflict(
                    "new identity would collide with an active serial",
                    holder,
                ));
            }
            let uuid =
                prior.map_or_else(|| *uuid::Uuid::new_v4().as_bytes(), |v| v.value.drive_uuid);
            let identity_source = match input.identity_source.trim() {
                "Standalone" => lib::IdentitySource::Standalone,
                "DvcidInline" => lib::IdentitySource::DvcidInline,
                "DvcidAndInquiry" => lib::IdentitySource::DvcidAndInquiry,
                "Derived" => lib::IdentitySource::Derived,
                _ => {
                    return Err(StateError::ConfigInvalid(
                        "unknown discovered identity source".into(),
                    )
                    .into())
                }
            };
            let managed = match input.managed.trim() {
                "rem" => lib::Managed::Rem,
                "foreign" => lib::Managed::Foreign,
                _ => {
                    return Err(StateError::ConfigInvalid(
                        "unknown discovered drive stewardship".into(),
                    )
                    .into())
                }
            };
            let library = input
                .library_serial
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty());
            let endpoint = match (library, input.element_address) {
                (Some(library), Some(bay)) => Some(lib::DriveKey::Bay(lib::Bay {
                    library_serial: library.into(),
                    bay: bay
                        .try_into()
                        .map_err(|_| StateError::ConfigInvalid("drive bay exceeds u16".into()))?,
                })),
                (None, None) => None,
                _ => {
                    return Err(StateError::ConfigInvalid(
                        "partial drive endpoint cannot be represented".into(),
                    )
                    .into())
                }
            };
            if input.observed_at_utc.is_some() {
                return Err(StateError::ConfigInvalid("typed discovery observation time is its writer envelope; caller timestamp is not supported".into()).into());
            }
            prepared.push((
                lib::DriveObserved {
                    drive_uuid: uuid,
                    serial: Some(serial.into()),
                    identity_source,
                    endpoint,
                    managed,
                    inventory_generation: generation,
                    vendor,
                    product,
                    firmware_rev: norm(input.firmware_rev),
                },
                prior.is_none(),
            ));
        }
        let mut out = Vec::new();
        for (value, newly_seen) in prepared {
            let drive_uuid = value.drive_uuid.to_vec();
            self.append_locked(|a, v| {
                a.drive_observed(value, lib::AuditActor::System, lib::SourceLayer::Layer4, v)
            })?;
            out.push(crate::DriveObservationOutcome {
                drive_uuid,
                newly_seen,
                serial_collision: false,
            });
        }
        Ok(out)
    }
}
