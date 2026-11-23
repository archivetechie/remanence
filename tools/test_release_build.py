#!/usr/bin/env python3
"""Check Remanence's release build funnel and workflow comparison wiring."""

import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "tools/release_build.sh"
WORKFLOW = ROOT / ".github/workflows/release.yml"
BINS = ("rem", "rem-debug", "rem-daemon", "rem-recover")


class ReleaseWorkflowTests(unittest.TestCase):
    def test_runner_versions(self):
        runners = re.findall(r"^\s+runs-on:\s*(.*?)\s*$", WORKFLOW.read_text(), re.M)
        self.assertEqual(runners, ["ubuntu-24.04"] * 5)

    def test_build_funnel_jobs(self):
        workflow = WORKFLOW.read_text()
        self.assertNotIn("cargo build", workflow)
        jobs = []
        in_jobs = False
        job = None
        run_indent = None
        for line in workflow.splitlines():
            if line == "jobs:":
                in_jobs = True
                continue
            if not in_jobs:
                continue
            if line and not line[0].isspace():
                in_jobs = False
                continue
            key = re.fullmatch(r"  ([\w-]+):", line)
            if key:
                job = key.group(1)
                run_indent = None
            if line.strip() and run_indent is not None:
                if len(line) - len(line.lstrip()) <= run_indent:
                    run_indent = None
            run = re.match(r"^(\s+)(?:- )?run:\s*(.*)$", line)
            if run:
                run_indent = len(run.group(1))
                command = run.group(2)
            elif run_indent is not None:
                command = line.strip()
            else:
                continue
            if command.strip() == "tools/release_build.sh":
                jobs.append(job)
        self.assertEqual(jobs, ["build", "rebuild"])

    def test_publish_needs_rebuild(self):
        publish = re.search(r"^  publish:\n(.*?)(?=^  [\w-]+:|\Z)", WORKFLOW.read_text(), re.M | re.S)
        self.assertIsNotNone(publish)
        needs = re.search(r"^    needs:\s*\[([^]]+)\]", publish.group(1), re.M)
        self.assertIsNotNone(needs)
        self.assertIn("rebuild", [job.strip() for job in needs.group(1).split(",")])


class ReleaseBuildTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="release build ")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        subprocess.run(["git", "init", "--quiet", str(self.root)], check=True, capture_output=True)
        (self.root / "Cargo.lock").touch()
        self.script = self.root / "release_build.sh"
        shutil.copy2(SCRIPT, self.script)
        self.stubs = self.root / "stubs"
        self.stubs.mkdir()
        self.cargo_home = self.root / "ch"
        self.cargo_home.mkdir()
        cargo_link = self.root / "ch-link"
        cargo_link.symlink_to(self.cargo_home, target_is_directory=True)
        self.record = self.root / "cargo-record.json"
        self.env = os.environ.copy()
        for name in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "REMANENCE_SOFTWARE_BUILD", "TARGET"):
            self.env.pop(name, None)
        self.env.update({
            "PATH": str(self.stubs) + os.pathsep + self.env["PATH"],
            "CARGO_HOME": str(cargo_link),
            "STUB_SYSROOT": str(self.root / "sys"),
            "STUB_RECORD": str(self.record),
        })
        self.stub("rustc", """#!/bin/sh
case "$*" in
  '--print sysroot') printf '%s\\n' "$STUB_SYSROOT" ;;
  '-vV') printf '%s\\n' 'rustc 1.95.0' 'commit-hash: 0123abcd' 'release: 1.95.0' ;;
  *) exit 1 ;;
esac
""")
        self.stub("cargo", """#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys
args = sys.argv[1:]
Path(os.environ['STUB_RECORD']).write_text(json.dumps({
    'argv': args,
    'CARGO_HOME': os.environ.get('CARGO_HOME'),
    'RUSTFLAGS': os.environ.get('RUSTFLAGS'),
    'CARGO_ENCODED_RUSTFLAGS': os.environ.get('CARGO_ENCODED_RUSTFLAGS'),
}))
release = Path('target') / args[args.index('--target') + 1] / 'release'
release.mkdir(parents=True)
for binary in ('rem', 'rem-debug', 'rem-daemon', 'rem-recover'):
    (release / binary).write_bytes(b'stub binary')
""")
        self.stub("file", "#!/bin/sh\nprintf '%s: static-pie linked\\n' \"$1\"\n")
        for tool in ("cc", "ld"):
            self.stub(tool, f"#!/bin/sh\nprintf '%s\\n' '{tool} stub version' 'second line'\n")

    def stub(self, name, content):
        path = self.stubs / name
        path.write_text(content)
        path.chmod(0o755)

    def run_build(self, cwd=None):
        return subprocess.run([str(self.script)], cwd=cwd or self.root, env=self.env,
                              text=True, capture_output=True)

    def test_build_arguments_and_remaps(self):
        result = self.run_build()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        record = json.loads(self.record.read_text())
        self.assertEqual(record["CARGO_ENCODED_RUSTFLAGS"].split("\x1f"), [
            f"--remap-path-prefix={self.cargo_home}=/cargo",
            f"--remap-path-prefix={self.root}=/build",
            f"--remap-path-prefix={self.root}/sys/lib/rustlib/src/rust=/rustc/0123abcd",
        ])
        self.assertEqual(record["CARGO_HOME"], str(self.cargo_home))
        self.assertIsNone(record["RUSTFLAGS"])
        self.assertEqual(record["argv"], [
            "build", "--release", "--locked", "--target", "x86_64-unknown-linux-musl",
            "--target-dir", "target", "-p", "remanence-cli", "-p", "remanence-daemon",
            "-p", "rem-recover", "--bin", "rem", "--bin", "rem-debug",
            "--bin", "rem-daemon", "--bin", "rem-recover",
        ])
        for binary in BINS:
            self.assertRegex(result.stdout, rf"(?m)^[0-9a-f]{{64}}  target/x86_64-unknown-linux-musl/release/{binary}$")
        self.assertIn("release: 1.95.0", result.stdout)
        self.assertIn("cc stub version", result.stdout)
        self.assertIn("ld stub version", result.stdout)
        self.assertNotIn("second line", result.stdout)

    def test_relative_cargo_home_ignores_cdpath(self):
        with tempfile.TemporaryDirectory(prefix="cdpath ") as competing:
            (Path(competing) / "ch").mkdir()
            self.env["CARGO_HOME"] = "ch"
            self.env["CDPATH"] = competing
            result = self.run_build()
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            record = json.loads(self.record.read_text())
            self.assertEqual(record["CARGO_HOME"], str(self.cargo_home))

    def assert_override_refused(self, variable):
        self.env[variable] = "override"
        result = self.run_build()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(variable, result.stderr)
        self.assertFalse(self.record.exists())

    def test_refuse_rustflags(self):
        self.assert_override_refused("RUSTFLAGS")

    def test_refuse_encoded_rustflags(self):
        self.assert_override_refused("CARGO_ENCODED_RUSTFLAGS")

    def test_refuse_software_build(self):
        self.assert_override_refused("REMANENCE_SOFTWARE_BUILD")

    def test_refuse_subdirectory_with_lockfile(self):
        subdir = self.root / "subdirectory"
        subdir.mkdir()
        (subdir / "Cargo.lock").touch()
        result = self.run_build(cwd=subdir)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("repository root", result.stderr)
        self.assertFalse(self.record.exists())

    def test_target_override(self):
        self.env["TARGET"] = "another-target"
        result = self.run_build()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        argv = json.loads(self.record.read_text())["argv"]
        self.assertEqual(argv[argv.index("--target") + 1], "another-target")


if __name__ == "__main__":
    unittest.main()
