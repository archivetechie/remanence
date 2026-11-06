//! Actor-owned observations share typed source ordering. Session/health keys and
//! alarm generations come from authenticated records under the dispatch lease.
use super::*;

impl LibraryRuntime {
    /// Preserve the existing first observation for a session/trigger lifetime
    /// key; retries do not replace it with counters sampled at a later instant.
    pub fn observe_drive_health(
        &self,
        value: lib::DriveHealthObserved,
        actor: lib::AuditActor,
    ) -> Result<(), RuntimeError> {
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        let view = self
            .0
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut existing = None;
        let replay = self
            .0
            .authority
            .snapshot()?
            .replay(&view, &mut |applied| {
                if let lib::BodyView::DriveHealthObserved(prior) = applied.record().body_view() {
                    if value.session_id.is_some()
                        && prior.session_id == value.session_id
                        && prior.trigger == value.trigger
                        && existing.is_none()
                    {
                        existing = Some(prior.tape_alert_flags);
                    }
                }
                Ok(())
            })
            .map_err(AuthorityError::Store)?;
        if replay.refusal.is_some() || !replay.fold.pending.is_empty() {
            return Err(StateError::JournalReplayFailed(
                "health observation needs resolved authority".into(),
            )
            .into());
        }
        if existing.is_some() {
            let rank = |mask: u64| {
                if mask & (1 << 19) != 0 {
                    2
                } else if mask & (1 << 20) != 0 {
                    1
                } else {
                    0
                }
            };
            if rank(value.tape_alert_flags) > 0 {
                self.0.motion_fenced.store(true, Ordering::Release);
                return Err(StateError::JournalReplayFailed("repeated health sample observes a cleaning obligation that cannot be durably recorded for this session; motion is fenced".into()).into());
            }
            self.catch_up_locked()?;
            return Ok(());
        }
        self.append_locked(|a, v| {
            a.drive_health_observed(value, actor, lib::SourceLayer::Layer4, v)
        })?;
        Ok(())
    }

    /// Raise/refresh a condition, or clear it when the producer observes its
    /// removal. An acknowledged active condition retains acknowledgement.
    /// Returns false only when a clear has no active source condition.
    pub fn observe_alarm(
        &self,
        condition: &str,
        raise: Option<lib::AlarmRaise>,
        clear_reason: &str,
    ) -> Result<bool, RuntimeError> {
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
                if let lib::BodyView::AlarmChanged(value) = applied.record().body_view() {
                    if value.condition_key.as_str() == condition {
                        head = Some(value.clone());
                    }
                }
                Ok(())
            })
            .map_err(AuthorityError::Store)?;
        if replay.refusal.is_some() || !replay.fold.pending.is_empty() {
            return Err(StateError::JournalReplayFailed(
                "alarm transition needs resolved authority".into(),
            )
            .into());
        }
        let active = head
            .as_ref()
            .is_some_and(|v| !matches!(v.action, lib::AlarmAction::Clear(_)));
        let action = match raise {
            Some(value) if active => lib::AlarmAction::Refresh(lib::AlarmRefresh {
                severity: value.severity,
                reason: value.reason,
                cause: None,
            }),
            Some(value) => lib::AlarmAction::Raise(value),
            None if active => lib::AlarmAction::Clear(lib::Reason {
                reason: clear_reason.into(),
            }),
            None => {
                self.catch_up_locked()?;
                return Ok(false);
            }
        };
        let expected = head.map_or(0, |v| v.change.next);
        self.append_locked(|a, v| {
            a.alarm_changed(
                lib::AlarmChanged {
                    condition_key: condition.into(),
                    change: lib::GenerationChange {
                        expected,
                        next: expected,
                    },
                    action,
                    cause: None,
                },
                expected,
                lib::AuditActor::System,
                lib::SourceLayer::Layer4,
                v,
            )
        })?;
        Ok(true)
    }
}
