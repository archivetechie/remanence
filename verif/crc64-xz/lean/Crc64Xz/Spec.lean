/- Specification theorems for the CRC-64/XZ byte-step extraction (SPEC.md X1-X5).

   This Lean file proves the arithmetic kernel used by the shared
   `remanence-crc` implementation: the reflected bit step, byte table entry,
   update fold, selected normative vectors, and the extracted Aeneas bit-step
   function's equivalence to the bit-vector spec. The Rust `drift_guard` test
   ties this proof-facing model back to production
   `crates/remanence-crc/src/lib.rs`. It does not prove every sidecar/audit call
   site that consumes the CRC. -/
import Std.Tactic.BVDecide
import Crc64Xz.Funs

open Aeneas Aeneas.Std Result

namespace Crc64Xz

abbrev U64 := BitVec 64
abbrev U8 := BitVec 8

def ReflectedPoly : U64 := 0xC96C5795D7870F42#64
def CheckValue : U64 := 0x995DC9BBDF1939FA#64

def bitStep (crc : U64) : U64 :=
  if crc &&& 1#64 = 1#64 then
    (crc >>> 1) ^^^ ReflectedPoly
  else
    crc >>> 1

def bitStepMask (crc : U64) : U64 :=
  (crc >>> 1) ^^^ (ReflectedPoly &&& (0#64 - (crc &&& 1#64)))

theorem bit_step_mask_equiv (crc : U64) :
    bitStepMask crc = bitStep crc := by
  unfold bitStepMask bitStep ReflectedPoly
  bv_decide

def byteRemainderFromState (crc : U64) : U64 :=
  bitStep (bitStep (bitStep (bitStep (bitStep (bitStep (bitStep (bitStep crc)))))))

def tableEntry (byte : U8) : U64 :=
  byteRemainderFromState (byte.zeroExtend 64)


def tableIndex (crc : U64) (byte : U8) : U8 :=
  ((crc ^^^ byte.zeroExtend 64) &&& 0xff#64).truncate 8

def update (crc : U64) (byte : U8) : U64 :=
  (crc >>> 8) ^^^ tableEntry (tableIndex crc byte)


def foldState : List U8 -> U64 -> U64
  | [], crc => crc
  | byte :: bytes, crc => foldState bytes (update crc byte)

def crc64xz (bytes : List U8) : U64 :=
  foldState bytes 0xffffffffffffffff#64 ^^^ 0xffffffffffffffff#64

theorem crc64xz_nil :
    crc64xz [] = 0#64 := by
  native_decide

theorem crc64xz_cons (byte : U8) (bytes : List U8) :
    crc64xz (byte :: bytes) =
      foldState bytes (update 0xffffffffffffffff#64 byte) ^^^ 0xffffffffffffffff#64 := by
  rfl

theorem crc64xz_single_zero :
    crc64xz [0x00#8] = 0x1FADA17364673F59#64 := by
  native_decide

theorem crc64xz_single_ff :
    crc64xz [0xff#8] = 0xFF00000000000000#64 := by
  native_decide

theorem crc64xz_check_value :
    crc64xz [0x31#8, 0x32#8, 0x33#8, 0x34#8, 0x35#8, 0x36#8, 0x37#8, 0x38#8, 0x39#8] =
      CheckValue := by
  native_decide

theorem extracted_check_value_constant :
    CRC64_XZ_CHECK_VALUE = 0x995DC9BBDF1939FA#u64 := by
  unfold CRC64_XZ_CHECK_VALUE
  rfl

theorem extracted_reflected_poly_constant :
    CRC64_XZ_REFLECTED_POLY = 0xC96C5795D7870F42#u64 := by
  unfold CRC64_XZ_REFLECTED_POLY
  rfl

theorem core_num_u64_max_bv :
    core.num.U64.MAX.bv = 0xffffffffffffffff#64 := by
  native_decide

theorem u64_shr_one_ok (x : Std.U64) :
    x >>> 1#i32 = ok (⟨x.bv >>> 1⟩ : Std.U64) := by
  change UScalar.shiftRight_IScalar x 1#i32 = ok (⟨x.bv >>> 1⟩ : Std.U64)
  unfold UScalar.shiftRight_IScalar UScalar.shiftRight IScalar.toNat IScalar.val
  simp

def bitStepStd (crc : Std.U64) : Std.U64 :=
  ⟨bitStep crc.bv⟩

def tableEntryStd (byte : Std.U8) : Std.U64 :=
  ⟨tableEntry byte.bv⟩

def updateStd (crc : Std.U64) (byte : Std.U8) : Std.U64 :=
  ⟨update crc.bv byte.bv⟩

def byteBvs (bytes : List Std.U8) : List U8 :=
  bytes.map (fun byte => byte.bv)

def foldStateStd (bytes : List Std.U8) (crc : Std.U64) : Std.U64 :=
  ⟨foldState (byteBvs bytes) crc.bv⟩

def crc64xzStd (bytes : Slice Std.U8) : Std.U64 :=
  ⟨crc64xz (byteBvs bytes.val)⟩

theorem foldStateStd_nil (crc : Std.U64) :
    foldStateStd [] crc = crc := by
  unfold foldStateStd byteBvs foldState
  rfl

theorem foldStateStd_cons (byte : Std.U8) (bytes : List Std.U8) (crc : Std.U64) :
    foldStateStd (byte :: bytes) crc = foldStateStd bytes (updateStd crc byte) := by
  apply U64.bv_eq_imp_eq
  simp only [foldStateStd, byteBvs, updateStd, List.map_cons, foldState]

theorem foldStateStd_drop_step (bytes : List Std.U8) (i : Nat) (h : i < bytes.length)
    (crc : Std.U64) :
    foldStateStd (bytes.drop (i + 1)) (updateStd crc bytes[i]) =
      foldStateStd (bytes.drop i) crc := by
  rw [List.drop_eq_getElem_cons h, foldStateStd_cons]

theorem generated_bit_step_matches_spec (crc : Std.U64) :
    crc64_xz_bit_step crc = ok (bitStepStd crc) := by
  unfold crc64_xz_bit_step bitStepStd bitStep CRC64_XZ_REFLECTED_POLY ReflectedPoly
  by_cases h : crc &&& 1#u64 = 1#u64
  · simp [h, u64_shr_one_ok, lift]
    have hbv : crc.bv &&& 1#64 = 1#64 := by
      exact congrArg UScalar.bv h
    simp [hbv]
    apply U64.bv_eq_imp_eq
    simp
  · simp [h, u64_shr_one_ok, lift]
    intro hbv
    exfalso
    apply h
    apply U64.bv_eq_imp_eq
    exact hbv

theorem generated_table_entry_matches_spec (byte : Std.U8) :
    crc64_xz_table_entry byte = ok (tableEntryStd byte) := by
  unfold crc64_xz_table_entry tableEntryStd tableEntry byteRemainderFromState
  simp [lift, UScalar.cast, generated_bit_step_matches_spec, bitStepStd]

theorem u64_shr_eight_ok (x : Std.U64) :
    x >>> 8#i32 = ok (⟨x.bv >>> 8⟩ : Std.U64) := by
  change UScalar.shiftRight_IScalar x 8#i32 = ok (⟨x.bv >>> 8⟩ : Std.U64)
  unfold UScalar.shiftRight_IScalar UScalar.shiftRight IScalar.toNat IScalar.val
  simp

theorem generated_update_matches_spec (crc : Std.U64) (byte : Std.U8) :
    crc64_xz_update crc byte = ok (updateStd crc byte) := by
  unfold crc64_xz_update updateStd update tableIndex
  simp [lift, u64_shr_eight_ok, generated_table_entry_matches_spec]
  apply U64.bv_eq_imp_eq
  simp [tableEntryStd, core.convert.num.FromU64U8.from]

theorem generated_crc64_xz_loop_matches_fold
    (iter : core.slice.iter.Iter Std.U8) (crc : Std.U64)
    (hidx : iter.i ≤ iter.slice.val.length) :
    crc64_xz_loop iter crc = ok (foldStateStd (iter.slice.val.drop iter.i) crc) := by
  let target := foldStateStd (iter.slice.val.drop iter.i) crc
  have hspec : crc64_xz_loop iter crc ⦃ fun out => out = target ⦄ := by
    unfold crc64_xz_loop
    apply loop.spec_decr_nat
      (measure := fun state : core.slice.iter.Iter Std.U8 × Std.U64 =>
        state.1.slice.val.length - state.1.i)
      (inv := fun state =>
        state.1.slice = iter.slice ∧
        state.1.i ≤ state.1.slice.val.length ∧
        foldStateStd (state.1.slice.val.drop state.1.i) state.2 = target)
      (post := fun out => out = target)
    · intro state hinv
      rcases state with ⟨it, acc⟩
      rcases hinv with ⟨hslice, hle, hfold⟩
      have hslice' : it.slice = iter.slice := by
        simpa using hslice
      have hle' : it.i ≤ it.slice.val.length := by
        simpa using hle
      unfold crc64_xz_loop.body core.slice.iter.IteratorSliceIter.next
      by_cases hlt : it.i < it.slice.val.length
      · have hstep :
            foldStateStd (it.slice.val.drop (it.i + 1)) (updateStd acc it.slice[it.i]) =
              target := by
          have hdrop :=
            foldStateStd_drop_step it.slice.val it.i hlt acc
          rw [← hfold]
          simpa [Slice.getElem_Nat_eq] using hdrop
        have hnew_le : it.i + 1 ≤ it.slice.val.length := by
          omega
        have hmeasure : it.slice.val.length - (it.i + 1) < it.slice.val.length - it.i := by
          omega
        simpa [hlt, generated_update_matches_spec, hnew_le, hmeasure] using
          And.intro hslice' hstep
      · have hi_eq : it.i = it.slice.val.length := by
          have hnot_lt : it.slice.val.length ≤ it.i := Nat.le_of_not_gt hlt
          omega
        have hdone : acc = target := by
          rw [hi_eq, List.drop_length, foldStateStd_nil] at hfold
          exact hfold
        simp [hlt, hdone]
    · exact ⟨rfl, hidx, rfl⟩
  obtain ⟨out, hout, hpost⟩ := WP.spec_imp_exists hspec
  simpa [hpost] using hout

theorem generated_crc64_xz_matches_spec (bytes : Slice Std.U8) :
    crc64_xz bytes = ok (crc64xzStd bytes) := by
  unfold crc64_xz
  simp [SharedSlice.Insts.CoreIterTraitsCollectIntoIteratorSharedIter.into_iter,
    generated_crc64_xz_loop_matches_fold]
  apply U64.bv_eq_imp_eq
  simp only [foldStateStd, crc64xzStd, crc64xz, byteBvs, core_num_u64_max_bv,
    UScalar.bv_xor]

def resultU64Eq (result : Result Std.U64) (expected : Std.U64) : Bool :=
  match result with
  | ok actual => actual == expected
  | _ => false

theorem extracted_single_zero_vector :
    resultU64Eq (crc64_xz_one 0x00#u8) 0x1FADA17364673F59#u64 = true := by
  native_decide

theorem extracted_single_ff_vector :
    resultU64Eq (crc64_xz_one 0xff#u8) 0xFF00000000000000#u64 = true := by
  native_decide

theorem extracted_check_value_vector :
    resultU64Eq
      (crc64_xz_nine
        0x31#u8 0x32#u8 0x33#u8 0x34#u8 0x35#u8
        0x36#u8 0x37#u8 0x38#u8 0x39#u8)
      0x995DC9BBDF1939FA#u64 = true := by
  native_decide

end Crc64Xz
