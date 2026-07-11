/- V2 scalar-header and canonical key-frame model.

   The Aeneas extraction in Funs remains the established v1 scalar kernel.
   This file adds the disjoint v2 mode and the variable key-frame round-trip
   obligations. Production byte placements are pinned by the Rust drift guard. -/
import RaoHeader.Spec

namespace RaoHeader

noncomputable section

local instance (proposition : Prop) : Decidable proposition :=
  Classical.propDecidable proposition

structure V2HeaderCore where
  keyIdZero : Bool
  saltNonzero : Bool
  wrapSuite : Nat
  keyFrameLen : Nat
  reservedZero : Bool
  deriving DecidableEq

def V2HeaderValid (header : V2HeaderCore) : Prop :=
  header.keyIdZero = true ∧
  header.saltNonzero = true ∧
  header.wrapSuite = 1 ∧
  103 ≤ header.keyFrameLen ∧
  header.keyFrameLen ≤ 4096 ∧
  header.reservedZero = true

structure V2HeaderWire where
  headerLen : Nat
  formatVersion : Nat
  core : V2HeaderCore
  deriving DecidableEq

def serializeV2Header (header : V2HeaderCore) : V2HeaderWire :=
  { headerLen := 128, formatVersion := 2, core := header }

def parseV2Header (wire : V2HeaderWire) : Option V2HeaderCore :=
  if wire.headerLen = 128 ∧ wire.formatVersion = 2 ∧ V2HeaderValid wire.core then
    some wire.core
  else
    none

theorem parse_serialize_v2_header_round_trip
    (header : V2HeaderCore) (valid : V2HeaderValid header) :
    parseV2Header (serializeV2Header header) = some header := by
  simp [parseV2Header, serializeV2Header, valid]

def v1AcceptsVersion (version : Nat) : Bool := version = 1
def v2AcceptsVersion (version : Nat) : Bool := version = 2

theorem v1_v2_dispatch_disjoint (version : Nat) :
    ¬ (v1AcceptsVersion version = true ∧ v2AcceptsVersion version = true) := by
  simp [v1AcceptsVersion, v2AcceptsVersion]
  omega

structure KeyFrameSlot where
  slotIndex : Nat
  labelLen : Nat
  labelPrintable : Bool
  deriving DecidableEq

def KeyFrameValid (slots : List KeyFrameSlot) : Prop :=
  slots ≠ [] ∧
  slots.length ≤ 8 ∧
  slots.Pairwise (fun left right => left.slotIndex < right.slotIndex) ∧
  ∀ slot ∈ slots, slot.labelLen ≤ 32 ∧ slot.labelPrintable = true

def serializeKeyFrame (slots : List KeyFrameSlot) : List KeyFrameSlot := slots

def parseKeyFrame (slots : List KeyFrameSlot) : Option (List KeyFrameSlot) :=
  if KeyFrameValid slots then some slots else none

theorem parse_serialize_key_frame_round_trip
    (slots : List KeyFrameSlot) (valid : KeyFrameValid slots) :
    parseKeyFrame (serializeKeyFrame slots) = some slots := by
  simp [parseKeyFrame, serializeKeyFrame, valid]

end

end RaoHeader
