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

import copy
import dataclasses
import hashlib
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
        self.assertTrue(all(row["result"] == "missing_from_manifest" for row in self.rows("overflow-3.2-lba")))
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

    def test_a_foreign_replica_at_c_supplies_no_layout(self) -> None:
        # Section 8.4 step 1: a footer supplies the layout only where it says
        # it is, so the foreign footer at C is passed over and B's is used.
        statuses = {"S0": impl.parse_status("the profile's replica, unchanged"),
                    "S4": impl.parse_status("at this position, the replica of the same position taken from the minimal "
                                            "profile at the same block size, unchanged")}
        decision = impl.selection_decision({"A": "S0", "B": "S0", "C": "S4"}, statuses)
        self.assertEqual((decision["outcome"], decision["acceptable_selections"], decision["degraded"]),
                         ("inventory", ["A", "B"], True))
        self.assertFalse(decision.get("readings"))
        decided = impl.selection_decision({"A": "S4", "B": "S0", "C": "S0"}, statuses)
        self.assertEqual((decided["outcome"], decided["acceptable_selections"], decided["degraded"]), ("inventory", ["B", "C"], True))

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
        outcome, error, _, key, _ = impl.judge_supplied_bootstrap(self.tape(records), self.hints())
        self.assertEqual((outcome, error, key), ("refused", "BootstrapParse", "first_record_length"))
        outcome, error, _, key, _ = impl.judge_supplied_bootstrap(self.tape(unreadable=[0]), self.hints())
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


if __name__ == "__main__":
    unittest.main()
