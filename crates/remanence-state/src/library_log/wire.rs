//! Canonical library CBOR scalars and schema machinery; no untyped value tree.
use super::*;
use crate::log_frame::cbor::{expect, head, invalid, Wire};
use std::io::{self, Read, Write};

/// UTF-8 scalar bounded before allocation and on encode.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TextValue<const N: usize>(pub String);
impl<const N: usize> From<&str> for TextValue<N> {
    fn from(s: &str) -> Self {
        Self(s.into())
    }
}
impl<const N: usize> TextValue<N> {
    /// Borrow the text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Whether the scalar is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
/// Opaque bytes with a schema bound, including malformed sense evidence.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Blob<const N: usize>(pub Vec<u8>);
fn scalar(r: &mut dyn Read, major: u8, max: usize) -> io::Result<Vec<u8>> {
    let n = expect(r, major)?;
    if n > max as u64 {
        return Err(invalid());
    }
    let mut v = Vec::new();
    if r.take(n).read_to_end(&mut v)? as u64 != n {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(v)
}
impl<const N: usize> Wire for TextValue<N> {
    fn put(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.0.len() > N {
            return Err(invalid());
        }
        head(w, 3, self.0.len() as u64)?;
        w.write_all(self.0.as_bytes())
    }
    fn get(r: &mut dyn Read) -> io::Result<Self> {
        Ok(Self(
            String::from_utf8(scalar(r, 3, N)?).map_err(|_| invalid())?,
        ))
    }
}
impl<const N: usize> Wire for Blob<N> {
    fn put(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.0.len() > N {
            return Err(invalid());
        }
        head(w, 2, self.0.len() as u64)?;
        w.write_all(&self.0)
    }
    fn get(r: &mut dyn Read) -> io::Result<Self> {
        Ok(Self(scalar(r, 2, N)?))
    }
}
/// CBOR null payload of a unit union variant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Unit;
impl Wire for Unit {
    fn put(&self, w: &mut dyn Write) -> io::Result<()> {
        w.write_all(&[0xf6])
    }
    fn get(r: &mut dyn Read) -> io::Result<Self> {
        if crate::log_frame::cbor::read_head(r)? == (7, 22) {
            Ok(Self)
        } else {
            Err(invalid())
        }
    }
}
/// Strictly negative signed diagnostic integer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Negative(pub i64);
impl Wire for Negative {
    fn put(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.0 >= 0 {
            return Err(invalid());
        }
        head(w, 1, (-1i128 - self.0 as i128) as u64)
    }
    fn get(r: &mut dyn Read) -> io::Result<Self> {
        let n = expect(r, 1)?;
        if n > i64::MAX as u64 {
            return Err(invalid());
        }
        Ok(Self(-1 - (n as i64)))
    }
}
macro_rules! fields {
    ($ty:ident; $($field:ident:$ft:ty),+ $(,)?) => {
        crate::log_frame::cbor::fields!($ty; $($field),+);
        impl $ty {
            #[cfg(test)]
            pub(super) const FIELDS: &'static [(&'static str,&'static str)] = &[$((stringify!($field),stringify!($ft))),+];
        }
    };
}
macro_rules! enumeration {
    ($ty:ident; $($n:literal=>$variant:ident),+ $(,)?) => {
        crate::log_frame::cbor::enumeration!($ty;$($n=>$variant),+);
    };
}
macro_rules! union {
    ($ty:ident; $($n:literal=>$variant:ident($payload:ty)),+ $(,)?) => {
        impl Wire for $ty {
            fn put(&self,w:&mut dyn Write)->io::Result<()> {head(w,4,2)?; match self {$(Self::$variant(v)=>{($n as u64).put(w)?;v.put(w)}),+}}
            fn get(r:&mut dyn Read)->io::Result<Self>{if expect(r,4)?!=2{return Err(invalid());}match u64::get(r)? {$($n=>Ok(Self::$variant(<$payload>::get(r)?)),)+_=>Err(invalid())}}
        }
    };
}
impl Wire for AuditActor {
    fn put(&self, w: &mut dyn Write) -> io::Result<()> {
        let (kind, name) = match self {
            Self::System => (0u8, None),
            Self::User(v) => (1, Some(Text::from(v.as_str()))),
            Self::Service(v) => (2, Some(Text::from(v.as_str()))),
        };
        head(w, 5, 2)?;
        0u8.put(w)?;
        kind.put(w)?;
        1u8.put(w)?;
        name.put(w)
    }
    fn get(r: &mut dyn Read) -> io::Result<Self> {
        if expect(r, 5)? != 2 || u8::get(r)? != 0 {
            return Err(invalid());
        }
        let kind = u8::get(r)?;
        if u8::get(r)? != 1 {
            return Err(invalid());
        }
        match (kind, Option::<Text>::get(r)?) {
            (0, None) => Ok(Self::System),
            (1, Some(v)) => Ok(Self::User(v.0)),
            (2, Some(v)) => Ok(Self::Service(v.0)),
            _ => Err(invalid()),
        }
    }
}
enumeration!(SourceLayer;0=>Layer2,1=>Layer3b,2=>Layer3c,3=>Layer4,4=>Layer5);
enumeration!(LtoGen;0=>Lto1,1=>Lto2,2=>Lto3,3=>Lto4,4=>Lto5,5=>Lto6,6=>Lto7,7=>M8,8=>Lto8,9=>Lto9);
enumeration!(GenerationSource;0=>Measured,1=>Label,2=>Operator);
impl Wire for DiagnosticEntry {
    fn put(&self, w: &mut dyn Write) -> io::Result<()> {
        head(w, 4, 2)?;
        self.0.put(w)?;
        self.1.put(w)
    }
    fn get(r: &mut dyn Read) -> io::Result<Self> {
        if expect(r, 4)? != 2 {
            return Err(invalid());
        }
        Ok(Self(Text::get(r)?, DiagnosticValue::get(r)?))
    }
}
fields!(Envelope; record_uuid:Id,timestamp_unix_ns:u64,host_id:Text,process_id:u32,software_build:Text,actor:AuditActor,source_layer:SourceLayer,operation_id:Option<Id>,session_id:Option<Id>,idempotency_key:Option<Id>);
fields!(TapeRef; uuid:Id,incarnation:u64);
fields!(TapeAt; tape:TapeRef,sequence:u64,record_digest:Hash);
fields!(DecisionRef; sequence:u64,record_uuid:Id);
fields!(GenerationChange; expected:u64,next:u64);
fields!(CompletionEvidence; dispatch:Dispatch,completion:Completion,scsi_status:Option<u8>,host_status:Option<u16>,driver_status:Option<u16>,sense:Option<Blob<252>>,residual:Option<u64>);
fields!(Bay; library_serial:Text,bay:u16);
fields!(Standalone; drive_serial:Text);
fields!(PriorHolder; tape:TapeRef,change:GenerationChange);
fields!(ParityScheme;id:Text,data_blocks_per_stripe:u16,parity_blocks_per_stripe:u16,stripes_per_neighborhood:u32);
fields!(MediumSpec; family:MediumFamily,usable_logical_bytes:u64,record_size:u32,rerecord:bool,lifecycle_ops:bool,source:GenerationSource);
fields!(Adoption; request_fingerprint:Hash,operation:Id,idempotency_key:Id,physical_tail:PhysicalTail,state:AdoptedState,library_revision:Option<Text>,home_slot:Option<u16>,drive_serial:Text,identity_only:bool,bootstrap_drive_compression:bool,configured_drive_compression:bool);
fields!(Load; endpoint:DriveKey,attempt:u32);
fields!(Unload; endpoint:DriveKey);
fields!(Move; library_serial:Text,source:u16,destination:u16);
fields!(Element; address:u16,kind:ElementKind,barcode:Option<LabelText>,full:bool);
fields!(VerificationCursor; partition:u16,file:u64,lba:u64,ordinal:u64,part:u32);
fields!(DamagedRange; partition:u16,start_lba:u64,end_lba:u64);
fields!(Unprotected; w:u64,t:u64);
fields!(PoolDefinition; id:Text,display_name:Option<Text>,copy_class:Option<Text>,content_class:Option<Text>,scheme:Option<Text>,selection_policy:SelectionPolicy,watermark_low_bits:u64,watermark_high_bits:u64,capacity_cap_bytes:Option<u64>,block_size_bytes:u64,min_object_size_bytes:u64);
fields!(PoolRule; prefix:Text,pool_id:Text);
fields!(Definitions; pools:Bounded<PoolDefinition, 4096>,rules:Bounded<PoolRule, 4096>,cleaning_prefixes:Bounded<Text, 256>);
fields!(Reason; reason:Text);
fields!(Annotation; purchase_date:Option<Text>,warranty_until:Option<Text>,cost:Option<Text>,notes:Option<Text>);
fields!(Counters; write_corrected:Option<u64>,write_uncorrected:Option<u64>,read_corrected:Option<u64>,read_uncorrected:Option<u64>);
fields!(Register; barcode:LabelText,home_slot:u16);
fields!(Begin; trigger:CleanTrigger);
fields!(Advance; phase:CleanPhase,home_slot:Option<u16>);
fields!(Finish; result:CleanResult,verified_credit:bool);
fields!(AlarmRefresh; severity:Severity,reason:Text,cause:Option<DecisionRef>);
fields!(AlarmRaise; kind:AlarmKind,severity:Severity,reason:Text);
fields!(Accepted; actor_fingerprint:Text,idempotency_key:Option<Id>,request_fingerprint:Hash,subject:Option<Text>);
fields!(Response; response_fingerprint:Option<Hash>);
fields!(Failed; retry:Option<Retry>,refusal_code:Option<u32>,summary:Text);
fields!(Cache; descriptors:Blob<4194304>,mapped_extent_lba:u64,write_epoch:u64,digest:Hash);
fields!(RepairApplied; archived_prefix_digest:Hash,retained_through:u64);
fields!(Recovered; source_digest:Hash);
fields!(AssignPool; tape:TapeRef,pool:Option<Text>,change:GenerationChange,source:AssignmentSource,configuration:Option<DecisionRef>);
fields!(Label; tape:TapeRef,label:Option<LabelText>,provenance:LabelProvenance,change:GenerationChange,prior_holder:Option<PriorHolder>,observed_bot_uuid:Option<Id>);
fields!(Retire; tape:TapeRef,reason:RetireReason,observed_tape_sequence:Option<u64>,assignment_generation:u64,detail:Option<Text>);
fields!(Reprovision; tape:TapeRef,predecessor:Option<TapeRef>,mode:ProvisionMode,label:Option<LabelText>,label_provenance:Option<LabelProvenance>,parity_scheme:Option<ParityScheme>,medium:MediumSpec,pool:Option<Text>,endpoint:Option<DriveKey>,adoption:Option<Adoption>);
fields!(MediumSpecSet; tape:TapeRef,medium:MediumSpec,change:GenerationChange);
fields!(SupersedeTail; at:TapeAt,operation:Option<Id>,session_id:Option<Id>,preview_digest:Option<Hash>,tail_file_count:u64);
fields!(AcceptUnprotectedFinalization; at:TapeAt,operation:Id,w:u64,t:u64,reason:Text);
fields!(FenceRaised; kind:FenceKind,scope:FenceScope,cause:FenceCause,evidence:Option<CompletionEvidence>,at:Option<TapeAt>,operation:Option<Id>);
fields!(FenceReleased; fence:u64,evidence:Text,forced:bool);
fields!(MotionIntent; command:Id,parent_operation:Option<Id>,motion:Motion,tape:Option<TapeRef>,inventory_generation:Option<u64>,readiness_generation:Option<u64>);
fields!(MotionOutcome; intent:DecisionRef,result:MotionResult,evidence:CompletionEvidence,confirmation:Option<Confirmation>);
fields!(InventoryReconciled; subject:InventorySubject,observation_generation:u64,resolves:Bounded<DecisionRef, 1024>,complete:bool,elements:Bounded<Element, 65535>);
fields!(VerificationProgress; operation:Id,at:TapeAt,plan_digest:Hash,modes:u32,parts_done:u32,next:VerificationCursor,completed_digest:Hash);
fields!(VerificationVerdict; operation:Id,at:TapeAt,damaged:Bounded<DamagedRange, 4096>,unprotected:Option<Unprotected>,modes:u32,edition_digest:Option<Hash>,progress:Option<DecisionRef>);
fields!(ManualFinalizationAccepted; at:TapeAt,operation:Id,actor_fingerprint:Text,idempotency_key:Id,request_fingerprint:Hash,assignment_generation:u64);
fields!(ConfigurationAccepted; generation:u64,config_digest:Hash,definitions:Definitions);
fields!(ReadinessObserved; operation:Id,endpoint:DriveKey,tape:Option<TapeRef>,barcode:Option<LabelText>,phase:ReadinessPhase,state:ReadinessState,previous:Option<DecisionRef>,started_unix_ns:u64,limit_ms:u64,evidence:Option<CompletionEvidence>,load_intent:Option<DecisionRef>,ready_generation:u64,result:ReadinessResult,scope:ReadinessScope,media:Option<MediumFamily>);
fields!(DriveObserved; drive_uuid:Id,serial:Option<Text>,identity_source:IdentitySource,endpoint:Option<DriveKey>,managed:Managed,inventory_generation:u64,vendor:Option<Text>,product:Option<Text>,firmware_rev:Option<Text>);
fields!(DriveDecision; drive_uuid:Id,decision:Stewardship,change:GenerationChange);
fields!(DriveHealthObserved; drive_uuid:Id,tape:Option<TapeRef>,session_id:Option<Id>,operation:Option<Id>,trigger:HealthTrigger,tape_alert_flags:u64,counters:Counters,raw_pages:Option<Blob<65536>>);
fields!(CleaningRecorded; run_id:Text,previous:Option<DecisionRef>,drive_uuid:Option<Id>,cartridge:Option<TapeRef>,action:CleaningAction);
fields!(AlarmChanged; condition_key:Text,change:GenerationChange,action:AlarmAction,cause:Option<DecisionRef>);
fields!(OperationRecorded; operation:Id,kind:OperationKind,previous:Option<DecisionRef>,event:OperationEvent);
fields!(SessionObserved; session_id:Id,kind:SessionKind,endpoint:Option<DriveKey>,tape:Option<TapeRef>,event:SessionEvent,previous:Option<DecisionRef>,reason:Option<Text>);
fields!(WrapMapCached; tape:TapeRef,calibration_generation:u64,action:WrapAction);
fields!(RecoveryRecorded; at:TapeAt,action:RecoveryAction,operation:Id,prior:Option<DecisionRef>);
fields!(Diagnostic; event:DiagnosticEvent,subject_kind:Text,subject_id:Option<Text>,detail:Bounded<DiagnosticEntry, 256>);
enumeration!(AssignmentSource; 0=>Operator,1=>Rule);
enumeration!(LabelProvenance; 0=>Changer,1=>Operator);
enumeration!(RetireReason; 0=>OperatorRequested);
enumeration!(ProvisionMode; 0=>Initialize,1=>Adopt);
enumeration!(Dispatch; 0=>NotIssued,1=>Issued);
enumeration!(Completion; 0=>Known,1=>Unknown);
enumeration!(FenceKind; 0=>Write,1=>DriveFault);
enumeration!(FenceCause; 0=>PartialBatch,1=>TransferError,2=>CloseoutCheckMismatch,3=>SupersedeTailFailed,4=>StandaloneWriteLostByRestart,5=>AppendAdmissionRefused,6=>ResetUnitAttention,7=>PositionDrift,8=>AuthorityAmbiguous,9=>SessionSuffixContradiction,10=>CloseoutReadContradiction,11=>Cleaning,12=>ContainerQuarantine,13=>CheckpointBarrierFailed);
enumeration!(MotionResult; 0=>Done,1=>Failed,2=>Unknown);
enumeration!(ElementKind; 0=>Transport,1=>Storage,2=>ImportExport,3=>Drive);
enumeration!(SelectionPolicy; 0=>CompleteOrFill,1=>FillOldest);
enumeration!(PhysicalTail; 0=>ExactBootstrapFilemarkEod,1=>DataAfterBootstrap,2=>MissingFilemark,3=>ExtraFilemark,4=>Ambiguous);
enumeration!(AdoptedState; 0=>Ready,1=>RecoveryRequired);
enumeration!(ReadinessPhase;0=>Planned,1=>InitializingCommandLoad,2=>ReadinessPoll,3=>Ready,4=>Cancelled,5=>Shutdown,6=>SessionOpenShortProbe,7=>SessionOpenImmediateLoad,8=>SessionOpenAfterImmediateLoad,9=>StartupReconcileTur,10=>StartupReconcile,11=>StartupReconcileRequiresRelease,12=>QuarantineRelease,13=>Released,14=>WaitReadyTur,15=>AfterWaitReadyTur,16=>BeforeConditionalImmediateLoad,17=>ConditionalImmediateLoad,18=>AfterConditionalImmediateLoad,19=>AfterBotClassification,20=>AfterClobberConfirmation,21=>AfterMoveMedium,22=>AfterOpenDrive,23=>AfterOpenLibrary,24=>AfterReadMediumPartitions,25=>AfterRewindBeforeBotRead,26=>ApplyInitWriteGate,27=>BeforeApplyInitWriteGate,28=>BeforeMoveMedium,29=>BeforeOpenDrive,30=>BeforeOpenLibrary,31=>BeforeReadDriveConfig,32=>BeforeRewindBeforeBootstrapWrite,33=>BeforeRewindBeforeBotRead,34=>BeforeWaitReadyTur,35=>MoveMedium,36=>OpenDrive,37=>OpenLibrary,38=>ReadDriveConfig,39=>ReadMediumPartitions,40=>RewindBeforeBootstrapWrite,41=>RewindBeforeBotRead,42=>ClobberConfirmation,43=>PreLoadTur,44=>AlreadyLoadedTur,45=>BeforePreLoadTur,46=>AfterPreLoadTur,47=>BeforeAlreadyLoadedTur,48=>AfterAlreadyLoadedTur);
enumeration!(ReadinessState; 0=>Planned,1=>PreReadyLoading,2=>Ready,3=>MediaInitializing,4=>BecomingReady,5=>UnitAttention,6=>TargetBusy,7=>ReservationConflict,8=>TransportUnknown,9=>TerminalError,10=>TimeoutUnknown,11=>AbortedUnknown,12=>Cancelled,13=>Released,14=>Starting,15=>RecordingFailed);
enumeration!(ReadinessScope; 0=>Endpoint,1=>Library);
enumeration!(ReadinessResult; 0=>Pending,1=>Ready,2=>Failed,3=>Cancelled,4=>AbortedUnknown,5=>Released);
enumeration!(IdentitySource; 0=>Standalone,1=>DvcidInline,2=>DvcidAndInquiry,3=>Derived);
enumeration!(Managed; 0=>Rem,1=>Foreign);
enumeration!(HealthTrigger; 0=>SessionClose,1=>AppendFailure,2=>ReadFailure,3=>ForeignCounter,4=>Manual);
enumeration!(CleanTrigger; 0=>Manual,1=>Periodic,2=>Now);
enumeration!(CleanPhase; 0=>Fencing,1=>Selecting,2=>MovingIn,3=>Cleaning,4=>MovingBack,5=>Verifying,6=>Done,7=>Failed,8=>NeedsOperator);
enumeration!(CleanResult; 0=>Done,1=>Failed,2=>NeedsOperator);
enumeration!(AlarmKind; 0=>DriveCleaningAbnormalFrequency,1=>CleaningNeedsOperator,2=>NoClnCart,3=>ClnCartExpired,4=>CartNotCleaningBehavior,5=>SnapshotPersistFailing,6=>CheckpointBarrierOverdue,7=>ForeignDriveWantsCleaning,8=>KindFlipRefused,9=>ContradictedTape,10=>PendingRecordRejected);
enumeration!(Severity; 0=>Warning,1=>Critical,2=>Error);
enumeration!(OperationKind; 0=>Unknown,1=>AckAlarm,2=>DriveStewardship,3=>CleanDrive,4=>RefreshInventory,5=>MoveMedium,6=>LoadDrive,7=>UnloadDrive,8=>MediaReadinessWait,9=>TapeVerification,10=>FinalizeTape,11=>ReconcileTape,12=>TapeCloseout,13=>CloseoutOverdue);
enumeration!(Retry; 0=>Transient,1=>Operator,2=>Media);
enumeration!(SessionKind; 1=>Read,2=>Operation);
enumeration!(SessionEvent; 0=>Opened,1=>Closed,2=>Orphaned,3=>LostByRestart);
enumeration!(DiagnosticEvent; 0=>RequestReceived,1=>OperationStarted,2=>OperationProgress,3=>OperationFinished,4=>OperationFailed,5=>CancelRequested,6=>CancelledBeforeDispatch,7=>CompletedAfterCancel,8=>CancellationRejected,9=>CompletionUnknown,10=>ClockRegressionObserved,11=>ClockForwardJumpObserved,12=>HardwareWarning,13=>RecoveryEvent,14=>ConfigLoaded,15=>ConfigRejected,16=>IndexRebuilt,17=>ReadOnlyModeEntered,18=>ReadOnlyModeLeft,19=>AuditWriteFailed);
union!(DriveKey; 0=>Bay(Bay),1=>Standalone(Standalone));
// Named payload keeps the Model wire map schema-directed.
struct ModelScope {
    model: Text,
    firmware: Text,
}
fields!(ModelScope; model:Text,firmware:Text);
impl Wire for MediumFamily {
    fn put(&self, w: &mut dyn Write) -> io::Result<()> {
        head(w, 4, 2)?;
        match self {
            Self::Lto(generation) => {
                0u64.put(w)?;
                generation.put(w)
            }
            Self::Disk => {
                1u64.put(w)?;
                Unit.put(w)
            }
        }
    }
    fn get(r: &mut dyn Read) -> io::Result<Self> {
        if expect(r, 4)? != 2 {
            return Err(invalid());
        }
        match u64::get(r)? {
            0 => Ok(Self::Lto(LtoGen::get(r)?)),
            1 => {
                Unit::get(r)?;
                Ok(Self::Disk)
            }
            _ => Err(invalid()),
        }
    }
}
impl Wire for FenceScope {
    fn put(&self, w: &mut dyn Write) -> io::Result<()> {
        head(w, 4, 2)?;
        match self {
            Self::Tape(t) => {
                0u64.put(w)?;
                t.put(w)
            }
            Self::Drive(d) => {
                1u64.put(w)?;
                d.put(w)
            }
            Self::Model { model, firmware } => {
                2u64.put(w)?;
                ModelScope {
                    model: model.clone(),
                    firmware: firmware.clone(),
                }
                .put(w)
            }
        }
    }
    fn get(r: &mut dyn Read) -> io::Result<Self> {
        if expect(r, 4)? != 2 {
            return Err(invalid());
        }
        match u64::get(r)? {
            0 => Ok(Self::Tape(TapeRef::get(r)?)),
            1 => Ok(Self::Drive(DriveKey::get(r)?)),
            2 => {
                let v = ModelScope::get(r)?;
                Ok(Self::Model {
                    model: v.model,
                    firmware: v.firmware,
                })
            }
            _ => Err(invalid()),
        }
    }
}
union!(Motion; 0=>Load(Load),1=>Unload(Unload),2=>Move(Move));
union!(InventorySubject; 0=>Library(Text),1=>Standalone(Standalone));
union!(Confirmation; 0=>Inventory(u64),1=>Readiness(u64));
union!(Stewardship; 0=>Retire(Reason),1=>Reinstate(Reason),2=>Annotate(Annotation));
union!(CleaningAction; 0=>Register(Register),1=>Begin(Begin),2=>Advance(Advance),3=>Finish(Finish),4=>Expire(Reason),5=>Reject(Reason));
union!(AlarmAction; 0=>Raise(AlarmRaise),1=>Acknowledge(Unit),2=>Clear(Reason),3=>Refresh(AlarmRefresh));
union!(OperationEvent; 0=>Accepted(Accepted),1=>Started(Unit),2=>CancelRequested(Unit),3=>CancellationRejected(Unit),4=>CancelledBeforeDispatch(Unit),5=>CompletedAfterCancel(Response),6=>Finished(Response),7=>Failed(Failed),8=>CompletionUnknown(Unit));
union!(WrapAction; 0=>Cache(Cache),1=>Invalidate(Reason));
union!(RecoveryAction; 1=>RepairApplied(RepairApplied),2=>AppendAuthorityRecovered(Recovered));
union!(DiagnosticValue; 0=>Text(Text),1=>Unsigned(u64),2=>Negative(Negative),3=>Bytes(Blob<4194304>),4=>Bool(bool),5=>Null(Unit));
pub(super) fn encode(record: &Record, w: &mut dyn Write) -> io::Result<()> {
    head(w, 5, 2)?;
    0u8.put(w)?;
    record.envelope.put(w)?;
    1u8.put(w)?;
    encode_body(&record.body, w)
}
pub(super) fn encode_body(body: &Body, w: &mut dyn Write) -> io::Result<()> {
    match body {
        Body::AssignPool(v) => v.put(w),
        Body::Label(v) => v.put(w),
        Body::Retire(v) => v.put(w),
        Body::Reprovision(v) => v.put(w),
        Body::MediumSpecSet(v) => v.put(w),
        Body::SupersedeTail(v) => v.put(w),
        Body::AcceptUnprotectedFinalization(v) => v.put(w),
        Body::FenceRaised(v) => v.put(w),
        Body::FenceReleased(v) => v.put(w),
        Body::MotionIntent(v) => v.put(w),
        Body::MotionOutcome(v) => v.put(w),
        Body::InventoryReconciled(v) => v.put(w),
        Body::VerificationProgress(v) => v.put(w),
        Body::VerificationVerdict(v) => v.put(w),
        Body::ManualFinalizationAccepted(v) => v.put(w),
        Body::ConfigurationAccepted(v) => v.put(w),
        Body::ReadinessObserved(v) => v.put(w),
        Body::DriveObserved(v) => v.put(w),
        Body::DriveDecision(v) => v.put(w),
        Body::DriveHealthObserved(v) => v.put(w),
        Body::CleaningRecorded(v) => v.put(w),
        Body::AlarmChanged(v) => v.put(w),
        Body::OperationRecorded(v) => v.put(w),
        Body::SessionObserved(v) => v.put(w),
        Body::WrapMapCached(v) => v.put(w),
        Body::RecoveryRecorded(v) => v.put(w),
        Body::Diagnostic(v) => v.put(w),
    }
}
pub(super) fn decode(tag: u16, bytes: &[u8]) -> io::Result<Record> {
    let mut r = bytes;
    if expect(&mut r, 5)? != 2 || u8::get(&mut r)? != 0 {
        return Err(invalid());
    }
    let envelope = Envelope::get(&mut r)?;
    if u8::get(&mut r)? != 1 {
        return Err(invalid());
    }
    let body = match tag {
        1 => Body::AssignPool(AssignPool::get(&mut r)?),
        2 => Body::Label(Label::get(&mut r)?),
        3 => Body::Retire(Retire::get(&mut r)?),
        4 => Body::Reprovision(Reprovision::get(&mut r)?),
        5 => Body::MediumSpecSet(MediumSpecSet::get(&mut r)?),
        6 => Body::SupersedeTail(SupersedeTail::get(&mut r)?),
        7 => Body::AcceptUnprotectedFinalization(AcceptUnprotectedFinalization::get(&mut r)?),
        8 => Body::FenceRaised(FenceRaised::get(&mut r)?),
        9 => Body::FenceReleased(FenceReleased::get(&mut r)?),
        10 => Body::MotionIntent(MotionIntent::get(&mut r)?),
        11 => Body::MotionOutcome(MotionOutcome::get(&mut r)?),
        12 => Body::InventoryReconciled(InventoryReconciled::get(&mut r)?),
        13 => Body::VerificationProgress(VerificationProgress::get(&mut r)?),
        14 => Body::VerificationVerdict(VerificationVerdict::get(&mut r)?),
        15 => Body::ManualFinalizationAccepted(ManualFinalizationAccepted::get(&mut r)?),
        16 => Body::ConfigurationAccepted(ConfigurationAccepted::get(&mut r)?),
        17 => Body::ReadinessObserved(ReadinessObserved::get(&mut r)?),
        18 => Body::DriveObserved(DriveObserved::get(&mut r)?),
        19 => Body::DriveDecision(DriveDecision::get(&mut r)?),
        20 => Body::DriveHealthObserved(DriveHealthObserved::get(&mut r)?),
        21 => Body::CleaningRecorded(CleaningRecorded::get(&mut r)?),
        22 => Body::AlarmChanged(AlarmChanged::get(&mut r)?),
        23 => Body::OperationRecorded(OperationRecorded::get(&mut r)?),
        24 => Body::SessionObserved(SessionObserved::get(&mut r)?),
        25 => Body::WrapMapCached(WrapMapCached::get(&mut r)?),
        26 => Body::RecoveryRecorded(RecoveryRecorded::get(&mut r)?),
        32 => Body::Diagnostic(Diagnostic::get(&mut r)?),
        _ => return Err(invalid()),
    };
    if !r.is_empty() {
        return Err(invalid());
    }
    Ok(Record { envelope, body })
}
/// Field metadata used by the fixture schema renderer.
#[cfg(test)]
pub(super) const SCHEMA_FIELDS: &[(&str, &[(&str, &str)])] = &[
    ("ParityScheme", ParityScheme::FIELDS),
    ("Envelope", Envelope::FIELDS),
    ("TapeRef", TapeRef::FIELDS),
    ("TapeAt", TapeAt::FIELDS),
    ("DecisionRef", DecisionRef::FIELDS),
    ("GenerationChange", GenerationChange::FIELDS),
    ("CompletionEvidence", CompletionEvidence::FIELDS),
    ("Bay", Bay::FIELDS),
    ("Standalone", Standalone::FIELDS),
    ("PriorHolder", PriorHolder::FIELDS),
    ("MediumSpec", MediumSpec::FIELDS),
    ("ModelScope", ModelScope::FIELDS),
    ("Adoption", Adoption::FIELDS),
    ("Load", Load::FIELDS),
    ("Unload", Unload::FIELDS),
    ("Move", Move::FIELDS),
    ("Element", Element::FIELDS),
    ("VerificationCursor", VerificationCursor::FIELDS),
    ("DamagedRange", DamagedRange::FIELDS),
    ("Unprotected", Unprotected::FIELDS),
    ("PoolDefinition", PoolDefinition::FIELDS),
    ("PoolRule", PoolRule::FIELDS),
    ("Definitions", Definitions::FIELDS),
    ("Reason", Reason::FIELDS),
    ("Annotation", Annotation::FIELDS),
    ("Counters", Counters::FIELDS),
    ("Register", Register::FIELDS),
    ("Begin", Begin::FIELDS),
    ("Advance", Advance::FIELDS),
    ("Finish", Finish::FIELDS),
    ("AlarmRefresh", AlarmRefresh::FIELDS),
    ("AlarmRaise", AlarmRaise::FIELDS),
    ("Accepted", Accepted::FIELDS),
    ("Response", Response::FIELDS),
    ("Failed", Failed::FIELDS),
    ("Cache", Cache::FIELDS),
    ("RepairApplied", RepairApplied::FIELDS),
    ("Recovered", Recovered::FIELDS),
    ("AssignPool", AssignPool::FIELDS),
    ("Label", Label::FIELDS),
    ("Retire", Retire::FIELDS),
    ("Reprovision", Reprovision::FIELDS),
    ("MediumSpecSet", MediumSpecSet::FIELDS),
    ("SupersedeTail", SupersedeTail::FIELDS),
    (
        "AcceptUnprotectedFinalization",
        AcceptUnprotectedFinalization::FIELDS,
    ),
    ("FenceRaised", FenceRaised::FIELDS),
    ("FenceReleased", FenceReleased::FIELDS),
    ("MotionIntent", MotionIntent::FIELDS),
    ("MotionOutcome", MotionOutcome::FIELDS),
    ("InventoryReconciled", InventoryReconciled::FIELDS),
    ("VerificationProgress", VerificationProgress::FIELDS),
    ("VerificationVerdict", VerificationVerdict::FIELDS),
    (
        "ManualFinalizationAccepted",
        ManualFinalizationAccepted::FIELDS,
    ),
    ("ConfigurationAccepted", ConfigurationAccepted::FIELDS),
    ("ReadinessObserved", ReadinessObserved::FIELDS),
    ("DriveObserved", DriveObserved::FIELDS),
    ("DriveDecision", DriveDecision::FIELDS),
    ("DriveHealthObserved", DriveHealthObserved::FIELDS),
    ("CleaningRecorded", CleaningRecorded::FIELDS),
    ("AlarmChanged", AlarmChanged::FIELDS),
    ("OperationRecorded", OperationRecorded::FIELDS),
    ("SessionObserved", SessionObserved::FIELDS),
    ("WrapMapCached", WrapMapCached::FIELDS),
    ("RecoveryRecorded", RecoveryRecorded::FIELDS),
    ("Diagnostic", Diagnostic::FIELDS),
];
