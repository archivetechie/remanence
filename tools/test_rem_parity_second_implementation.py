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
  for the resume determinism test (default: synthetic cases written by the test).
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


if __name__ == "__main__":
    unittest.main()
