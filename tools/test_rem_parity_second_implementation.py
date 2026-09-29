#!/usr/bin/env python3
"""Tests for the REM-PARITY generation-2 second implementation.

Run with ``python3 -m unittest tools/test_rem_parity_second_implementation.py``.

Two environment variables are optional:

* ``REM_PARITY_SECOND_IMPL_SCRATCH`` names a directory the tests may write
  into (default: ``test-scratch`` under the second-implementation output
  directory, removed afterwards);
* ``REM_PARITY_SECOND_IMPL_CASES`` names a directory of damage-case JSON files
  for the determinism test (default: three synthetic cases written by the test);
* ``REM_PARITY_SECOND_IMPL_RESUME`` names a directory of resume-case JSON files
  for the resume determinism test (default: synthetic cases written by the test);
* ``REM_PARITY_SECOND_IMPL_NEGATIVES`` names a negative-case JSON file for the
  negatives determinism test (default: a small synthetic file written by the test);
* ``REM_PARITY_SECOND_IMPL_SUPPLEMENT`` names a supplement-variant JSON file for
  the supplement tests (default: a small synthetic file written by the test);
* ``REM_PARITY_SECOND_IMPL_MUTATIONS`` names a terminal-mutation JSON file for the
  mutation tests (default: ``blind-inputs/mutations-blind.json``);
* ``REM_PARITY_SECOND_IMPL_SELECTION`` names a survivor-set JSON file for the
  selection tests (default: ``blind-inputs/selection-blind.json``).
"""

from __future__ import annotations

import contextlib
import copy
import dataclasses
import hashlib
import io
import itertools
import json
import os
import pathlib
import shutil
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import rem_parity_second_implementation as impl  # noqa: E402

DEFAULT_SCRATCH = impl.OUTPUT_ROOT / "test-scratch"
SCRATCH = pathlib.Path(os.environ.get("REM_PARITY_SECOND_IMPL_SCRATCH", DEFAULT_SCRATCH))
CASES = os.environ.get("REM_PARITY_SECOND_IMPL_CASES")
RESUME_CASES = os.environ.get("REM_PARITY_SECOND_IMPL_RESUME")
NEGATIVE_CASES = os.environ.get("REM_PARITY_SECOND_IMPL_NEGATIVES")
SUPPLEMENT_CASES = os.environ.get("REM_PARITY_SECOND_IMPL_SUPPLEMENT")


def setUpModule() -> None:
    SCRATCH.mkdir(parents=True, exist_ok=True)


def tearDownModule() -> None:
    if SCRATCH == DEFAULT_SCRATCH:
        shutil.rmtree(SCRATCH, ignore_errors=True)


# ---------------------------------------------------------------------------
# The imported arithmetic against Sections 5.1, 6 and 7.3 and Appendix A.
# ---------------------------------------------------------------------------


class ArithmeticTests(unittest.TestCase):
    def test_crc64_vectors_section_5_1(self) -> None:
        vectors = [
            (b"123456789", 0x995DC9BBDF1939FA),
            (b"", 0),
            (b"\x00", 0x1FADA17364673F59),
            (b"\xff", 0xFF00000000000000),
            (b"\x00" * 262144, 0x261BDF3D299838FC),
            (b"\xff" * 262144, 0x55433DD0F38908BA),
        ]
        for data, expected in vectors:
            self.assertEqual(impl.crc64_xz(data), expected, data[:4])
        self.assertEqual(impl.le64(impl.crc64_xz(b"123456789")), bytes.fromhex("fa3919dfbbc95d99"))

    def test_crc64_parameters_section_5_1(self) -> None:
        self.assertEqual(impl.CRC64_XZ_REFLECTED_POLYNOMIAL, 0xC96C5795D7870F42)
        reflected = int(f"{0x42F0E1EBA9EA3693:064b}"[::-1], 2)
        self.assertEqual(reflected, impl.CRC64_XZ_REFLECTED_POLYNOMIAL)
        self.assertEqual(impl.MASK64, 0xFFFF_FFFF_FFFF_FFFF)

    def test_field_section_6_1(self) -> None:
        self.assertEqual(impl.GF_REDUCTION_POLYNOMIAL, 0x11D)
        for value in range(1, 256):
            self.assertEqual(impl.gf_mul(value, impl.gf_inv(value)), 1)
        self.assertRaises(impl.RederivationError, impl.gf_inv, 0)
        for coefficient in (0, 1, 2, 0x8E, 0xF4, 0xFF):
            table = impl._multiplication_table(coefficient)
            self.assertEqual(table, bytes(impl.gf_mul(coefficient, b) for b in range(256)))

    def test_inverses_and_generator_section_6_8(self) -> None:
        self.assertEqual(impl.gf_inv(0x02), 0x8E)
        self.assertEqual(impl.gf_inv(0x03), 0xF4)
        self.assertEqual(impl.cauchy_matrix(2, 2), ((0x8E, 0xF4), (0xF4, 0x8E)))

    def test_parity_section_6_8(self) -> None:
        matrix = impl.cauchy_matrix(2, 2)
        parity = impl.encode_parity([bytes.fromhex("01020304"), bytes.fromhex("10203040")], matrix)
        self.assertEqual(parity, (bytes.fromhex("75EA9FC9"), bytes.fromhex("FCE519D7")))

    def test_incremental_equals_batch_section_6_3(self) -> None:
        k, m = 4, 3
        matrix = impl.cauchy_matrix(k, m)
        shards = [bytes((i * 37 + j * 11) % 256 for j in range(64)) for i in range(k)]
        batch = impl.encode_parity(shards, matrix)
        for order in (range(k), reversed(range(k))):
            accumulators = [0] * m
            for i in order:
                for j in range(m):
                    accumulators[j] ^= int.from_bytes(impl.combine([matrix[j][i]], [shards[i]]), "big")
            self.assertEqual(tuple(a.to_bytes(64, "big") for a in accumulators), batch)

    def test_every_erasure_pattern_section_6_8(self) -> None:
        k, m = 2, 2
        matrix = impl.cauchy_matrix(k, m)
        data = [bytes.fromhex("01020304"), bytes.fromhex("10203040")]
        parity = impl.encode_parity(data, matrix)
        shards = list(data) + list(parity)
        rows = [[1, 0], [0, 1]] + [list(row) for row in matrix]
        for lost in range(0, m + 1):
            for erased in itertools.combinations(range(k + m), lost):
                survivors = [i for i in range(k + m) if i not in erased][:k]
                for target in range(k):
                    inverse = impl.gf_solve_row([rows[i] for i in survivors], target, k)
                    self.assertEqual(impl.combine(inverse, [shards[i] for i in survivors]), data[target],
                                     (erased, target))

    def test_canonical_digest_section_7_3_and_appendix_a3(self) -> None:
        entries = [impl.MapEntry(0, 2, 1), impl.MapEntry(1, 0, 3, 0), impl.MapEntry(2, 1, 2, None, 0, 3, 7)]
        encoded = impl.canonical_projection(entries)
        self.assertEqual(encoded, bytes.fromhex("8387000201f6f6f6f68701000300f6f6f687020102f6000307"))
        self.assertEqual(len(encoded), 25)
        self.assertEqual(hashlib.sha256(encoded).hexdigest(),
                         "548ca6c967073a6c1ad011d10fc132c2739e251d015ea45a628bbec96892c26b")

    def test_ordinal_mapping_appendix_a1(self) -> None:
        start, stripes, ordinal = 65536, 512, 100000
        offset = ordinal - start
        self.assertEqual((offset, offset % stripes, offset // stripes), (34464, 160, 67))
        self.assertEqual(start + 67 * stripes + 160, ordinal)

    def test_index_layout_appendix_a2(self) -> None:
        header_blocks, inline, placements = impl.sidecar_layout(262144, 512, 4, 65536)
        self.assertEqual((header_blocks, inline), (3, 261936))
        self.assertEqual(2 * header_blocks + 512 * 4 + 1, 2055)
        self.assertEqual([len(block) for block in placements], [2048 + 28646, 32767, 4123])

    def test_constants_section_2_5(self) -> None:
        self.assertEqual(impl.LABEL_SIDECAR, b"REM\x00PAR\x01")
        self.assertEqual(impl.LABEL_SIDECAR_FOOTER, b"REM\x00PARFOOT\x01")
        self.assertEqual(len(impl.LABEL_PARITY_MAP), 9)
        self.assertEqual(impl.LABEL_REPLICA_HEADER, b"REM\x00TIREP\x01H")
        self.assertEqual(impl.LABEL_REPLICA_FOOTER, b"REM\x00TIREP\x01F")
        self.assertEqual(impl.LABEL_SEPARATION_HEADER, b"REM\x00TISEP\x01H")
        self.assertEqual(impl.LABEL_SEPARATION_FOOTER, b"REM\x00TISEP\x01F")
        self.assertEqual(impl.BOOTSTRAP_MAGIC_BYTES, b"REM\x00BOO\x01")
        self.assertEqual(len(impl.SIDECAR_METADATA_DOMAIN), 29)

    def test_deterministic_cbor_section_5_3(self) -> None:
        self.assertEqual(impl.encode_deterministic_cbor({10: 0, 2: 0, 24: 0, -1: 0}),
                         bytes.fromhex("a4" "0200" "0a00" "2000" "181800"))
        for bad in ("1801", "a20200 0100", "9f00ff", "f93c00", "c100", "a2010001 00"):
            with self.assertRaises(impl.RederivationError, msg=bad):
                impl.decode_deterministic_cbor(bytes.fromhex(bad.replace(" ", "")))

    def test_definitions_are_imported_from_the_two_tools(self) -> None:
        import rem_parity_rederive
        import verify_rem_object_vectors_independent
        for name in ("gf_mul", "gf_inv", "cauchy_matrix", "_multiplication_table", "encode_parity", "crc64_xz",
                     "_cbor_type_and_length", "encode_deterministic_cbor", "decode_deterministic_cbor",
                     "RederivationError"):
            self.assertIs(getattr(impl, name), getattr(rem_parity_rederive, name), name)
        for name in ("build_plaintext_with_manifest", "FileSpec"):
            self.assertIs(getattr(impl, name), getattr(verify_rem_object_vectors_independent, name), name)

    def test_decision_quotes_occur_in_the_text(self) -> None:
        text = " ".join(impl.SPEC_PATH.read_text(encoding="utf-8").split())
        for key, (section, quote) in impl.QUOTES.items():
            self.assertIn(" ".join(quote.split()), text, f"{key} (Section {section})")


# ---------------------------------------------------------------------------
# Mutation helpers.
# ---------------------------------------------------------------------------

HEX = set("0123456789abcdefABCDEF")


def leaf_paths(value, prefix=()):
    if isinstance(value, dict) and value:
        for key, item in value.items():
            yield from leaf_paths(item, prefix + (key,))
    elif isinstance(value, list) and value:
        for index, item in enumerate(value):
            yield from leaf_paths(item, prefix + (index,))
    else:
        yield prefix


def get_path(value, path):
    for key in path:
        value = value[key]
    return value


def set_path(value, path, new):
    for key in path[:-1]:
        value = value[key]
    value[path[-1]] = new


def mutated_value(path, value):
    key = path[-1]
    if key == "capacity_bytes":
        return 1
    if isinstance(value, bool):
        return not value
    if value is None:
        return {"executable": True, "link_target": "target", "mtime": "1"}.get(key, 0)
    if isinstance(value, int):
        return value + 1
    if isinstance(value, str):
        if value and set(value) <= HEX | {"-"} and any(c in HEX for c in value):
            last = max(i for i, c in enumerate(value) if c in HEX)
            replacement = "0" if value[last] != "0" else "1"
            return value[:last] + replacement + value[last + 1 :]
        return value + "x"
    if isinstance(value, dict):
        return {"xattrs": {"user.test": "00"}}.get(key, {"org.example.test": 1})
    if isinstance(value, list):
        return [0]
    raise AssertionError(f"no mutation for {path}")


def mutations(inputs, skip=()):
    for path in leaf_paths(inputs):
        if path and path[-1] in skip:
            continue
        mutated = copy.deepcopy(inputs)
        set_path(mutated, path, mutated_value(path, get_path(inputs, path)))
        yield path, mutated


def corrupt(data: bytes, position: int) -> bytes:
    return data[:position] + bytes([data[position] ^ 0x5A]) + data[position + 1 :]


def image_with_block(image, tape_file_number, block_index, block):
    files = []
    for tape_file in image.files:
        blocks = list(tape_file.blocks)
        if tape_file.tape_file_number == tape_file_number:
            blocks[block_index] = block
        files.append(dataclasses.replace(tape_file, blocks=blocks))
    return dataclasses.replace(image, files=files)


# ---------------------------------------------------------------------------
# The smallest image: every input field, every tape file, the candidate digests.
# ---------------------------------------------------------------------------


class ImageComparisonTests(unittest.TestCase):
    NAME = "a4-minimal"

    @classmethod
    def setUpClass(cls) -> None:
        cls.inputs = impl.load_image_inputs(cls.NAME)
        cls.rows = impl.read_tsv(impl.FIXTURE_ROOT / "tape-images" / "MANIFEST.tsv")
        cls.reference = impl.build_image(cls.inputs, cls.NAME)
        results = impl.compare_image(cls.reference, cls.rows)
        assert all(result.reproduced for result in results), [r.detail for r in results if not r.reproduced]

    def report(self, inputs) -> list[str]:
        try:
            image = impl.build_image(inputs, self.NAME)
        except impl.BuildRefusal as refusal:
            return [f"build refused: field {refusal.field_name}: {refusal.message}"]
        return [result.detail for result in impl.compare_image(image, self.rows, self.reference)
                if not result.reproduced]

    def test_reference_reproduces_the_manifest(self) -> None:
        self.assertTrue(all(result.reproduced for result in impl.compare_image(self.reference, self.rows)))

    def test_every_input_field_change_is_reported(self) -> None:
        count = 0
        for path, mutated in mutations(self.inputs):
            count += 1
            with self.subTest(field=".".join(map(str, path))):
                report = self.report(mutated)
                self.assertTrue(report, "no mismatch reported")
                self.assertTrue(all("first differing byte" in line or "field" in line for line in report), report)
        self.assertGreater(count, 30)

    def test_changed_tape_file_bytes_are_located(self) -> None:
        for tape_file in self.reference.files:
            data = b"".join(tape_file.blocks)
            for position in sorted({0, len(data) // 2, len(data) - 1}):
                with self.subTest(tape_file=tape_file.tape_file_number, position=position):
                    block_index, offset = divmod(position, self.reference.block_size)
                    block = corrupt(tape_file.blocks[block_index], offset)
                    damaged = image_with_block(self.reference, tape_file.tape_file_number, block_index, block)
                    results = impl.compare_image(damaged, self.rows, self.reference)
                    failed = {r.artifact: r.detail for r in results if not r.reproduced}
                    key = f"{self.NAME} tape file {tape_file.tape_file_number}"
                    self.assertIn(key, failed)
                    self.assertIn(f"first differing byte {position}:", failed[key])
                    self.assertIn(f"{self.NAME} tape file ALL", failed)

    def test_changed_candidate_digest_is_reported(self) -> None:
        for row_index in range(len(self.rows)):
            if self.rows[row_index]["image"] != self.NAME:
                continue
            rows = copy.deepcopy(self.rows)
            rows[row_index]["sha256"] = rows[row_index]["sha256"][:-1] + (
                "0" if rows[row_index]["sha256"][-1] != "0" else "1")
            with self.subTest(tape_file=rows[row_index]["tape_file"]):
                failed = [r.detail for r in impl.compare_image(self.reference, rows) if not r.reproduced]
                self.assertEqual(len(failed), 1)
                self.assertIn("field sha256", failed[0])


class UnfinalizedImageComparisonTests(ImageComparisonTests):
    """The smallest image by bytes. Its Writer never finalizes, so the inputs that
    only finalization reads (edition, extent size, ParityMap sequence) are recorded
    but byte-neutral; the test checks that and every other field."""

    NAME = "unfinalized-closed"
    FINALIZATION_ONLY = ("edition_id_hex", "edition_sequence", "nominal_extent_bytes", "parity_map_sequence_start")

    def test_every_input_field_change_is_reported(self) -> None:
        for path, mutated in mutations(self.inputs):
            with self.subTest(field=".".join(map(str, path))):
                report = self.report(mutated)
                if path[-1] in self.FINALIZATION_ONLY:
                    self.assertEqual(report, [])
                else:
                    self.assertTrue(report, "no mismatch reported")


# ---------------------------------------------------------------------------
# One terminal profile.
# ---------------------------------------------------------------------------


class ProfileComparisonTests(unittest.TestCase):
    NAME = "multi-256k"

    @classmethod
    def setUpClass(cls) -> None:
        cls.inputs = impl.load_json(impl.FIXTURE_ROOT / cls.NAME / "inputs.json")
        cls.rows = impl.read_tsv(impl.FIXTURE_ROOT / "MANIFEST.tsv")
        cls.candidates = impl.FIXTURE_ROOT / cls.NAME
        results = impl.compare_profile(cls.NAME, cls.inputs, cls.rows, cls.candidates)
        assert all(result.reproduced for result in results)

    def report(self, inputs, candidates=None) -> list[str]:
        try:
            results = impl.compare_profile(self.NAME, inputs, self.rows, candidates or self.candidates)
        except impl.BuildRefusal as refusal:
            return [f"build refused: field {refusal.field_name}: {refusal.message}"]
        return [result.detail for result in results if not result.reproduced]

    def test_every_input_field_change_is_reported(self) -> None:
        # "description" is prose describing the file, not an input to any byte.
        for path, mutated in mutations(self.inputs, skip=("description",)):
            with self.subTest(field=".".join(map(str, path))):
                report = self.report(mutated)
                self.assertTrue(report, "no mismatch reported")
                self.assertTrue(all("first differing byte" in line or "field" in line or "length differs" in line
                                    for line in report), report)

    def test_changed_component_bytes_are_located(self) -> None:
        terminal = impl.build_terminal_suffix(impl.terminal_inputs_from_profile(self.inputs))
        for index, filename in enumerate(impl.PROFILE_COMPONENT_FILES):
            built = b"".join(terminal.components[index])
            candidate = (self.candidates / filename).read_bytes()
            for position in sorted({0, len(built) // 2, len(built) - 1}):
                with self.subTest(component=filename, position=position):
                    result = impl.compare_bytes(filename, corrupt(built, position), candidate,
                                                impl.component_describer(self.inputs, terminal, index))
                    self.assertFalse(result.reproduced)
                    self.assertIn(f"first differing byte {position} ", result.detail)

    def test_changed_candidate_file_is_reported(self) -> None:
        copy_dir = SCRATCH / "candidate-copy"
        shutil.rmtree(copy_dir, ignore_errors=True)
        shutil.copytree(self.candidates, copy_dir, ignore=shutil.ignore_patterns("*.json"))
        for filename in impl.PROFILE_COMPONENT_FILES:
            original = (copy_dir / filename).read_bytes()
            for position in (0, 262144 + 70, len(original) - 1):
                with self.subTest(component=filename, position=position):
                    (copy_dir / filename).write_bytes(corrupt(original, position))
                    report = self.report(self.inputs, copy_dir)
                    self.assertTrue(any(f"first differing byte {position} " in line for line in report), report)
            (copy_dir / filename).write_bytes(original)
        shutil.rmtree(copy_dir, ignore_errors=True)


# ---------------------------------------------------------------------------
# decide is deterministic.
# ---------------------------------------------------------------------------

SYNTHETIC_CASES = {
    "synthetic-replica-headers": {
        "failed_data_addresses": [], "hints": None, "image": "a4-minimal", "removed_filemark_after_tape_file": None,
        "unreadable_records": [{"filemark": False, "lba": 19, "record_index": 0, "tape_file": 4},
                               {"filemark": False, "lba": 35, "record_index": 0, "tape_file": 8}]},
    "synthetic-object-block": {
        "failed_data_addresses": [[1, 1]], "hints": None, "image": "a4-minimal",
        "removed_filemark_after_tape_file": None,
        "unreadable_records": [{"filemark": False, "lba": 3, "record_index": 1, "tape_file": 1}]},
    "synthetic-all-headers": {
        "failed_data_addresses": [[1, 0]], "hints": None, "image": "a4-minimal",
        "removed_filemark_after_tape_file": None,
        "unreadable_records": [{"filemark": False, "lba": lba, "record_index": 0, "tape_file": tf}
                               for lba, tf in ((2, 1), (19, 4), (27, 6), (35, 8))]},
}


class DecideDeterminismTests(unittest.TestCase):
    def test_decide_twice_gives_identical_output(self) -> None:
        if CASES:
            paths = sorted(pathlib.Path(CASES).glob("*.json"))
        else:
            case_dir = SCRATCH / "synthetic-cases"
            case_dir.mkdir(parents=True, exist_ok=True)
            paths = []
            for name, case in SYNTHETIC_CASES.items():
                path = case_dir / f"{name}.json"
                path.write_text(json.dumps(case), encoding="utf-8")
                paths.append(path)
        self.assertTrue(paths)
        first = SCRATCH / "decisions-first.json"
        second = SCRATCH / "decisions-second.json"
        impl.run_decide(paths, first)
        impl.run_decide(list(reversed(paths)), second)
        self.assertEqual(first.read_bytes(), second.read_bytes())
        decisions = json.loads(first.read_text(encoding="utf-8"))
        self.assertEqual(decisions["schema"], "rem-parity-second-implementation-decisions/1")
        self.assertEqual(sorted(decisions["cases"]), sorted(impl.case_id_of(path) for path in paths))

    def test_fault_map_case_id_is_its_directory(self) -> None:
        case_dir = SCRATCH / "repository-layout" / "replica-headers"
        case_dir.mkdir(parents=True, exist_ok=True)
        path = case_dir / "fault-map.json"
        path.write_text(json.dumps(SYNTHETIC_CASES["synthetic-replica-headers"]), encoding="utf-8")
        self.assertEqual(impl.case_id_of(path), "replica-headers")
        out = SCRATCH / "repository-layout" / "decisions.json"
        decisions = impl.run_decide([path], out)
        self.assertEqual(list(decisions["cases"]), ["replica-headers"])
        self.assertTrue((SCRATCH / "repository-layout" / "decisions-trace.json").exists())


# ---------------------------------------------------------------------------
# resume (Section 14).
# ---------------------------------------------------------------------------


def entry_json(entry):
    return {"block_count": entry.block_count, "epoch_id": entry.epoch_id,
            "first_parity_data_ordinal": entry.first_parity_data_ordinal, "kind": impl.KIND_NAMES[entry.kind],
            "protected_ordinal_end_exclusive": entry.protected_ordinal_end_exclusive,
            "protected_ordinal_start": entry.protected_ordinal_start, "tape_file_number": entry.tape_file_number}


APPEND_OBJECT = {"files": [], "options": {
    "caller_object_id": "test-append", "chunk_size": 262144, "encryption": "none", "extensions": {},
    "manifest_file_id": "00000000-0000-4000-8000-0000000000aa", "metadata_preservation": "archival",
    "object_id": "00000000-0000-4000-8000-0000000000ab", "write_timestamp": "2026-08-09T00:00:00Z"}}


def synthetic_resume_cases():
    image = impl.build_image(impl.load_image_inputs("unfinalized-open"), "unfinalized-open")
    prefix = [entry_json(e) for e in image.prefix_entries]
    accepted = {"image": "unfinalized-open", "committed_prefix": prefix, "W": 4, "T": 6, "append_object": APPEND_OBJECT}
    gapped = json.loads(json.dumps(accepted))
    gapped["committed_prefix"][2]["epoch_id"] = 1
    return {"synthetic-accepted": accepted, "synthetic-epoch-gap": gapped}


class ResumeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.case_dir = SCRATCH / "resume-cases"
        cls.case_dir.mkdir(parents=True, exist_ok=True)
        cls.synthetic = {}
        for name, case in synthetic_resume_cases().items():
            path = cls.case_dir / f"{name}.json"
            path.write_text(json.dumps(case), encoding="utf-8")
            cls.synthetic[name] = path

    def test_resume_twice_gives_identical_output(self) -> None:
        paths = sorted(pathlib.Path(RESUME_CASES).glob("*.json")) if RESUME_CASES else sorted(self.synthetic.values())
        first, second = SCRATCH / "resume-first.json", SCRATCH / "resume-second.json"
        impl.run_resume(paths, first)
        impl.run_resume(list(reversed(paths)), second)
        self.assertEqual(first.read_bytes(), second.read_bytes())
        self.assertEqual(json.loads(first.read_text())["schema"], "rem-parity-second-implementation-resume/1")

    def test_accepted_resume_equals_one_uninterrupted_session(self) -> None:
        out = impl.run_resume([self.synthetic["synthetic-accepted"]], SCRATCH / "resume-accepted.json")
        decision = out["cases"]["synthetic-accepted"]
        self.assertEqual(decision["decision"]["result"], "accepted")
        self.assertEqual(decision["step3"]["lbas"], [15, 16])
        self.assertEqual(decision["step4"]["append_point_lba"], 18)
        self.assertTrue(decision["append"]["uninterrupted_equal"])
        rows = decision["append"]["tape_files"]
        pinned = {r["tape_file"]: r["sha256"] for r in impl.read_tsv(impl.FIXTURE_ROOT / "tape-images" / "MANIFEST.tsv")
                  if r["image"] == "unfinalized-open"}
        for tape_file in ("0", "1", "2", "3"):
            self.assertEqual(rows[int(tape_file)]["sha256"], pinned[tape_file])

    def test_step2_refusal_reads_nothing(self) -> None:
        out = impl.run_resume([self.synthetic["synthetic-epoch-gap"]], SCRATCH / "resume-gap.json")
        decision = out["cases"]["synthetic-epoch-gap"]["decision"]
        self.assertEqual((decision["result"], decision["error"], decision["refused_at"]),
                         ("refused", "ResumeAppend", "step 2"))
        self.assertEqual(decision["records_read"], [])
        self.assertIs(decision["before_any_tape_read"], True)


# ---------------------------------------------------------------------------
# negatives (design D6).
# ---------------------------------------------------------------------------

SYNTHETIC_NEGATIVES = {"description": "synthetic subset for the tests", "conventions": {}, "cases": [
    {"id": "neg-13", "target": "sidecar header logical_shard_count rule (Section 9.2)"},
    {"id": "neg-21", "target": "replica record-geometry validation", "variants": [{"variant": "a-64s"}]},
    {"id": "neg-33", "target": "directory-assisted tail rescue", "variants": [{"variant": "a-H-seven"}]},
    {"id": "neg-35", "target": "the Resumer (Section 14 step 2)"},
    {"id": "neg-26", "target": "Writer capacity admission", "evaluation": "Only the Writer evaluates this."},
]}


class NegativeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if NEGATIVE_CASES:
            cls.path = pathlib.Path(NEGATIVE_CASES)
        else:
            cls.path = SCRATCH / "negatives.json"
            cls.path.write_text(json.dumps(SYNTHETIC_NEGATIVES), encoding="utf-8")

    def test_negatives_twice_gives_identical_output(self) -> None:
        first, second = SCRATCH / "negatives-first.json", SCRATCH / "negatives-second.json"
        impl.run_negatives(self.path, first)
        impl.run_negatives(self.path, second)
        self.assertEqual(first.read_bytes(), second.read_bytes())
        self.assertEqual(json.loads(first.read_text())["schema"], "rem-parity-second-implementation-negatives/1")

    def test_every_self_check_agrees_and_every_mutation_resolves(self) -> None:
        out = impl.run_negatives(self.path, SCRATCH / "negatives-check.json")
        for entry_id, entry in out["entries"].items():
            with self.subTest(entry=entry_id):
                self.assertIsNot(entry["apply"]["resolved"], False)
                if entry["self_check"]["agrees"] is not None:
                    self.assertTrue(entry["self_check"]["agrees"], entry["self_check"]["detail"])

    def test_a_from_value_that_differs_is_reported_not_guessed(self) -> None:
        ws = impl.Workspace(image=impl._image_cache("a4-minimal"))
        res = impl.Resolution()
        impl.edit_int(ws, res, (2, 0), 0x40, 8, 99, 5, "logical_shard_count")
        self.assertFalse(res.resolved)
        self.assertEqual(res.checks[0]["found"], 4)


SYNTHETIC_SUPPLEMENT = {"description": "synthetic subset for the tests", "conventions": {}, "variants": [
    {"id": "sup-01", "parent_blind_id": "neg-45", "group": "7c", "mutation": {"inputs": "L = 2^64 - 1"}},
    {"id": "sup-05", "parent_blind_id": "neg-22", "group": "7a", "every_other_rule_holds": "synthetic"},
    {"id": "sup-10", "parent_blind_id": "neg-52", "group": "7a", "every_other_rule_holds": "synthetic"},
    {"id": "sup-37", "parent_blind_id": "neg-43", "group": "7a", "every_other_rule_holds": "synthetic"},
    {"id": "sup-39", "parent_blind_id": "neg-56", "group": "7c", "every_other_rule_holds": "synthetic"},
]}


class SupplementTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if SUPPLEMENT_CASES:
            cls.path = pathlib.Path(SUPPLEMENT_CASES)
        else:
            cls.path = SCRATCH / "supplement.json"
            cls.path.write_text(json.dumps(SYNTHETIC_SUPPLEMENT), encoding="utf-8")

    def test_supplement_twice_gives_identical_output(self) -> None:
        first, second = SCRATCH / "supplement-first.json", SCRATCH / "supplement-second.json"
        impl.run_supplement(self.path, first)
        impl.run_supplement(self.path, second)
        self.assertEqual(first.read_bytes(), second.read_bytes())

    def test_every_variant_resolves_and_self_checks(self) -> None:
        out = impl.run_supplement(self.path, SCRATCH / "supplement-check.json")
        for entry_id, entry in out["entries"].items():
            with self.subTest(entry=entry_id):
                self.assertIsNot(entry["apply"]["resolved"], False)
                self.assertTrue(entry["self_check"]["agrees"], entry["self_check"]["detail"])

    def test_the_auditor_counts_one_rule_under_the_text_reading(self) -> None:
        out = impl.run_supplement(self.path, SCRATCH / "supplement-audit.json")
        for entry_id in ("sup-05", "sup-10", "sup-37"):
            entry = out["entries"].get(entry_id)
            if entry is None:
                continue
            with self.subTest(entry=entry_id):
                view = entry["isolation"]["by_copy"]["primary"]
                text_key = "text (H = 0x60 field, P = S × m); hash over wire bytes"
                self.assertEqual(len(view[text_key]), 1, view[text_key])

    def test_a_rebuilt_tape_keeps_valid_replicas(self) -> None:
        values = dict(impl.a4_sidecar_values(), total=8)
        image = impl._image_cache("a4-minimal")
        parity = image.files[2].blocks[1:5]
        entries = [__import__("struct").pack("<IHHQ", s, j, 0, impl.crc64_xz(parity[j * 2 + s]))
                   for s in range(2) for j in range(2)] + [impl.le64(impl.crc64_xz(b)) for b in image.files[1].blocks]
        blocks, metadata_hash = impl.build_sidecar_explicit(image.tape_uuid, image.block_size, values, entries, parity, 1, 1)
        rebuilt = impl.rebuild_a4(blocks, impl.MapEntry(2, 1, 8, None, 0, 4, 0),
                                  {1: 2, 2: 0, 3: 0, 4: 4, 5: 8, 6: 1, 7: 4, 8: metadata_hash, 9: 6})
        self.assertIn("A valid, B valid, C valid", impl.role_terminal_image(rebuilt))
        self.assertEqual(impl.image_rows(rebuilt)[-1]["eod_record"], "40")


# ---------------------------------------------------------------------------
# negative-blocks: the negatives' mutated-block digests.
# ---------------------------------------------------------------------------

SYNTHETIC_BLOCK_NEGATIVES = {"description": "synthetic subset for the tests", "conventions": {}, "cases": [
    {"id": "neg-13"}, {"id": "neg-21", "variants": [{"variant": "a-64s"}]},
    {"id": "neg-33", "variants": [{"variant": "a-H-seven"}]}, {"id": "neg-44"}, {"id": "neg-26"}]}
SYNTHETIC_BLOCK_SUPPLEMENT = {"description": "synthetic subset for the tests", "conventions": {}, "variants": [
    {"id": "sup-05"}, {"id": "sup-37"}, {"id": "sup-14"}, {"id": "sup-01"}]}


class NegativeBlockTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.negatives = SCRATCH / "block-negatives.json"
        cls.supplement = SCRATCH / "block-supplement.json"
        cls.negatives.write_text(json.dumps(SYNTHETIC_BLOCK_NEGATIVES), encoding="utf-8")
        cls.supplement.write_text(json.dumps(SYNTHETIC_BLOCK_SUPPLEMENT), encoding="utf-8")
        cls.manifest = impl.NEGATIVES_ROOT / "MANIFEST.tsv"
        cls.out = impl.run_negative_blocks(cls.negatives, cls.supplement, cls.manifest, SCRATCH / "blocks-first.json")

    def rows(self, case):
        return [row for row in self.out["rows"] if row["case"] == case]

    def test_negative_blocks_twice_gives_identical_output(self) -> None:
        impl.run_negative_blocks(self.negatives, self.supplement, self.manifest, SCRATCH / "blocks-second.json")
        self.assertEqual((SCRATCH / "blocks-first.json").read_bytes(), (SCRATCH / "blocks-second.json").read_bytes())
        self.assertEqual(self.out["schema"], "rem-parity-second-implementation-negative-blocks/1")

    def test_in_place_mutations_match_the_manifest(self) -> None:
        for case, count in (("sidecar-logical-shard-count", 1), ("overflow-10.4-slot-product/a-64s", 2),
                            ("overflow-13.3-tail-location/a-H-seven", 5), ("sidecar-data-crc-count/isolated", 6)):
            with self.subTest(case=case):
                rows = self.rows(case)
                self.assertEqual(len(rows), count)
                self.assertTrue(all(row["result"] == "matched" for row in rows), rows)

    def test_a_rebuilt_image_lists_unchanged_blocks_only_on_the_manifest_side(self) -> None:
        rows = self.rows("sidecar-header-block-count/isolated")
        self.assertEqual(sum(row["result"] == "matched" for row in rows), 25)
        extra = [row for row in rows if row["result"] != "matched"]
        self.assertEqual([(r["tape_file"], r["block_within_file"]) for r in extra], [(5, 1), (7, 1)])
        self.assertTrue(all(r["result"] == "missing_from_mine" and r["unchanged_block_matches_pinned"] for r in extra))

    def test_sup14_differences_are_located_in_parity_derived_fields(self) -> None:
        mismatched = [row for row in self.rows("sidecar-real-data-shard-count/isolated-2") if row["result"] == "mismatched"]
        self.assertEqual(len(mismatched), 6)
        for row in mismatched:
            self.assertTrue(row["diagnostic_candidate"]["reproduces_pinned_digest"])
            self.assertIn(row["field"].split(": ")[1].split(" ")[0], ("canonical_metadata_hash", "payload_sha256"))

    def test_unpinned_and_unit_cases(self) -> None:
        # R2's manifest pins the nine replica blocks of overflow-3.2-lba (neg-44); my blocks match each of them.
        rows = self.rows("overflow-3.2-lba")
        self.assertEqual(len(rows), 9)
        self.assertTrue(all(row["result"] == "matched" for row in rows), rows)
        self.assertEqual(self.out["entries"]["neg-26"]["vector"], "none")
        self.assertEqual(self.out["entries"]["sup-01"]["vector"], "unit")


# ---------------------------------------------------------------------------
# mutations and selection: the terminal-index mutations and survivor sets.
# ---------------------------------------------------------------------------

MUTATIONS = pathlib.Path(os.environ.get("REM_PARITY_SECOND_IMPL_MUTATIONS", impl.BLIND_INPUTS / "mutations-blind.json"))
SELECTION = pathlib.Path(os.environ.get("REM_PARITY_SECOND_IMPL_SELECTION", impl.BLIND_INPUTS / "selection-blind.json"))


class MutationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.out = impl.run_mutations(MUTATIONS, SCRATCH / "mutations-first.json")

    def test_mutations_twice_gives_identical_output(self) -> None:
        impl.run_mutations(MUTATIONS, SCRATCH / "mutations-second.json")
        self.assertEqual((SCRATCH / "mutations-first.json").read_bytes(), (SCRATCH / "mutations-second.json").read_bytes())
        self.assertEqual(self.out["schema"], "rem-parity-second-implementation-mutations/1")

    def test_every_mutation_resolves_and_self_checks(self) -> None:
        for entry_id, entry in self.out["entries"].items():
            with self.subTest(entry=entry_id):
                self.assertTrue(entry["apply"]["resolved"], entry["apply"]["checks_failed"])
                self.assertTrue(entry["self_check"]["agrees"], entry["self_check"]["detail"])

    def test_every_decision_cites_the_text(self) -> None:
        text = " ".join(impl.SPEC_PATH.read_text(encoding="utf-8").split())
        for entry_id, entry in self.out["entries"].items():
            decision = entry["decision"]
            for citation in decision["component"]["rules"] + decision["tape"]["rules"]:
                self.assertIn(citation["quote"], text, entry_id)

    def test_a_stated_old_value_that_differs_is_reported_not_guessed(self) -> None:
        inputs, terminal = impl.profile_build("multi-256k")
        stream = b"".join(terminal.components[0])
        change = {"edits": [{"record": "header record 0", "field": "replica count 3", "offset": "0x03A",
                             "component_byte_offset": 58, "size": 2, "old": {"unsigned_little_endian": 9, "hex": "0900"},
                             "new": {"unsigned_little_endian": 2, "hex": "0200"}}]}
        res = impl.Resolution()
        impl.apply_byte_change(stream, change, {}, inputs["block_size"], "test", res, impl.KIND_REPLICA, "multi-256k")
        self.assertFalse(res.resolved)
        self.assertEqual(res.checks[0]["found"], "0300")

    def test_listed_repairs_are_recomputed_not_copied(self) -> None:
        inputs, terminal = impl.profile_build("multi-256k")
        stream = b"".join(terminal.components[0])
        wrong_crc = {"unsigned_little_endian": 0, "hex": "0000000000000000"}
        change = {"edits": [
            {"record": "header record 0", "field": "reserved zero", "offset": "0x300", "component_byte_offset": 768, "size": 1,
             "old": {"hex": "00"}, "new": {"hex": "01"}},
            {"record": "header record 0", "field": "CRC-64/XZ", "offset": "0x3F8", "component_byte_offset": 1016, "size": 8,
             "old": {"hex": stream[1016:1024].hex()}, "new": wrong_crc}]}
        repairs = {"recomputed": ["Header CRC-64/XZ at 0x3F8, over header bytes [0x000, 0x3F8)."]}
        res = impl.Resolution()
        out = impl.apply_byte_change(stream, change, repairs, inputs["block_size"], "test", res, impl.KIND_REPLICA, "multi-256k")
        self.assertFalse(res.resolved)
        self.assertEqual(out[1016:1024], impl.le64(impl.crc64_xz(out[:0x3F8])))


class SelectionTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.out = impl.run_selection(SELECTION, SCRATCH / "selection-first.json")

    def test_selection_twice_gives_identical_output(self) -> None:
        impl.run_selection(SELECTION, SCRATCH / "selection-second.json")
        self.assertEqual((SCRATCH / "selection-first.json").read_bytes(), (SCRATCH / "selection-second.json").read_bytes())
        self.assertEqual(self.out["schema"], "rem-parity-second-implementation-selection/1")

    def test_every_set_resolves_and_self_checks(self) -> None:
        for entry_id, entry in self.out["entries"].items():
            with self.subTest(entry=entry_id):
                self.assertTrue(entry["apply"]["resolved"], entry["apply"]["checks_failed"])
                self.assertTrue(entry["self_check"]["agrees"], entry["self_check"]["detail"])

    def test_a_second_edition_replica_conflicts_with_a_valid_replica_of_the_first(self) -> None:
        # 8.5: a fully valid replica that differs from another fully valid replica in any edition-common field is
        # TerminalIndexReplicaConflict; S4 differs from the profile's replica only in edition ID and sequence.
        statuses = {"S0": impl.parse_status("the profile's replica, unchanged"),
                    "S4": impl.parse_status("at this position, a second-edition replica: a replica that is locally "
                                            "eligible at this position (its planned layout, recorded positions, counts and "
                                            "every CRC and digest are valid for this tape) and differs from the profile's "
                                            "replica at this position only in its edition id and edition sequence")}
        self.assertEqual(statuses["S4"], {"class": "second-edition"})
        for row in ({"A": "S0", "B": "S0", "C": "S4"}, {"A": "S4", "B": "S0", "C": "S0"}, {"A": "S4", "B": "S0", "C": "S4"}):
            decision = impl.selection_decision(row, statuses)
            self.assertEqual((decision["outcome"], decision["acceptable_selections"], decision["degraded"]),
                             ("TerminalIndexReplicaConflict", [], None), row)
        # All three of one edition, whichever it is, agree.
        decision = impl.selection_decision({"A": "S4", "B": "S4", "C": "S4"}, statuses)
        self.assertEqual((decision["outcome"], decision["acceptable_selections"]), ("inventory", ["A", "B", "C"]))

    def test_a_second_edition_replica_differs_only_in_its_edition_fields(self) -> None:
        inputs, terminal = impl.profile_build("multi-256k")
        first = terminal.components[0]
        second = impl.second_edition_stream("multi-256k", "replica-a.bin")
        records = [second[i:i + inputs["block_size"]] for i in range(0, len(second), inputs["block_size"])]
        self.assertEqual(len(records), len(first))
        self.assertEqual(records[1], first[1])  # the payload records are the edition's, and unchanged
        self.assertNotEqual(records[0], first[0])
        frame = impl.parse_replica_frame(records[0], bytes.fromhex(inputs["tape_uuid"]), inputs["block_size"], 1)
        old = impl.parse_replica_frame(first[0], bytes.fromhex(inputs["tape_uuid"]), inputs["block_size"], 1)
        self.assertNotEqual(frame["edition_id"], old["edition_id"])
        self.assertEqual((frame["ordinal"], frame["tuples"]), (old["ordinal"], old["tuples"]))

    def test_agreeing_valid_replicas_are_all_acceptable(self) -> None:
        statuses = {"S0": impl.parse_status("the profile's replica, unchanged")}
        decision = impl.selection_decision({"A": "S0", "B": "S0", "C": "S0"}, statuses)
        self.assertEqual((decision["outcome"], decision["acceptable_selections"], decision["degraded"]),
                         ("inventory", ["A", "B", "C"], False))


# ---------------------------------------------------------------------------
# The revised text (F0): each test names the sentence it checks.
# ---------------------------------------------------------------------------


class RevisedTextTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.image = impl.build_image(impl.load_image_inputs("a4-minimal"), "a4-minimal")

    def tape(self, records=None, unreadable=()):
        return impl.DamagedTape(self.image.records() if records is None else records, set(unreadable))

    def hints(self):
        return {"tape_uuid": self.image.tape_uuid, "block_size": self.image.block_size, "scheme": self.image.scheme}

    def test_a_footer_supplies_the_layout_only_where_it_says_it_is(self) -> None:
        # 8.4 step 1: "... only when its recorded footer position equals the position at which it was read".
        layout, note = impl.discover_layout(self.tape(), self.image.tape_uuid, self.image.block_size)
        self.assertIsNotNone(layout)
        self.assertIn("replica ordinal 3", note)
        shifted, _ = impl.damaged_tape_for(self.image, {"removed_filemark_after_tape_file": 1})
        layout, note = impl.discover_layout(shifted, self.image.tape_uuid, self.image.block_size)
        self.assertIsNone(layout)
        self.assertEqual(note.count("records its position as"), 3)

    def test_a_layout_whose_planned_eod_precedes_the_tape_eod_is_not_used(self) -> None:
        # 8.4 step 1: "A layout whose planned EOD lies before the tape's EOD ... is not used".
        records = self.image.records()
        longer = self.tape(records + [records[2], None])
        layout, note = impl.discover_layout(longer, self.image.tape_uuid, self.image.block_size)
        self.assertIsNone(layout)
        self.assertIn("before the tape's EOD", note)

    def test_a_count_mismatch_leaves_the_file_to_item_7(self) -> None:
        # 12.3: "under items 4, 5 and 6, as under item 1, the file is not recognised, and item 7 applies."
        case = {"failed_data_addresses": [], "hints": None, "image": "a4-minimal",
                "removed_filemark_after_tape_file": 1, "unreadable_records": []}
        decision = impl.decide_case(case, self.image, {})
        walk = decision["walk"]
        self.assertEqual(walk["classes"]["1"], "Object")
        self.assertIn("differs from the measured 11 blocks", walk["failed_classifications"]["1"])
        self.assertEqual(walk["error"], "FilemarkMapDigestMismatch")
        self.assertEqual(walk["object_identity"], "unknown")
        self.assertEqual(decision["verifier"]["separations"], {"A-B": "invalid", "B-C": "invalid"})
        self.assertEqual(decision["undecided"], [])

    def test_the_first_record_with_supplied_values(self) -> None:
        # 8.4's tables: a wrong length is refused; a medium error leaves the bootstrap unreadable.
        self.assertEqual(impl.judge_supplied_bootstrap(self.tape(), self.hints())[0], "use")
        records = self.image.records()
        records[0] = records[0][:100]
        outcome, error, _, key, _, named = impl.judge_supplied_bootstrap(self.tape(records), self.hints())
        self.assertEqual((outcome, error, key, named), ("refused", "BootstrapParse", "first_record_length", "block size"))
        outcome, error, _, key, _, _ = impl.judge_supplied_bootstrap(self.tape(unreadable=[0]), self.hints())
        self.assertEqual((outcome, error, key), ("unreadable", None, "first_record_unreadable"))

    def test_a_boundary_read_in_step_3_is_resume_append(self) -> None:
        # 14 step 3: "A read that finds a filemark, EOD or a record shorter or longer than one block where the
        # committed prefix places a data block contradicts the commit record, and is `ResumeAppend`".
        case = copy.deepcopy(synthetic_resume_cases()["synthetic-accepted"])
        case["committed_prefix"][-1]["block_count"] += 1
        case["T"] += 1
        decision, appended = impl.resume_case(case, impl._image_cache("unfinalized-open"), {})
        self.assertIsNone(appended)
        self.assertEqual((decision["decision"]["result"], decision["decision"]["error"], decision["decision"]["refused_at"]),
                         ("refused", "ResumeAppend", "step 3"))
        self.assertIn("filemark where data is expected", decision["step3"]["failure"])
        self.assertEqual(decision["undecided"], [])

    def test_a_map_position_that_does_not_fit_invalidates_the_replicas(self) -> None:
        # 7.2: "Every record position and every trailing filemark position that the map describes ... MUST fit in u64".
        path = SCRATCH / "negatives-44.json"
        path.write_text(json.dumps({"description": "neg-44 alone", "conventions": {}, "cases": [
            {"id": "neg-44", "target": "replica map position arithmetic"}]}), encoding="utf-8")
        entry = impl.run_negatives(path, SCRATCH / "negatives-44-out.json")["entries"]["neg-44"]
        self.assertEqual((entry["decision"]["outcome"], entry["decision"]["error"]),
                         ("rejected", "TerminalIndexReplicaParse"))
        self.assertEqual(entry["implementation"][0]["selection"], "BotStructuralRecoveryRequired")
        self.assertTrue(entry["self_check"]["agrees"], entry["self_check"]["detail"])

    def sidecar_context(self, with_directory: bool):
        ws = impl.Workspace(image=impl._image_cache("a4-minimal"))
        res = impl.Resolution()
        impl.negatives_table()["neg-11"]["apply"](ws, res)  # the tail copy diverges, and both copies are valid
        self.assertTrue(res.resolved)
        tape = impl.DamagedTape(ws.records(), {ws.image.file_start_lba(2) + 6})  # the footer is unreadable
        boot = impl.parse_bootstrap(tape.read_data(0), ws.block_size)
        entries = ws.image.prefix_entries
        directory = impl.load_parity_map_directory(tape, entries, ws.tape_uuid, ws.block_size)[0] if with_directory else None
        ctx = impl.RecoveryContext(tape, ws.tape_uuid, ws.block_size, boot["scheme"], entries, len(entries),
                                   impl.derived_watermark(entries), directory, "replica")
        return ctx, next(e for e in entries if e.tape_file_number == 2)

    def test_without_an_entry_a_diverging_tail_leaves_the_epoch_unavailable(self) -> None:
        # 13.3: "A valid tail copy whose canonical metadata hash (Section 9.5) differs from the primary's leaves
        # nothing to decide between them, and the epoch is metadata-unavailable."
        ctx, sidecar = self.sidecar_context(with_directory=False)
        index, note = impl.acquire_index(ctx, sidecar, [])
        self.assertIsNone(index)
        self.assertIn("hash differs from the primary's", note)
        self.assertEqual(impl.recover_address(ctx, [1, 0])["error"], "SidecarMetadataUnavailable")

    def test_an_available_entry_decides_by_hash(self) -> None:
        # 13.3: "When a sidecar epoch directory entry is available (step 3), it decides as the footer would".
        ctx, sidecar = self.sidecar_context(with_directory=True)
        index, note = impl.acquire_index(ctx, sidecar, [])
        self.assertIsNotNone(index)
        self.assertIn("its hash equals the directory entry's", note)
        self.assertEqual(impl.recover_address(ctx, [1, 0])["result"], "recovered")


# ---------------------------------------------------------------------------
# F1: observations, record edits, strict fault maps and the E1 negative.
# ---------------------------------------------------------------------------


def boot_edit_case(image, edits, hints_list, length=None):
    """A fault map that edits the bootstrap record, as the e1 cases state their edits."""
    original = image.files[0].blocks[0]
    record = bytearray(original if length is None else (original[:length] + bytes(max(0, length - len(original)))))
    stated = []
    for offset, new in edits:
        stated.append({"offset": offset, "old_bytes": bytes(record[offset:offset + len(new)]).hex(),
                       "new_bytes": new.hex(), "reason": "test edit"})
        record[offset:offset + len(new)] = new
    construction = ("the original record's length" if length in (None, len(original)) else
                    f"the first {length} bytes of the original record" if length < len(original) else
                    f"the original record followed by {length - len(original)} zero bytes")
    return {"failed_data_addresses": [], "image": "a4-minimal", "removed_filemark_after_tape_file": None,
            "unreadable_records": [],
            "observations": [{"id": name, "hints": hints} for name, hints in hints_list],
            "record_edits": [{"construction": construction, "edits": stated, "lba": 0,
                              "length": len(record), "original_length": len(original), "record_index": 0,
                              "sha256": hashlib.sha256(bytes(record)).hexdigest(), "tape_file": 0}]}


SUPPLIED = {"block_size": 262144, "scheme": {"S": 2, "k": 2, "m": 2}, "tape_uuid": "the image's"}


class FaultMapTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.image = impl.build_image(impl.load_image_inputs("a4-minimal"), "a4-minimal")
        cls.dir = SCRATCH / "fault-maps"
        cls.dir.mkdir(parents=True, exist_ok=True)

    def write(self, name, case):
        path = self.dir / f"{name}.json"
        path.write_text(json.dumps(case), encoding="utf-8")
        return path

    def test_an_unknown_key_fails_the_run(self) -> None:
        base = dict(SYNTHETIC_CASES["synthetic-replica-headers"])
        for name, case in (("top", dict(base, surprise=1)),
                           ("nested", dict(base, unreadable_records=[dict(base["unreadable_records"][0], why="x")])),
                           ("hint", dict(base, hints={"block_size": 262144, "scheme": None, "tape_uuid": "the image's",
                                                      "extra": 1}))):
            with self.subTest(case=name):
                path = self.write(f"unknown-{name}", case)
                with self.assertRaises(impl.FaultMapError):
                    impl.run_decide([path], SCRATCH / "unknown.json")
                with contextlib.redirect_stderr(io.StringIO()) as stderr:
                    self.assertEqual(impl.main(["decide", str(path), "--out", str(SCRATCH / "unknown.json")]), 2)
                self.assertIn("unknown key", stderr.getvalue())

    def test_hints_and_observations_are_exclusive(self) -> None:
        case = dict(boot_edit_case(self.image, [], [("a", None)]), hints=None)
        with self.assertRaises(impl.FaultMapError):
            impl.validate_fault_map(case, "test")

    def test_a_stated_old_byte_or_digest_that_differs_is_refused(self) -> None:
        case = boot_edit_case(self.image, [(0, b"\x53")], [("a", SUPPLIED)])
        wrong_old = copy.deepcopy(case)
        wrong_old["record_edits"][0]["edits"][0]["old_bytes"] = "00"
        wrong_digest = copy.deepcopy(case)
        wrong_digest["record_edits"][0]["sha256"] = "00" * 32
        wrong_construction = copy.deepcopy(case)
        wrong_construction["record_edits"][0]["construction"] = "the first 40 bytes of the original record"
        for name, bad in (("old", wrong_old), ("digest", wrong_digest), ("construction", wrong_construction)):
            with self.subTest(case=name):
                with self.assertRaises(impl.FaultMapError):
                    impl.apply_record_edits(self.image, self.image.records(), bad)

    def test_each_observation_is_a_separate_decision(self) -> None:
        # The magic is missing: with supplied values the bootstrap is unreadable and discovery continues (8.4);
        # without them no candidate gives a usable bootstrap (NoBootstrapFound, Section 15).
        case = boot_edit_case(self.image, [(0, b"\x53")], [("supplied values", SUPPLIED), ("no supplied values", None)])
        out = impl.run_decide([self.write("magic", case)], SCRATCH / "observations.json")
        entry = out["cases"]["magic"]
        self.assertEqual(list(entry["observations"]), ["supplied values", "no supplied values"])
        supplied, unsupplied = entry["observations"]["supplied values"], entry["observations"]["no supplied values"]
        self.assertEqual((supplied["scanner"]["result"], supplied["scanner"]["acceptable_selections"]),
                         ("Inventory", ["A", "B", "C"]))
        self.assertEqual(supplied["verifier"]["outside_terminal_suffix"][0]["error"], "BootstrapParse")
        self.assertEqual(unsupplied["scanner"]["error"], "NoBootstrapFound")
        self.assertTrue(entry["record_edits"][0]["sha256_agrees"])

    def test_a_decodable_scheme_decides_even_in_a_non_canonical_payload(self) -> None:
        # 8.4: "treats the bootstrap as unreadable, unless a value that can still be decoded disagrees".
        block = self.image.files[0].blocks[0]
        length = impl.rd32(block, 0x2C)
        payload = impl.decode_deterministic_cbor(block[0x38:0x38 + length])
        def reordered(scheme_k):
            items = dict(payload)
            items[1] = {**items[1], 2: scheme_k}
            body = bytearray(b"\xa5")
            for key in (2, 1, 3, 4, 5):  # key 2 before key 1: not in deterministic order
                body += impl.encode_deterministic_cbor(key) + impl.encode_deterministic_cbor(items[key])
            return bytes(body)
        hints = {"tape_uuid": self.image.tape_uuid, "block_size": self.image.block_size, "scheme": self.image.scheme}
        for k, expected in ((2, ("unreadable", None, None)), (3, ("refused", "BootstrapParse", "scheme"))):
            with self.subTest(k=k):
                body = reordered(k)
                self.assertEqual(len(body), length)
                self.assertEqual(impl.decode_cbor_lenient(body)[1][2], k)
                record = bytearray(block)
                record[0x38:0x38 + length] = body
                record[0x38 + length:0x38 + length + 8] = impl.le64(impl.crc64_xz(body))
                outcome, error, _, _, _, named = impl.judge_supplied_bootstrap(
                    impl.DamagedTape([bytes(record)] + self.image.records()[1:], set()), hints)
                self.assertEqual((outcome, error, named), expected)

    def test_a_duplicated_key_has_no_decodable_value(self) -> None:
        value = impl.decode_cbor_lenient(bytes([0xa2, 0x01, 0x02, 0x01, 0x03]))
        self.assertIs(value[1], impl.AMBIGUOUS)


class NegativeE1Tests(unittest.TestCase):
    def test_a_no_parity_bootstrap_may_record_compression(self) -> None:
        # 16.3: "Both sentences concern a parity tape (Section 11.4): a no-parity bootstrap may record compression."
        path = impl.BLIND_INPUTS / "negatives-e1-blind.json"
        first, second = SCRATCH / "negative-e1-first.json", SCRATCH / "negative-e1-second.json"
        out = impl.run_negatives(path, first, impl.e1_negatives_table())
        impl.run_negatives(path, second, impl.e1_negatives_table())
        self.assertEqual(first.read_bytes(), second.read_bytes())
        entry = out["entries"]["e1-16"]
        self.assertTrue(entry["apply"]["resolved"])
        self.assertEqual((entry["decision"]["outcome"], entry["decision"]["error"]), ("accepted", None))
        self.assertTrue(entry["self_check"]["agrees"], entry["self_check"]["detail"])
        self.assertEqual(out["table_entries_without_a_case"], [])


# ---------------------------------------------------------------------------
# F-T1b: the tail rescue from the map entry, and full verification.
# ---------------------------------------------------------------------------


class MapEntryRescueTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.image = impl.build_image(impl.load_image_inputs("a4-minimal"), "a4-minimal")

    def context(self, unreadable, route="replica", entries=None):
        tape = impl.DamagedTape(self.image.records(), set(unreadable))
        entries = entries or self.image.prefix_entries
        return impl.RecoveryContext(tape, self.image.tape_uuid, self.image.block_size, self.image.scheme, entries,
                                    len(entries), impl.derived_watermark(entries), None, route)

    def sidecar_lbas(self):
        start = self.image.file_start_lba(2)
        return start, start + len(self.image.files[2].blocks) - 1

    def test_the_tail_copy_is_tried_from_the_map_entry(self) -> None:
        # 13.3: "a Recoverer MUST try the tail copy that the map entry locates ... H = (total − 1 − P) / 2".
        primary, footer = self.sidecar_lbas()
        ctx = self.context({primary, footer})
        sidecar = next(e for e in ctx.entries if e.tape_file_number == 2)
        index, note = impl.acquire_index(ctx, sidecar, [])
        self.assertIsNotNone(index)
        self.assertIn("rescue from the map entry, H = 1", note)
        self.assertEqual(impl.recover_address(ctx, [1, 0])["result"], "recovered")

    def test_a_walked_map_does_not_qualify(self) -> None:
        # 13.3: "A walked map does not qualify: it gains a validated scope only through a final ParityMap".
        primary, footer = self.sidecar_lbas()
        ctx = self.context({primary, footer}, route="walk")
        sidecar = next(e for e in ctx.entries if e.tape_file_number == 2)
        self.assertIsNone(impl.acquire_index(ctx, sidecar, [])[0])

    def test_a_validated_final_parity_map_excludes_the_rescue(self) -> None:
        # 13.3: "When a final ParityMap validates but the sidecar's entry fails a precondition of the
        # directory-assisted rescue, this rescue does not apply either".
        primary, footer = self.sidecar_lbas()
        ctx = self.context({primary, footer})
        directory, _ = impl.load_parity_map_directory(ctx.tape, ctx.entries, self.image.tape_uuid, self.image.block_size)
        self.assertIsNotNone(directory)
        directory = {key: dict(value, H=7) for key, value in directory.items()}  # the entry fails 2H + P + 1
        ctx = dataclasses.replace(ctx, directory=directory)
        sidecar = next(e for e in ctx.entries if e.tape_file_number == 2)
        index, note = impl.acquire_index(ctx, sidecar, [])
        self.assertIsNone(index)
        self.assertIn("a final ParityMap validates", note)

    def test_the_rescue_requires_an_even_remainder_and_an_unreadable_tail_fails_it(self) -> None:
        # 13.3: "This rescue requires that `total − 1 − P` is even"; "If this rescue fails, the epoch is
        # metadata-unavailable (step 4)."
        primary, footer = self.sidecar_lbas()
        ctx = self.context({primary, footer, primary + 5})
        sidecar = next(e for e in ctx.entries if e.tape_file_number == 2)
        index, note = impl.acquire_index(ctx, sidecar, [])
        self.assertIsNone(index)
        self.assertIn("tail copy unreadable", note)
        odd = [dataclasses.replace(e, block_count=8) if e.tape_file_number == 2 else e for e in self.image.prefix_entries]
        ctx = self.context({primary, footer}, entries=odd)
        index, note = impl.acquire_index(ctx, next(e for e in odd if e.tape_file_number == 2), [])
        self.assertIsNone(index)
        self.assertIn("is not even", note)

    def test_parity_map_and_sidecar_recovers_epoch_0_and_the_verifier_is_decided(self) -> None:
        # The case's fault map as the repository gives it, when it is present.
        path = impl.FIXTURE_ROOT / "tape-images" / "cases" / "parity-map-and-sidecar" / "fault-map.json"
        if not path.exists():
            self.skipTest("the repository's damage case is not present")
        out = impl.run_decide([path], SCRATCH / "pms" / "decisions.json")
        decision = out["cases"]["parity-map-and-sidecar"]
        epoch0 = next(a for a in decision["recoverer"]["addresses"] if a["epoch"] == 0)
        self.assertEqual((epoch0["result"], epoch0["error"]), ("recovered", None))
        self.assertEqual(decision["undecided"], [])
        data = [f for f in decision["verifier"]["outside_terminal_suffix"] if f["checks"] == "data"]
        self.assertTrue(data)
        # 2.2: "a data block's tape-file position, or a parity shard's epoch, stripe and parity index".
        for finding in data:
            self.assertIn(sorted(finding["address"]), (["block", "tape_file"], ["epoch", "parity_index", "stripe"]))


# ---------------------------------------------------------------------------
# F2: read requests, resume tape faults and strict resume inputs.
# ---------------------------------------------------------------------------


class ReadRequestTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.image = impl.build_image(impl.load_image_inputs("a4-minimal"), "a4-minimal")
        cls.dir = SCRATCH / "read-requests"
        cls.dir.mkdir(parents=True, exist_ok=True)

    def case(self, record=None, length=None):
        case = {"failed_data_addresses": [], "hints": None, "image": "a4-minimal", "read_data_addresses": [[1, 0]],
                "removed_filemark_after_tape_file": None, "unreadable_records": []}
        if length is not None:
            original = self.image.files[1].blocks[0]
            new = original[:length] + bytes(max(0, length - len(original)))
            case["record_edits"] = [{"construction": f"the first {length} bytes of the original record", "edits": [],
                                     "lba": 2, "length": length, "original_length": len(original), "record_index": 0,
                                     "sha256": hashlib.sha256(new).hexdigest(), "tape_file": 1}]
        return case

    def run_case(self, name, case):
        path = self.dir / f"{name}.json"
        path.write_text(json.dumps(case), encoding="utf-8")
        return impl.run_decide([path], self.dir / f"{name}-out.json")["cases"][name]

    def test_a_block_that_reads_returns_as_read(self) -> None:
        outcome = self.run_case("intact", self.case())["recoverer"]["addresses"][0]
        self.assertEqual((outcome["request"], outcome["result"], outcome["bytes_match"]), ("read", "read", True))

    def test_a_short_record_is_a_read_failure_and_is_recovered(self) -> None:
        # 13.4: "A record shorter or longer than one block is a read failure (Section 3.5)."
        outcome = self.run_case("short", self.case(length=1000))["recoverer"]["addresses"][0]
        self.assertEqual((outcome["result"], outcome["bytes_match"]), ("recovered", True))
        self.assertIn("1000-byte record", outcome["read"])


class ResumeInputTests(unittest.TestCase):
    def test_an_unknown_resume_key_fails_the_run(self) -> None:
        case = dict(synthetic_resume_cases()["synthetic-accepted"], surprise=True)
        path = SCRATCH / "resume-unknown.json"
        path.write_text(json.dumps(case), encoding="utf-8")
        with self.assertRaises(impl.FaultMapError):
            impl.run_resume([path], SCRATCH / "resume-unknown-out.json")

    def test_a_medium_error_in_step_3_is_tape_io(self) -> None:
        # 14 step 3: "a medium or transport failure is `TapeIo`".
        case = dict(synthetic_resume_cases()["synthetic-accepted"],
                    tape_faults={"record_edits": [], "unreadable_records": [{"lba": 15, "record_index": 0, "tape_file": 3}]})
        path = SCRATCH / "resume-medium.json"
        path.write_text(json.dumps(case), encoding="utf-8")
        decision = impl.run_resume([path], SCRATCH / "resume-medium-out.json")["cases"]["resume-medium"]["decision"]
        self.assertEqual((decision["result"], decision["error"], decision["refused_at"]), ("refused", "TapeIo", "step 3"))


# ---------------------------------------------------------------------------
# F3: new fault keys, artifacts after the terminal suffix, the full verification.
# ---------------------------------------------------------------------------


class InsertionAndAppendTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.image = impl.build_image(impl.load_image_inputs("a4-minimal"), "a4-minimal")
        cls.dir = SCRATCH / "f3"
        cls.dir.mkdir(parents=True, exist_ok=True)

    def base(self, **extra):
        return dict({"failed_data_addresses": [], "hints": None, "image": "a4-minimal",
                     "removed_filemark_after_tape_file": None, "unreadable_records": []}, **extra)

    def foreign(self, length=262144):
        data = b"X" + bytes(length - 1)
        return {"fill": impl.FOREIGN_FILL, "first_byte": "58", "length": length,
                "sha256": hashlib.sha256(data).hexdigest(), "source": "foreign"}

    def run_case(self, name, case):
        path = self.dir / f"{name}.json"
        path.write_text(json.dumps(case), encoding="utf-8")
        return impl.run_decide([path], self.dir / f"{name}-out.json")["cases"][name]

    def test_every_new_key_is_checked_at_every_level(self) -> None:
        good_insertion = {"after_record_index": 2, "fill": "00", "length": 1, "tape_file": 5,
                          "sha256": hashlib.sha256(b"\x00").hexdigest()}
        impl.validate_fault_map(self.base(record_insertions=[good_insertion]), "test")
        bad = [self.base(record_insertions=[dict(good_insertion, surprise=1)]),
               self.base(appended_files=[{"records": [self.foreign()], "trailing_filemark": True, "surprise": 1}]),
               self.base(appended_files=[{"records": [dict(self.foreign(), surprise=1)], "trailing_filemark": True}]),
               self.base(appended_files=[{"records": [dict(self.foreign(), source="unheard-of")], "trailing_filemark": True}]),
               self.base(appended_files=[{"records": [self.foreign()]}])]
        for case in bad:
            with self.assertRaises(impl.FaultMapError):
                impl.validate_fault_map(case, "test")

    def test_an_insertion_moves_every_later_record_and_its_digest_is_checked(self) -> None:
        insertion = {"after_record_index": 2, "fill": "00", "length": 1, "tape_file": 5,
                     "sha256": hashlib.sha256(b"\x00").hexdigest()}
        tape, _ = impl.damaged_tape_for(self.image, self.base(record_insertions=[insertion]))
        start = self.image.file_start_lba(5)
        self.assertEqual(tape.read(start + 3), b"\x00")
        self.assertTrue(tape.is_filemark(start + 4))
        self.assertEqual(tape.eod(), len(self.image.records()) + 1)
        with self.assertRaises(impl.FaultMapError):
            impl.damaged_tape_for(self.image, self.base(record_insertions=[dict(insertion, sha256="00" * 32)]))

    def test_an_unreadable_record_keeps_its_image_lba_when_records_are_inserted(self) -> None:
        insertion = {"after_record_index": 0, "fill": "00", "length": 1, "tape_file": 1,
                     "sha256": hashlib.sha256(b"\x00").hexdigest()}
        case = self.base(record_insertions=[insertion],
                         unreadable_records=[{"filemark": False, "lba": 4, "record_index": 2, "tape_file": 1}])
        tape, _ = impl.damaged_tape_for(self.image, case)
        with self.assertRaises(impl.MediumError):
            tape.read(5)  # image LBA 4 is now at position 5
        self.assertEqual(tape.read(3), b"\x00")  # the inserted record

    def test_appended_files_follow_the_tape_and_a_foreign_record_is_checked_by_its_digest(self) -> None:
        case = self.base(appended_files=[{"records": [self.foreign(), self.foreign()], "trailing_filemark": False}])
        tape, _ = impl.damaged_tape_for(self.image, case)
        end = len(self.image.records())
        self.assertEqual(tape.eod(), end + 2)
        self.assertEqual(tape.read(end)[:2], b"X\x00")
        bad = self.foreign()
        bad["sha256"] = "00" * 32
        with self.assertRaises(impl.FaultMapError):
            impl.damaged_tape_for(self.image, self.base(appended_files=[{"records": [bad], "trailing_filemark": True}]))

    def test_a_copy_of_a_record_is_my_own_build_of_it(self) -> None:
        record = self.image.files[8].blocks[2]
        copy_of = {"length": len(record), "record_index": 2, "sha256": hashlib.sha256(record).hexdigest(),
                   "source": "copy_of", "tape_file": 8}
        tape, _ = impl.damaged_tape_for(self.image, self.base(appended_files=[{"records": [copy_of], "trailing_filemark": True}]))
        self.assertEqual(tape.read(len(self.image.records())), record)

    def test_spacing_back_from_eod_crosses_records_to_the_nearest_filemark(self) -> None:
        # 8.4 step 1: a tape whose last file lacks its filemark still spaces back over one filemark,
        # crossing the records after it, and reads the record before it.
        case = self.base(appended_files=[{"records": [self.foreign(), self.foreign()], "trailing_filemark": False}])
        tape, _ = impl.damaged_tape_for(self.image, case)
        layout, note = impl.discover_layout(tape, self.image.tape_uuid, self.image.block_size)
        self.assertIsNone(layout)
        self.assertIn("plans EOD 39, before the tape's EOD 41", note)

    def test_an_object_after_the_exact_terminal_suffix_is_not_admitted_as_an_object(self) -> None:
        # 12.6: "A structural artifact after the exact terminal suffix is nonconformant and MUST NOT be admitted as an Object."
        case = self.base(appended_files=[{"records": [self.foreign(), self.foreign()], "trailing_filemark": True}])
        decision = self.run_case("artifact", case)
        self.assertEqual(decision["scanner"]["result"], "BotStructuralRecoveryRequired")
        self.assertEqual(decision["walk"]["classes"]["9"], impl.ARTIFACT_CLASS)
        self.assertEqual(decision["walk"]["classes"]["1"], "Object")
        full = decision["verifier-full"]
        self.assertIs(full["terminal_suffix"]["complete"], False)
        self.assertTrue(any("after the exact terminal suffix" in f["finding"] for f in full["other_findings"]))

    def test_a_missing_trailing_filemark_is_structural_damage(self) -> None:
        # 12.2: "a zero-block file or a missing trailing filemark is structural damage".
        case = self.base(appended_files=[{"records": [self.foreign()], "trailing_filemark": False}])
        decision = self.run_case("torn", case)
        self.assertEqual(decision["walk"]["structural_damage"], ["tape file 9: missing trailing filemark before EOD"])

    def test_an_object_after_a_broken_suffix_is_an_object_candidate(self) -> None:
        # 12.3 item 7 applies when the five terminal files are not exact.
        case = self.base(appended_files=[{"records": [self.foreign()], "trailing_filemark": True}],
                         removed_filemark_after_tape_file=5)
        decision = self.run_case("broken", case)
        self.assertEqual(decision["walk"]["classes"]["8"], "Object")


    def second_edition(self):
        """A second-edition replica built as e3-07 states it: base record 0/1/2 of tape file 4, fields replaced."""
        base = self.image.files[4].blocks
        edition_id = bytes([0x55] * 16)
        layout = [(4, 1, 9, 38, 3), (5, 1, 10, 42, 3), (4, 2, 11, 46, 3), (5, 2, 12, 50, 3), (4, 3, 13, 54, 3)]
        tuples = b"".join(impl.component_tuple(*t) for t in layout)
        header = bytearray(base[0])
        footer = bytearray(base[2])
        for record in (header, footer):
            record[0x20:0x30] = edition_id
            record[0x30:0x38] = impl.le64(8)
            record[0x88:0x90] = impl.le64(9)
            record[0x90:0x98] = impl.le64(38)
            record[0xA0:0xA8] = impl.le64(58)
            record[0x148:0x1E8] = tuples
        fields = lambda record, names: {n: {"hex": bytes(record[o:o + l]).hex(), "length": l, "offset": o}
                                        for n, (o, l) in names.items()}
        names = {"edition_id": (0x20, 16), "edition_sequence": (0x30, 8), "planned_tape_file_number": (0x88, 8),
                 "planned_start_lba": (0x90, 8), "expected_eod_lba": (0xA0, 8), "planned_layout_components": (0x148, 160)}
        records = []
        for index, (role, record) in enumerate((("header", header), ("payload", bytearray(base[1])), ("footer", footer))):
            records.append({"base": {"record_index": index, "tape_file": 4},
                            "fields": fields(record, names) if role != "payload" else {},
                            "length": 262144, "planned_start_lba": 38, "planned_tape_file_number": 9,
                            "record_index": index, "role": role, "sha256": hashlib.sha256(bytes(record)).hexdigest(),
                            "source": "second_edition_replica"})
        replica = {"base": {"meaning": "replica A", "tape_file": 4}, "base_edition_sequence": 7,
                   "edition_id": edition_id.hex(), "edition_sequence": 8, "expected_eod_lba": 58,
                   "footer_record_index": 2, "header_record_index": 0, "payload_record_index": 1,
                   "planned_layout": [{"filemark_count": 1, "kind": k, "kind_name": "x", "ordinal": o,
                                       "planned_start_lba": st, "planned_tape_file_number": tf, "record_count": c}
                                      for k, o, tf, st, c in layout],
                   "planned_start_lba": 38, "planned_tape_file_number": 9, "replica": "A", "replica_ordinal": 1}
        return {"records": records, "replica": replica, "trailing_filemark": True}

    def test_a_second_edition_replica_is_built_from_its_fields_and_checked(self) -> None:
        # The frame CRC is not among these fields, so the digests here are of records built with stale CRCs;
        # the stated SHA-256 is of whatever the fields yield, and a wrong digest is refused.
        appended = self.second_edition()
        case = self.base(appended_files=[appended], removed_filemark_after_tape_file=8)
        # the test's records carry the base CRC, so the reported comparison says so and the run still proceeds
        tape, notes = impl.damaged_tape_for(self.image, case)
        self.assertTrue(any("header frame CRC differs from mine" in n for n in notes))
        self.assertEqual(tape.underivable, set())
        bad = copy.deepcopy(appended)
        bad["records"][0]["sha256"] = "00" * 32
        with self.assertRaises(impl.FaultMapError):
            impl.damaged_tape_for(self.image, self.base(appended_files=[bad], removed_filemark_after_tape_file=8))
        wrong = copy.deepcopy(appended)
        wrong["replica"]["edition_sequence"] = 9
        with self.assertRaises(impl.FaultMapError):
            impl.damaged_tape_for(self.image, self.base(appended_files=[wrong], removed_filemark_after_tape_file=8))

    def test_a_second_edition_replica_with_stale_footer_fields_supplies_no_layout(self) -> None:
        # 8.4: a footer supplies a layout only "when its recorded footer position equals the position at which it was read".
        case = self.base(appended_files=[self.second_edition()], removed_filemark_after_tape_file=8)
        decision = self.run_case("second-edition", case)
        self.assertEqual(decision["scanner"]["result"], "BotStructuralRecoveryRequired")

    def test_an_unreadable_underivable_position_is_not_read(self) -> None:
        tape = impl.DamagedTape([b"x", None], set(), {0})
        with self.assertRaises(impl.Underivable):
            tape.read(0)


class FullVerificationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.image = impl.build_image(impl.load_image_inputs("a4-minimal"), "a4-minimal")
        cls.dir = SCRATCH / "f3-verifier"
        cls.dir.mkdir(parents=True, exist_ok=True)

    def run_case(self, name, unreadable, failed=()):
        case = {"failed_data_addresses": list(failed), "hints": None, "image": "a4-minimal",
                "removed_filemark_after_tape_file": None,
                "unreadable_records": [{"filemark": False, "lba": lba, "record_index": lba - self.image.file_start_lba(tf),
                                        "tape_file": tf} for tf, lba in unreadable]}
        path = self.dir / f"{name}.json"
        path.write_text(json.dumps(case), encoding="utf-8")
        return impl.run_decide([path], self.dir / f"{name}-out.json")["cases"][name]

    def test_a_healthy_tape_is_complete_and_every_block_and_shard_is_read(self) -> None:
        full = self.run_case("healthy", [])["verifier-full"]
        self.assertEqual((full["data_blocks_failed"], full["parity_shards_failed"], full["other_findings"]), ([], [], []))
        self.assertEqual(full["terminal_suffix"]["complete"], True)
        self.assertEqual((full["coverage"]["data_blocks_read"], full["coverage"]["parity_shards_read"]), (4, 4))

    def test_a_failed_block_and_a_failed_shard_are_reported_by_address(self) -> None:
        # 2.2: "a data block's tape-file position, or a parity shard's epoch, stripe and parity index".
        sidecar = self.image.file_start_lba(2)
        full = self.run_case("addresses", [(1, self.image.file_start_lba(1) + 2), (2, sidecar + 1 + 3)])["verifier-full"]
        self.assertEqual([f["address"] for f in full["data_blocks_failed"]], [{"tape_file": 1, "block": 2}])
        shards = [f["address"] for f in full["parity_shards_failed"]]
        self.assertEqual(len(shards), 1)
        self.assertEqual(set(shards[0]), {"epoch", "stripe", "parity_index"})
        self.assertEqual(shards[0]["epoch"], 0)

    def test_a_shard_is_read_and_reported_even_when_the_epochs_index_is_unavailable(self) -> None:
        # 9.1 fixes total = 2H + P + 1 with P = S x m, so the parity region is located from the map entry; 13.3 step 4
        # leaves the epoch metadata-unavailable, so the shard can be read but not checked against a CRC.
        start = self.image.file_start_lba(2)
        count = len(self.image.files[2].blocks)
        header_blocks = [(2, start + 0), (2, start + count - 2), (2, start + count - 1)]
        full = self.run_case("no-index", header_blocks + [(2, start + 2)])["verifier-full"]
        self.assertEqual(full["coverage"]["epochs_without_index"], [0])
        self.assertEqual(full["coverage"]["parity_shards_read_but_not_checkable"], 4)
        self.assertEqual([f["address"]["epoch"] for f in full["parity_shards_failed"]], [0])
        self.assertTrue(any(f["error"] == "SidecarMetadataUnavailable" for f in full["other_findings"]))

    def test_an_invalid_separation_extent_keeps_the_suffix_from_complete(self) -> None:
        # 10.6: "A Verifier that finds a separation extent invalid MUST report it and MUST NOT report the terminal suffix as complete."
        full = self.run_case("separation", [(5, self.image.file_start_lba(5) + 1)])["verifier-full"]
        self.assertIs(full["terminal_suffix"]["complete"], False)
        self.assertTrue(any(f["component"] == "separation extent A-B" for f in full["other_findings"]))

    def test_every_decision_carries_a_full_verification_observation(self) -> None:
        case = SYNTHETIC_CASES["synthetic-replica-headers"]
        path = self.dir / "synthetic.json"
        path.write_text(json.dumps(case), encoding="utf-8")
        entry = impl.run_decide([path], self.dir / "synthetic-out.json")["cases"]["synthetic"]
        self.assertEqual(set(entry["verifier-full"]), {"data_blocks_failed", "parity_shards_failed", "coverage",
                                                       "other_findings", "terminal_suffix", "citations"})


class ParityMapConstructionTests(unittest.TestCase):
    def test_a_reencoded_parity_map_keeps_its_length(self) -> None:
        self.assertTrue(impl._construction_matches(impl.PARITY_MAP_RECONSTRUCTION, 262144, 262144))
        self.assertFalse(impl._construction_matches(impl.PARITY_MAP_RECONSTRUCTION, 1000, 262144))
        self.assertFalse(impl._construction_matches("re-encoded some other way", 262144, 262144))

    def test_a_recomputed_parity_map_hash_and_crc_are_compared_with_mine(self) -> None:
        image = impl.build_image(impl.load_image_inputs("a4-minimal"), "a4-minimal")
        record = image.files[3].blocks[0]
        self.assertIn("agrees with mine", impl._recomputed_check(record, 0xC0, record[0xC0:0xC8], image))
        broken = bytearray(record)
        broken[0x38] ^= 1
        self.assertIn("differs from mine", impl._recomputed_check(bytes(broken), 0x38, bytes(broken[0x38:0x58]), image))


if __name__ == "__main__":
    unittest.main()
