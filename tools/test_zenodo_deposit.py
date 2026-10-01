"""Offline safety tests for the irreversible, six-record Zenodo deposit.

The example JSON names invented ids and tiny inputs created in a temporary
repository. Only git's read-only answers are stubbed: no branch, index, or commit
is changed. The version checker is an actual subprocess (a small test double in
the temporary repo); the real repository checker is a separate CI gate. All API
traffic uses FakeZenodo in memory, never a server or socket.
"""

from contextlib import redirect_stderr, redirect_stdout
import copy
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch
import urllib.parse
import uuid

from tools import zenodo_deposit as z

EXAMPLE = Path(__file__).parent / "testdata/zenodo_records_example.json"
TOKEN = "test-secret-do-not-leak"


def response(body=None, status=200):
    return z.Response(status, {}, b"" if body is None else json.dumps(body).encode())


class FakeZenodo(z.Transport):
    """Deposit and records API shapes observed on Zenodo on 2026-09-30."""
    def __init__(self, config):
        self.base = config["api_base"]
        self.records = {}
        self.blobs = {}
        self.calls = []
        self.hook = None
        self.after_publish = None
        self.next_file = 1
        for r in config["records"].values():
            ident = str(r["draft_id"])
            self.records[ident] = {
                "id": int(ident), "state": "unsubmitted", "submitted": False,
                "record_id": int(ident), "owner": 1, "title": "Unstaged",
                "created": "2026-09-30T00:00:00Z", "modified": "2026-09-30T00:00:00Z",
                "conceptrecid": r["concept_doi"].rsplit(".", 1)[1],
                "metadata": {"title": "Unstaged", "prereserve_doi": {"doi": r["version_doi"], "recid": int(ident)}},
                "files": [], "links": {
                    "bucket": f"{self.base}/files/{ident}",
                    "self": f"{self.base}/deposit/depositions/{ident}",
                    "files": f"{self.base}/deposit/depositions/{ident}/files",
                    "latest_draft": f"{self.base}/deposit/depositions/{ident}",
                    **{action: f"{self.base}/deposit/depositions/{ident}/actions/{action}"
                       for action in ("publish", "discard", "newversion")},
                },
            }

    @property
    def posts(self):
        return [url for method, url, body, headers in self.calls if method == "POST"]

    @property
    def writes(self):
        return [c for c in self.calls if c[0] != "GET"]

    def put_file(self, ident, name, data):
        r = self.records[str(ident)]
        file_id = str(uuid.UUID(int=self.next_file))
        self.next_file += 1
        name_url = urllib.parse.quote(name, safe="")
        url = f"{self.base}/records/{ident}/draft/files/{name_url}/content"
        f = {"id": file_id, "filename": name, "filesize": len(data),
             "checksum": hashlib.md5(data).hexdigest(), "links": {
                 "download": url, "self": f"{self.base}/deposit/depositions/{ident}/files/{file_id}"}}
        r["files"] = [old for old in r["files"] if old["filename"] != name] + [f]
        self.blobs[self.content_url(ident, name)] = data

    def content_url(self, ident, name):
        return f"{self.base}/records/{ident}/files/{urllib.parse.quote(name, safe='')}/content"

    def request(self, method, url, *, json=None, data=None, headers=None):
        self.calls.append((method, url, copy.deepcopy(json), copy.deepcopy(headers)))
        if z.url_origin(url) != z.url_origin(self.base):
            assert "Authorization" not in (headers or {}), "Token leaked to storage host"
            if self.hook:
                result = self.hook(method, url, json)
                if result is not None:
                    return result
            return response({"error": "unknown storage host"}, 404)
        if (headers or {}).get("Authorization") != f"Bearer {TOKEN}":
            return response({"error": "unauthorized"}, 401)
        if self.hook:
            result = self.hook(method, url, json)
            if result is not None:
                return result
        parsed = urllib.parse.urlsplit(url)
        parts = parsed.path.split("/")[2:]
        if method == "GET" and len(parts) >= 2 and parts[0] == "records":
            r = self.records.get(parts[1])
            if r is None or r["state"] != "done" or "draft" in parts:
                return response({"error": "not published content"}, 404)
            if len(parts) == 2:
                return response({
                    **{k: r[k] for k in ("id", "doi", "conceptdoi", "conceptrecid", "metadata")},
                    "files": [{"key": f["filename"], "size": f["filesize"],
                               "checksum": "md5:" + f["checksum"].removeprefix("md5:"),
                               "links": {"self": self.content_url(r["id"], f["filename"])}}
                              for f in r["files"]],
                })
            if len(parts) == 5 and parts[2] == "files" and parts[4] == "content" and url in self.blobs:
                data = self.blobs[url]
                if headers.get("Range") == "bytes=0-15":
                    return z.Response(206, {"Content-Range": f"bytes 0-15/{len(data)}"}, data[:16])
                return z.Response(200, {}, data)
            return response({"error": "missing content"}, 404)
        if parts == ["deposit", "depositions"] and method == "GET":
            query = urllib.parse.parse_qs(parsed.query)
            assert query["sort"] == ["mostrecent"]
            assert int(query["size"][0]) > 0
            page = int(query["page"][0])
            # A small page cap forces the client to paginate until EMPTY.
            rows = sorted(self.records.values(), key=lambda r: r["id"], reverse=True)
            return response(rows[(page - 1) * 2:page * 2])
        if len(parts) == 3 and parts[0] == "files" and method == "PUT":
            ident, name = parts[1], urllib.parse.unquote(parts[2])
            r = self.records[ident]
            if r["state"] != "unsubmitted":
                return response({"error": "immutable"}, 403)
            self.put_file(ident, name, data)
            return response(r["files"][-1], 201)
        if len(parts) >= 3 and parts[:2] == ["deposit", "depositions"]:
            ident = parts[2]
            if ident not in self.records:
                return response({"error": "missing"}, 404)
            r = self.records[ident]
            if len(parts) == 3 and method == "GET":
                return response(r)
            if r["state"] != "unsubmitted":
                return response({"error": "immutable"}, 403)
            if len(parts) == 3 and method == "PUT":
                assert set(json) == {"metadata"}
                assert "prereserve_doi" not in json["metadata"]
                reserved = r["metadata"]["prereserve_doi"]
                r["metadata"] = copy.deepcopy(json["metadata"])
                # Zenodo normalizes HTML without changing rendered prose.
                r["metadata"]["description"] = r["metadata"]["description"].replace("<P>", "<p>").replace("</P>", "</p>").replace("<B>", "<strong>").replace("</B>", "</strong>")
                r["metadata"]["keywords"].reverse()
                # As the real service does: a null affiliation on each creator, a scheme on each
                # related identifier.
                for creator in r["metadata"].get("creators", []):
                    creator.setdefault("affiliation", None)
                for related in r["metadata"].get("related_identifiers", []):
                    related.setdefault("scheme", "doi")
                r["metadata"]["license"] = {"id": r["metadata"]["license"]}
                r["metadata"]["prereserve_doi"] = reserved
                r["title"] = r["metadata"]["title"]
                return response(r)
            if len(parts) == 5 and parts[3] == "files" and method == "DELETE":
                found = [f for f in r["files"] if f["id"] == parts[4]]
                if not found:
                    return response({"error": "missing file"}, 404)
                r["files"].remove(found[0])
                return response(status=204)
            if parts[3:] == ["actions", "publish"] and method == "POST":
                if not r["files"]:
                    return response({"error": "file required"}, 400)
                r["state"] = "done"
                r["submitted"] = True
                r["doi"] = r["metadata"]["prereserve_doi"]["doi"]
                r["conceptdoi"] = f"10.5281/zenodo.{r['conceptrecid']}"
                r["metadata"].pop("prereserve_doi", None)
                if self.after_publish:
                    self.after_publish(ident)
                return response(r, 202)
        return response({"error": "unsupported endpoint"}, 404)

    def upload_file(self, url, path, *, headers=None):
        return self.request("PUT", url, data=path.read_bytes(), headers=headers)


class DepositTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.config = json.loads(EXAMPLE.read_text())
        self.config_path = self.repo / "records.json"
        self.state = self.root / "state.json"
        self.deposited = self.repo / "specs/publication/DEPOSITED.sha256"
        self.deposited.parent.mkdir(parents=True)
        self.deposited.write_text("# Format: <filename>  <version>  <sha256>\n")
        self.before = self.deposited.read_bytes()
        (self.repo / "input").mkdir()
        (self.repo / "tools").mkdir()
        self.checker = self.repo / "tools/check_spec_versioning.py"
        self.checker.write_text("# Test checker: invoked as a real subprocess.\n")
        for key, r in self.config["records"].items():
            contents = "tiny invented dataset\n" if key == "vectors" else (
                f"| Version | {r['metadata']['version']} |\n"
                f"Revision: https://doi.org/{r['version_doi']}\n"
                f"Concept: https://doi.org/{r['concept_doi']}\n"
                "See https://example.org/review\n")
            (self.repo / r["files"][0]["path"]).write_text(contents)
        self.write_config()
        self.fake = FakeZenodo(self.config)
        self.dirty = False
        self.commit = "1" * 40
        self.subprocesses = []
        actual_run = subprocess.run

        def local_run(args, **kwargs):
            self.subprocesses.append(args)
            if args[0] != "git":
                return actual_run(args, **kwargs)
            if args[1:] == ["rev-parse", "--git-dir"]:
                if (Path(kwargs["cwd"]) / ".git").exists():
                    return subprocess.CompletedProcess(args, 0, ".git", "")
                return subprocess.CompletedProcess(args, 128, "", "fatal: not a git repository")
            if args[1] == "status":
                return subprocess.CompletedProcess(args, 0, " M file\n" if self.dirty else "", "")
            if args[1:] == ["rev-parse", "HEAD"]:
                return subprocess.CompletedProcess(args, 0, self.commit + "\n", "")
            raise AssertionError(f"Unexpected git command: {args}")

        self.addCleanup(patch.stopall)
        patch.dict(os.environ, {"ZENODO_TOKEN": TOKEN}).start()
        patch.object(z.subprocess, "run", side_effect=local_run).start()
        # A regression to real HTTP is a loud failure, not a skipped test.
        patch.object(z.UrllibTransport, "request", side_effect=AssertionError("Network forbidden")).start()

    def write_config(self):
        self.config_path.write_text(json.dumps(self.config))

    def cli(self, mode, *flags, expected=0, transport=None):
        out, err = io.StringIO(), io.StringIO()
        args = [mode, "--records", str(self.config_path), "--repo", str(self.repo), *flags]
        with redirect_stdout(out), redirect_stderr(err):
            code = z.main(args, self.fake if transport is None else transport)
        self.assertEqual(code, expected, out.getvalue() + err.getvalue())
        output = out.getvalue() + err.getvalue()
        self.assertNotIn(TOKEN, output)
        return output

    def plan(self):
        return json.loads(self.cli("plan", "--json"))

    def stage(self):
        self.cli("stage-metadata")
        self.cli("stage-files")

    def flags(self, digest=None):
        return ["--plan-digest", digest or self.plan()["plan_digest"],
                "--publish-permanently", "--webhook-off-confirmed", "--state", str(self.state)]

    def remote(self, key):
        return self.fake.records[str(self.config["records"][key]["draft_id"])]

    def publish_all(self):
        self.stage()
        self.cli("publish", *self.flags())

    def add_reference(self):
        config = copy.deepcopy(self.config)
        r = config["records"]["vectors"]
        r.update(draft_id=800002, version_doi="10.5281/zenodo.800002",
                 concept_doi="10.5281/zenodo.800001")
        self.fake.records["800002"] = FakeZenodo(config).records["800002"]
        self.fake.put_file(800002, "reference +é.txt", b"reference content long enough for range")
        result = self.fake.request("POST", f"{self.fake.base}/deposit/depositions/800002/actions/publish",
                                   headers={"Authorization": f"Bearer {TOKEN}"})
        self.assertEqual(result.status, 202)
        self.fake.calls.clear()

    def test_real_draft_preflight_and_published_api_shapes(self):
        self.stage()
        for r in self.fake.records.values():
            self.assertNotIn("doi", r)
            self.assertNotIn("conceptdoi", r)
            self.assertEqual(r["metadata"]["prereserve_doi"],
                             {"doi": f"10.5281/zenodo.{r['id']}", "recid": r["id"]})
            self.assertEqual(set(r["links"]),
                             {"bucket", "publish", "discard", "files", "latest_draft", "newversion", "self"})
        self.cli("publish", *self.flags())
        auth = {"Authorization": f"Bearer {TOKEN}"}
        for r in self.fake.records.values():
            self.assertNotIn("prereserve_doi", r["metadata"])
            f = r["files"][0]
            self.assertEqual(str(uuid.UUID(f["id"])), f["id"])
            self.assertRegex(f["checksum"], r"^[0-9a-f]{32}$")
            self.assertEqual(self.fake.request("GET", f["links"]["download"], headers=auth).status, 404)
            public = json.loads(self.fake.request("GET", f"{self.fake.base}/records/{r['id']}", headers=auth).body)
            self.assertEqual(set(public["files"][0]), {"key", "size", "checksum", "links"})
            self.assertEqual(public["files"][0]["checksum"], "md5:" + f["checksum"])
        self.fake.calls.clear()
        self.cli("record")
        self.assertEqual(sum(c[1].endswith("/content") for c in self.fake.calls), 6)
        self.assertFalse(any("/draft/" in c[1] for c in self.fake.calls))

    def test_content_endpoint_is_unavailable_before_publication(self):
        self.cli("stage-files")
        r = self.remote("vectors")
        result = self.fake.request("GET", self.fake.content_url(r["id"], "vectors.txt"),
                                   headers={"Authorization": f"Bearer {TOKEN}"})
        self.assertEqual(result.status, 404)

    def test_draft_reserved_doi_and_recid_required_before_writes(self):
        r = self.remote("vectors")
        original = copy.deepcopy(r["metadata"]["prereserve_doi"])
        for reserved in ({}, {**original, "doi": "10.5281/zenodo.123"}, {**original, "recid": 123}):
            with self.subTest(reserved=reserved):
                r["metadata"]["prereserve_doi"] = reserved
                for mode in ("stage-metadata", "stage-files"):
                    self.cli(mode, expected=1)
        self.assertFalse(self.fake.writes)

    def test_checksum_prefix_accepted_by_legacy_file_verification(self):
        self.stage()
        for r in self.fake.records.values():
            for f in r["files"]:
                f["checksum"] = "md5:" + f["checksum"].upper()
        writes = len(self.fake.writes)
        self.cli("stage-files")
        self.assertEqual(len(self.fake.writes), writes)
        self.cli("publish", *self.flags())
        self.cli("record")

    def test_post_publish_uses_records_api_checksums(self):
        self.stage()
        def corrupt(method, url, body):
            if url == f"{self.fake.base}/records/900002":
                return response({"id": 900002, "doi": "10.5281/zenodo.900002",
                                 "conceptdoi": "10.5281/zenodo.900001", "conceptrecid": "900001",
                                 "files": [{"key": "vectors.txt", "size": 22, "checksum": "md5:" + "0" * 32}]})
        self.fake.hook = corrupt
        self.assertIn("file names, sizes or MD5 differ", self.cli("publish", *self.flags(), expected=1))
        self.assertEqual(len(self.fake.posts), 1)

    def test_record_ignores_legacy_files_after_publication(self):
        self.publish_all()
        def legacy(method, url, body):
            if method == "GET" and "/deposit/depositions/" in url:
                r = copy.deepcopy(self.fake.records[url.rsplit("/", 1)[1]])
                r["files"] = [{"filename": "wrong", "filesize": -1, "checksum": "invalid"}]
                return response(r)
        self.fake.hook = legacy
        self.cli("record")

    def test_content_redirect_strips_token_on_cross_origin_and_never_restores_it(self):
        self.publish_all()
        start = self.fake.content_url(900002, "vectors.txt")
        storage = "https://storage.example/content?signature=opaque"
        back = f"{self.fake.base}/returned-content"
        original = self.fake.request
        seen = []
        def request(method, url, **kwargs):
            seen.append((method, url, dict(kwargs["headers"])))
            if url == start:
                return z.Response(302, {"Location": storage}, b"")
            if url == storage:
                return z.Response(307, {"location": back}, b"")
            if url == back:
                return z.Response(200, {}, self.fake.blobs[start])
            return original(method, url, **kwargs)
        with patch.object(self.fake, "request", side_effect=request):
            self.cli("record")
        for method, url, headers in seen:
            self.assertEqual(method, "GET")
            if url in {storage, back}:
                self.assertNotIn("Authorization", headers)
            else:
                self.assertEqual(headers["Authorization"], f"Bearer {TOKEN}")
        self.assertTrue(any(url == storage for _, url, _ in seen))

    def test_content_redirect_limits_https_and_same_origin_credentials(self):
        self.publish_all()
        start = self.fake.content_url(900002, "vectors.txt")
        for hops, target, success in ((3, "/api/hop", True), (4, "/api/hop", False),
                                      (1, "http://storage.example/file", False),
                                      (1, "https://user:pass@storage.example/file", False)):
            with self.subTest(hops=hops, target=target):
                count = 0
                def redirect(method, url, body):
                    nonlocal count
                    if url == start or "/api/hop" in url:
                        count += 1
                        if count <= hops:
                            return z.Response(302, {"Location": target + str(count)}, b"")
                        return z.Response(200, {}, self.fake.blobs[start])
                self.fake.hook = redirect
                self.fake.calls.clear()
                self.deposited.write_bytes(self.before)
                self.cli("record", expected=0 if success else 1)
                self.assertLessEqual(count, 4)
                self.assertFalse(any(c[1].startswith("http:") or "user:pass" in c[1] for c in self.fake.calls))
                if success:
                    self.assertEqual(count, 4)
                    self.assertTrue(all(c[3]["Authorization"] == f"Bearer {TOKEN}" for c in self.fake.calls))
                else:
                    self.assertEqual(self.deposited.read_bytes(), self.before)

    def test_other_requests_refuse_redirects(self):
        self.fake.hook = lambda *args: z.Response(302, {"Location": "https://storage.example/file"}, b"")
        self.assertIn("HTTP 302", self.cli("status", expected=1))
        self.assertEqual(len(self.fake.calls), 1)

    def test_record_encodes_content_filename(self):
        r = self.config["records"]["vectors"]
        r["files"][0]["name"] = r["deposit_lines"][0]["name"] = "véctors+?.txt"
        self.write_config()
        self.publish_all()
        self.cli("record")
        self.assertTrue(any("/v%C3%A9ctors%2B%3F.txt/content" in c[1] for c in self.fake.calls))

    def test_deposit_line_comment_version_and_name_refused(self):
        line = self.config["records"]["vectors"]["deposit_lines"][0]
        for field in ("name", "version"):
            with self.subTest(field=field):
                old = line[field]
                line[field] = "#comment"
                self.write_config()
                self.cli("plan", expected=1)
                line[field] = old
        self.assertFalse(self.fake.calls)

    def test_record_and_publish_share_state_directory_lock(self):
        self.publish_all()
        flags = self.flags()
        self.fake.calls.clear()
        with z._DirectoryLock(self.state.parent):
            self.assertIn("using this state directory", self.cli("record", "--state", str(self.state), expected=1))
            self.assertIn("using this state directory", self.cli("publish", *flags, expected=1))
        self.assertFalse(self.fake.calls)
        self.assertEqual(self.deposited.read_bytes(), self.before)

    def test_empty_errors_have_class_names_in_journal_and_save_diagnostic(self):
        self.stage()
        flags = self.flags()
        def interrupt(method, url, body):
            if method == "POST":
                raise KeyboardInterrupt()
        self.fake.hook = interrupt
        self.cli("publish", *flags, expected=1)
        self.assertEqual(json.loads(self.state.read_text())["error"], "KeyboardInterrupt")
        with patch.object(z, "atomic_write", side_effect=OSError()):
            output = self.cli("publish", *flags, expected=1)
        self.assertIn("Could not save progress: OSError", output)

    def test_inventory_retries_missing_id_once_and_refuses_persistent_gap(self):
        self.stage()
        flags = self.flags()
        for persistent in (False, True):
            with self.subTest(persistent=persistent):
                listings = 0
                def missing(method, url, body):
                    nonlocal listings
                    if "/deposit/depositions?" in url:
                        page = int(urllib.parse.parse_qs(urllib.parse.urlsplit(url).query)["page"][0])
                        if page == 1:
                            listings += 1
                        if page == 2 and (persistent or listings == 1):
                            return response([self.remote("object")])
                self.fake.hook = missing
                self.fake.calls.clear()
                self.cli("publish", *flags, expected=1 if persistent else 0)
                if persistent:
                    self.assertEqual(listings, 2)
                    self.assertFalse(self.fake.posts)
                else:
                    self.assertEqual(listings, 8)  # Two initial attempts, six post-publish lists.

    def test_inventory_retries_overlapping_pages(self):
        listings = 0
        def overlap(method, url, body):
            nonlocal listings
            if "/deposit/depositions?" in url:
                page = int(urllib.parse.parse_qs(urllib.parse.urlsplit(url).query)["page"][0])
                if page == 1:
                    listings += 1
                if page == 2 and listings == 1:
                    return response([self.remote("parity"), self.remote("object")])
        self.fake.hook = overlap
        args = z.parser().parse_args(["status", "--records", str(self.config_path), "--repo", str(self.repo)])
        deposit = z.Deposit(args, self.fake)
        deposit.load()
        self.assertEqual(deposit.inventory(), set(self.fake.records))
        self.assertEqual(listings, 2)

    def test_probe_is_read_only_with_dirty_tree_missing_inputs_and_no_plan(self):
        self.add_reference()
        self.dirty = True
        (self.repo / "input/vectors.txt").unlink()
        self.checker.unlink()
        output = self.cli("probe", "--reference-record", "800002")
        self.assertEqual(sum(line.startswith("PASS ") for line in output.splitlines()), 10)
        self.assertFalse(self.fake.writes)
        self.assertFalse(self.subprocesses)
        self.assertFalse(self.state.exists())
        self.assertEqual(self.deposited.read_bytes(), self.before)
        ranges = [c for c in self.fake.calls if c[3].get("Range")]
        self.assertEqual(len(ranges), 1)
        self.assertEqual(ranges[0][3]["Range"], "bytes=0-15")

    def test_probe_without_records_file_and_range_200(self):
        self.add_reference()
        def full_range(method, url, body):
            if url.endswith("/content"):
                return z.Response(200, {}, b"0123456789abcdef")
        self.fake.hook = full_range
        out = io.StringIO()
        with redirect_stdout(out), redirect_stderr(out):
            code = z.main(["probe", "--reference-record", "800002", "--repo", str(self.repo)], self.fake)
        self.assertEqual(code, 0, out.getvalue())
        self.assertEqual(sum(line.startswith("PASS ") for line in out.getvalue().splitlines()), 4)
        self.assertFalse(self.subprocesses)
        self.assertFalse(self.fake.writes)

    def test_probe_reports_all_checks_after_contract_failures(self):
        self.add_reference()
        reference = self.fake.records["800002"]
        reference["files"][0]["checksum"] = "md5:" + reference["files"][0]["checksum"]
        self.remote("vectors")["doi"] = "unexpected"
        self.remote("companion")["metadata"]["prereserve_doi"]["recid"] = 123
        self.remote("object")["metadata"]["prereserve_doi"]["doi"] = "wrong"
        self.remote("encrypt")["conceptrecid"] = "123"
        self.remote("parity")["links"]["bucket"] = "https://evil.example/bucket"
        output = self.cli("probe", "--reference-record", "800002", expected=1)
        lines = [line for line in output.splitlines() if line.startswith(("PASS ", "FAIL "))]
        self.assertEqual(len(lines), 10)
        self.assertEqual(sum(line.startswith("FAIL ") for line in lines), 6)
        self.assertFalse(self.fake.writes)
        self.assertFalse(self.subprocesses)

    def test_probe_records_md5_size_prefix_and_range_failures(self):
        self.add_reference()
        original = self.fake.request
        endpoint = f"{self.fake.base}/records/800002"
        for field, value in (("checksum", "md5:" + "0" * 32), ("checksum", "0" * 32),
                             ("size", "38"), ("key", "wrong")):
            with self.subTest(field=field):
                def mutate(method, url, **kwargs):
                    result = original(method, url, **kwargs)
                    if url == endpoint:
                        body = json.loads(result.body)
                        body["files"][0][field] = value
                        return response(body)
                    return result
                with patch.object(self.fake, "request", side_effect=mutate):
                    output = self.cli("probe", "--reference-record", "800002", expected=1)
                self.assertIn("FAIL reference records API", output)
        for status, data in ((206, b"short"), (404, b"0123456789abcdef")):
            self.fake.hook = lambda method, url, body: z.Response(status, {}, data) if url.endswith("/content") else None
            self.assertIn("FAIL reference content range", self.cli("probe", "--reference-record", "800002", expected=1))
        self.assertFalse(self.fake.writes)

    def test_probe_reference_identity_and_legacy_size_failures(self):
        self.add_reference()
        good = copy.deepcopy(self.fake.records["800002"])
        mutations = [lambda r: r.update(state="unsubmitted"),
                     lambda r: r.pop("doi"), lambda r: r.pop("conceptdoi"),
                     lambda r: r.pop("conceptrecid"),
                     lambda r: r["files"][0].update(filesize=True),
                     lambda r: r["files"][0].update(checksum="invalid")]
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                self.fake.records["800002"] = copy.deepcopy(good)
                mutation(self.fake.records["800002"])
                self.assertIn("FAIL reference deposit API", self.cli("probe", "--reference-record", "800002", expected=1))
        self.assertFalse(self.fake.writes)

    def test_probe_inventory_failure_is_one_line_and_other_checks_still_run(self):
        self.add_reference()
        def unavailable(method, url, body):
            if "/deposit/depositions?" in url:
                return z.Response(503, {}, b"upstream\nservice\nunavailable")
        self.fake.hook = unavailable
        output = self.cli("probe", "--reference-record", "800002", expected=1)
        self.assertIn("FAIL account inventory: HTTP 503: upstream service unavailable\n", output)
        self.assertEqual(sum(line.startswith("PASS ") for line in output.splitlines()), 9)
        self.assertFalse(self.fake.writes)

    def test_probe_reference_need_not_appear_in_account_inventory(self):
        self.add_reference()
        original = self.fake.request
        def listing(method, url, **kwargs):
            result = original(method, url, **kwargs)
            if "/deposit/depositions?" in url:
                return response([r for r in json.loads(result.body) if r["id"] != 800002])
            return result
        with patch.object(self.fake, "request", side_effect=listing):
            self.cli("probe", "--reference-record", "800002")

    def test_record_checks_download_md5_even_when_sha_matches(self):
        self.publish_all()
        actual_md5 = hashlib.md5
        class WrongMD5:
            def hexdigest(self):
                return "0" * 32
        def md5(data=b""):
            return WrongMD5() if data == b"tiny invented dataset\n" else actual_md5(data)
        with patch.object(z.hashlib, "md5", side_effect=md5):
            self.assertIn("downloaded bytes differ", self.cli("record", expected=1))
        self.assertEqual(self.deposited.read_bytes(), self.before)

    def test_plan_is_local_and_has_canonical_digest_hashes_and_urls(self):
        class NoNetwork(z.Transport):
            def request(self, *args, **kwargs):
                raise AssertionError("Plan used transport")
        with patch.dict(os.environ, {"ZENODO_TOKEN": ""}):
            p = json.loads(self.cli("plan", "--json", transport=NoNetwork()))
        payload = {k: p[k] for k in ("api_base", "commit", "publish_order", "records")}
        self.assertEqual(p["plan_digest"], hashlib.sha256(z.canonical(payload).encode()).hexdigest())
        f = p["records"]["vectors"]["files"][0]
        data = (self.repo / "input/vectors.txt").read_bytes()
        self.assertEqual(f["sha256"], hashlib.sha256(data).hexdigest())
        self.assertEqual(f["md5"], hashlib.md5(data).hexdigest())
        self.assertEqual(f["size"], len(data))
        self.assertIn("https://example.org/review", p["external_urls"]["object"])
        self.assertIn(["python3", "tools/check_spec_versioning.py"], self.subprocesses)
        self.assertFalse(self.fake.calls)

    def test_plan_digest_binds_commit_metadata_identity_and_bytes(self):
        first = self.plan()["plan_digest"]
        self.commit = "2" * 40
        self.assertNotEqual(first, self.plan()["plan_digest"])
        self.commit = "1" * 40
        self.config["records"]["vectors"]["metadata"]["title"] += " changed"
        self.write_config()
        second = self.plan()["plan_digest"]
        self.assertNotEqual(first, second)
        (self.repo / "input/vectors.txt").write_text("changed bytes")
        self.assertNotEqual(second, self.plan()["plan_digest"])

    def test_plan_checks_documents_and_checker(self):
        path = self.repo / "input/object.txt"
        original = path.read_text()
        for broken in (original.replace("0.1-test", "wrong"), original.replace("900006", "123456"),
                       original.replace("900005", "123456")):
            with self.subTest(broken=broken):
                path.write_text(broken)
                self.cli("plan", expected=1)
        path.write_text(original)
        self.checker.write_text("raise SystemExit('checker deliberately failed')\n")
        self.assertIn("checker deliberately failed", self.cli("plan", expected=1))
        self.assertFalse(self.fake.calls)

    def test_guide_plan_has_dated_version_and_fixed_six_record_order(self):
        plan = self.plan()
        self.assertEqual(plan["publish_order"],
                         ["vectors", "companion", "guide", "object", "encrypt", "parity"])
        guide = plan["records"]["guide"]
        self.assertEqual(guide["kind"], "document")
        self.assertEqual(guide["metadata"]["version"], "2026-10-01")
        self.assertEqual(guide["deposit_lines"][0]["version"], "2026-10-01")
        for field in ("concept_doi", "version_doi"):
            self.assertIn("https://doi.org/" + guide[field], plan["external_urls"]["guide"])
        self.assertFalse(self.fake.calls)

    def test_guide_plan_requires_matching_version_and_both_dois(self):
        guide = self.config["records"]["guide"]
        path = self.repo / guide["files"][0]["path"]
        original = path.read_text()
        for old, new, message in (
                ("2026-10-01", "2026-10-02", "Version row differs"),
                (guide["concept_doi"], "missing", "must cite concept_doi"),
                (guide["version_doi"], "missing", "must cite version_doi")):
            with self.subTest(old=old):
                self.assertIn(old, original)
                path.write_text(original.replace(old, new))
                self.assertIn(message, self.cli("plan", expected=1))
        self.assertFalse(self.fake.calls)

    def test_guide_requires_valid_date_and_matching_deposit_version(self):
        guide = self.config["records"]["guide"]
        for version in ("1.0.0", "2026-1-01", "2026-02-30"):
            with self.subTest(version=version):
                guide["metadata"]["version"] = version
                self.write_config()
                self.assertIn("valid date YYYY-MM-DD", self.cli("plan", expected=1))
        guide["metadata"]["version"] = "2026-10-01"
        guide["deposit_lines"][0]["version"] = "2026-10-02"
        self.write_config()
        self.assertIn("deposit version differs", self.cli("plan", expected=1))
        self.assertFalse(self.fake.calls)

    def test_missing_guide_record_is_refused(self):
        del self.config["records"]["guide"]
        self.write_config()
        self.assertIn("All six records are required", self.cli("plan", expected=1))
        self.assertFalse(self.fake.calls)

    def test_plan_rejects_absent_file(self):
        (self.repo / "input/vectors.txt").unlink()
        self.assertIn("missing file", self.cli("plan", expected=1))

    def test_config_refuses_missing_extra_reordered_and_invalid_ids(self):
        original = copy.deepcopy(self.config)
        mutations = [lambda c: c["records"].pop("parity"),
                     lambda c: c["records"].update(extra=copy.deepcopy(c["records"]["parity"])),
                     lambda c: c["publish_order"].reverse(),
                     lambda c: c["records"]["vectors"].update(version_doi="10.5281/zenodo.123"),
                     lambda c: c["records"]["vectors"].update(concept_doi="10.1234/other"),
                     lambda c: c["records"]["vectors"]["metadata"].update(prereserve_doi={})]
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                self.config = copy.deepcopy(original)
                mutation(self.config)
                self.write_config()
                self.cli("plan", expected=1)
        self.assertFalse(self.fake.calls)

    def test_path_traversal_absolute_symlink_and_duplicate_files_refused(self):
        outside = self.root / "outside.txt"
        outside.write_text("outside")
        (self.repo / "input/escape.txt").symlink_to(outside)
        for path in ("../outside.txt", str(outside), "input/escape.txt", "input/../input/vectors.txt"):
            with self.subTest(path=path):
                self.config["records"]["vectors"]["files"][0]["path"] = path
                self.write_config()
                self.cli("plan", expected=1)
        self.config["records"]["vectors"]["files"] = [{"name": "same", "path": "input/vectors.txt"}] * 2
        self.write_config()
        self.assertIn("duplicate filename", self.cli("plan", expected=1))

    def test_filename_cannot_turn_a_deposit_line_into_a_comment(self):
        r = self.config["records"]["vectors"]
        r["files"][0]["name"] = r["deposit_lines"][0]["name"] = "#hidden.txt"
        self.write_config()
        self.assertIn("Unsafe filename", self.cli("plan", expected=1))
        self.assertFalse(self.fake.calls)

    def test_duplicate_json_keys_refused(self):
        text = self.config_path.read_text()
        self.config_path.write_text(text.replace('"api_base":', '"api_base": "https://evil.example/api", "api_base":'))
        self.assertIn("Duplicate JSON key", self.cli("plan", expected=1))

    def test_publish_names_every_missing_flag_and_never_posts(self):
        flags = self.flags()
        cases = [([], ["--plan-digest", "--publish-permanently", "--webhook-off-confirmed"]),
                 (flags[2:], ["--plan-digest"]),
                 ([f for f in flags if f != "--publish-permanently"], ["--publish-permanently"]),
                 ([f for f in flags if f != "--webhook-off-confirmed"], ["--webhook-off-confirmed"])]
        for args, missing in cases:
            with self.subTest(missing=missing):
                output = self.cli("publish", *args, expected=1)
                for name in missing:
                    self.assertIn(name, output)
        self.assertFalse(self.fake.calls)

    def test_publish_refuses_wrong_digest_dirty_tree_and_missing_record(self):
        flags = self.flags()
        self.cli("publish", *self.flags("0" * 64), expected=1)
        self.dirty = True
        self.assertIn("clean git tree", self.cli("publish", *flags, expected=1))
        self.dirty = False
        self.config["records"].pop("parity")
        self.write_config()
        self.cli("publish", *flags, expected=1)
        self.assertFalse(self.fake.calls)

    def test_stage_metadata_sanitization_idempotence_and_reservation_preserved(self):
        self.cli("stage-metadata")
        first = copy.deepcopy(self.fake.records)
        self.cli("stage-metadata")
        self.assertEqual(first, self.fake.records)
        self.assertIn("<strong>", self.remote("object")["metadata"]["description"])
        for r in self.fake.records.values():
            self.assertNotIn("conceptdoi", r)
            self.assertIn("prereserve_doi", r["metadata"])
        for method, url, body, headers in self.fake.calls:
            self.assertNotIn(TOKEN, url)
            if body:
                self.assertNotIn("prereserve_doi", body["metadata"])
        self.assertFalse(self.fake.posts)

    def test_metadata_normalization_sets_entities_whitespace_and_inline_tags(self):
        a = {"description": "<P>A &amp; te<B>st</B></P>", "keywords": ["a", "b", "a"],
             "related_identifiers": [{"identifier": "x", "relation": "references"}]}
        b = {"description": " A & test ", "keywords": ["b", "a"],
             "related_identifiers": [{"relation": "references", "identifier": "x"}] * 2}
        self.assertEqual(z.normalized_metadata(a), z.normalized_metadata(b))

    def test_stage_files_deletes_extras_replaces_corruption_and_is_idempotent(self):
        ident = self.remote("vectors")["id"]
        self.fake.put_file(ident, "extra.txt", b"extra")
        self.fake.put_file(ident, "vectors.txt", b"wrong")
        self.cli("stage-files")
        self.assertEqual(len([c for c in self.fake.calls if c[0] == "DELETE"]), 2)
        writes = len(self.fake.writes)
        self.cli("stage-files")
        self.assertEqual(len(self.fake.writes), writes)
        self.assertEqual([f["filename"] for f in self.remote("vectors")["files"]], ["vectors.txt"])
        self.assertFalse(self.fake.posts)

    def test_file_deletion_cannot_traverse_to_a_deposition(self):
        self.fake.put_file(self.remote("vectors")["id"], "extra.txt", b"extra")
        self.remote("vectors")["files"][0]["id"] = ".."
        self.assertIn("unsafe remote file id", self.cli("stage-files", expected=1))
        self.assertFalse(self.fake.writes)

    def test_stage_files_requires_plan(self):
        self.dirty = True
        self.cli("stage-files", expected=1)
        self.assertFalse(self.fake.calls)

    def test_stage_modes_refuse_published_and_never_publish(self):
        self.remote("vectors")["state"] = "done"
        for mode in ("stage-metadata", "stage-files"):
            with self.subTest(mode=mode):
                self.cli(mode, expected=1)
        self.assertFalse(self.fake.writes)

    def test_stage_identity_mismatches_prevent_writes(self):
        for field, value in (("conceptrecid", "123"), ("id", 123)):
            old = self.remote("vectors")[field]
            self.remote("vectors")[field] = value
            for mode in ("stage-metadata", "stage-files"):
                with self.subTest(field=field, mode=mode):
                    self.cli(mode, expected=1)
            self.remote("vectors")[field] = old
        self.assertFalse(self.fake.writes)

    def test_publish_rechecks_identity_metadata_and_files_before_post(self):
        self.stage()
        flags = self.flags()
        good = copy.deepcopy(self.remote("vectors"))
        mutations = [lambda r: r["metadata"]["prereserve_doi"].update(doi="10.5281/zenodo.123"),
                     lambda r: r["metadata"]["prereserve_doi"].update(recid=123),
                     lambda r: r.update(conceptrecid="123"),
                     lambda r: r.update(id=123),
                     lambda r: r.update(state="unknown"),
                     lambda r: r["files"][0].update(checksum="0" * 32),
                     lambda r: r["files"][0].update(filesize=999),
                     lambda r: r["files"][0].update(filename="wrong"),
                     lambda r: r.update(files=[])]
        for field in z.FIELDS:
            def change(r, field=field):
                value = [] if field in {"creators", "keywords", "related_identifiers"} else "wrong"
                r["metadata"][field] = value
            mutations.append(change)
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                self.fake.records[str(good["id"])] = copy.deepcopy(good)
                mutation(self.remote("vectors"))
                self.cli("publish", *flags, expected=1)
                # A missing planned inventory id prevents establishing a journal
                # baseline; other preflight failures preserve that baseline.
                self.assertEqual(self.state.exists(), self.remote("vectors")["id"] == good["id"])
                self.assertFalse(self.fake.posts)
                self.state.unlink(missing_ok=True)

    def test_fresh_get_catches_late_change(self):
        self.stage()
        flags = self.flags()
        reads = 0
        endpoint = f"{self.fake.base}/deposit/depositions/{self.remote('vectors')['id']}"
        def change(method, url, body):
            nonlocal reads
            if method == "GET" and url == endpoint:
                reads += 1
                if reads == 2:
                    self.remote("vectors")["metadata"]["title"] = "changed after inventory"
        self.fake.hook = change
        self.cli("publish", *flags, expected=1)
        self.assertFalse(self.fake.posts)

    def test_publish_fixed_order_pagination_and_state(self):
        self.publish_all()
        ids = [url.split("/")[-3] for url in self.fake.posts]
        self.assertEqual(ids, [str(self.config["records"][key]["draft_id"]) for key in z.ORDER])
        state = json.loads(self.state.read_text())
        self.assertEqual(set(state["public"]), set(z.ORDER))
        self.assertEqual(len(state["inventory"]), 6)
        self.assertEqual(state["uncertain"], [])
        self.assertTrue(any("page=4&" in c[1] for c in self.fake.calls))
        self.assertFalse(list(self.root.glob(".state.json.*")))

    def test_unexpected_inventory_stops_and_journal_lists_public_record(self):
        self.stage()
        def duplicate(ident):
            self.fake.records["999999"] = {"id": 999999, "state": "done"}
        self.fake.after_publish = duplicate
        output = self.cli("publish", *self.flags(), expected=1)
        self.assertIn("Unexpected new record(s): 999999", output)
        self.assertIn("Public records:", output)
        self.assertEqual(len(self.fake.posts), 1)
        state = json.loads(self.state.read_text())
        self.assertEqual(set(state["public"]), {"vectors"})
        self.assertEqual(state["unexpected_records"], ["999999"])
        # Resume must not forget the first inventory and bless the duplicate.
        self.cli("publish", *self.flags(), expected=1)
        self.assertEqual(len(self.fake.posts), 1)

    def test_half_published_resume_and_already_public_verification(self):
        self.stage()
        stop_id = str(self.remote("object")["id"])
        def fail(method, url, body):
            if method == "POST" and f"/{stop_id}/" in url:
                return response({"error": "temporary outage"}, 503)
        self.fake.hook = fail
        flags = self.flags()
        self.cli("publish", *flags, expected=1)
        self.assertEqual(set(json.loads(self.state.read_text())["public"]), {"vectors", "companion", "guide"})
        self.fake.hook = None
        # A public record must still be verified before skipping it.
        old = self.remote("vectors")["files"][0]["checksum"]
        self.remote("vectors")["files"][0]["checksum"] = "0" * 32
        count = len(self.fake.posts)
        self.cli("publish", *flags, expected=1)
        self.assertEqual(count, len(self.fake.posts))
        self.remote("vectors")["files"][0]["checksum"] = old
        self.cli("publish", *flags)
        self.assertEqual(set(json.loads(self.state.read_text())["public"]), set(z.ORDER))
        posts = list(self.fake.posts)
        self.cli("publish", *flags)
        self.assertEqual(posts, self.fake.posts)

    def test_lost_publish_response_reconciles_before_resume(self):
        self.stage()
        def lost(ident):
            self.fake.after_publish = None
            raise OSError("response lost after publication")
        self.fake.after_publish = lost
        flags = self.flags()
        self.cli("publish", *flags, expected=1)
        self.assertEqual(set(json.loads(self.state.read_text())["public"]), {"vectors"})
        self.cli("publish", *flags)
        self.assertEqual(len(self.fake.posts), 6)

    def test_post_publication_identity_failure_is_recorded_and_stops(self):
        self.stage()
        def wrong(ident):
            self.fake.records[ident]["conceptdoi"] = "10.5281/zenodo.123"
        self.fake.after_publish = wrong
        self.cli("publish", *self.flags(), expected=1)
        self.assertEqual(len(self.fake.posts), 1)
        self.assertEqual(json.loads(self.state.read_text())["public"]["vectors"]["conceptdoi"], "10.5281/zenodo.123")

    def test_state_inside_git_repo_or_symlink_refused_before_post(self):
        self.stage()
        flags = self.flags()
        other = self.root / "other"
        other.mkdir()
        (other / ".git").write_text("gitdir: elsewhere")
        linked = self.root / "link.json"
        linked.symlink_to(self.repo / "state.json")
        for state in (self.repo / "state.json", other / "state.json", linked):
            with self.subTest(state=state):
                self.cli("publish", *flags[:-1], str(state), expected=1)
        self.assertFalse(self.fake.posts)

    def test_state_must_be_writable_before_publication_and_plan_must_match(self):
        self.stage()
        flags = self.flags()
        with patch.object(z, "atomic_write", side_effect=OSError("state unavailable")):
            self.cli("publish", *flags, expected=1)
        self.assertFalse(self.fake.posts)
        self.state.write_text(json.dumps({"plan_digest": "wrong", "inventory": []}))
        self.cli("publish", *flags, expected=1)
        self.assertFalse(self.fake.posts)

    def test_record_verifies_all_downloads_before_appending_and_is_idempotent(self):
        self.publish_all()
        self.cli("record")
        content = self.deposited.read_text()
        self.assertTrue(content.startswith(self.before.decode()))
        rows = [line.split() for line in content.splitlines() if not line.startswith("#")]
        self.assertEqual(len(rows), 6)
        for name, version, sha in rows:
            self.assertEqual(version, "2026-10-01" if name == "guide.txt" else "0.1-test")
            self.assertEqual(sha, hashlib.sha256((self.repo / "input" / name).read_bytes()).hexdigest())
        self.cli("record")
        self.assertEqual(self.deposited.read_text(), content)
        downloads = [c for c in self.fake.calls if c[1].endswith("/content")]
        self.assertEqual(len(downloads), 12)

    def test_record_file_doi_version_and_state_mismatches_write_nothing(self):
        self.publish_all()
        good = copy.deepcopy(self.remote("parity"))
        mutations = [lambda r: r.update(state="unsubmitted"),
                     lambda r: r.update(doi="10.5281/zenodo.123"),
                     lambda r: r.update(conceptdoi="10.5281/zenodo.123"),
                     lambda r: r["metadata"].update(version="wrong"),
                     lambda r: r["files"][0].update(checksum="0" * 32),
                     lambda r: r.update(files=[])]
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                self.fake.records[str(good["id"])] = copy.deepcopy(good)
                mutation(self.remote("parity"))
                self.cli("record", expected=1)
                self.assertEqual(self.deposited.read_bytes(), self.before)
        self.fake.records[str(good["id"])] = good
        url = self.fake.content_url(good["id"], good["files"][0]["filename"])
        self.fake.blobs[url] = b"corrupt downloaded bytes"
        self.assertIn("downloaded bytes differ", self.cli("record", expected=1))
        self.assertEqual(self.deposited.read_bytes(), self.before)

    def test_record_checks_sha_even_when_md5_matches(self):
        self.publish_all()
        actual_sha = hashlib.sha256
        class WrongSHA:
            def hexdigest(self):
                return "0" * 64
        def sha(data=b""):
            # Planning hashes incrementally; download verification uses bytes.
            return WrongSHA() if data == b"tiny invented dataset\n" else actual_sha(data)
        with patch.object(z.hashlib, "sha256", side_effect=sha):
            self.cli("record", expected=1)
        self.assertEqual(self.deposited.read_bytes(), self.before)

    def test_record_refuses_conflicting_duplicate_without_any_append(self):
        self.publish_all()
        self.deposited.write_text(self.before.decode() + "parity.txt  0.1-test  " + "0" * 64 + "\n")
        before = self.deposited.read_bytes()
        self.cli("record", expected=1)
        self.assertEqual(self.deposited.read_bytes(), before)

    def test_status_read_only_and_needs_no_plan_or_local_files(self):
        self.dirty = True
        (self.repo / "input/vectors.txt").unlink()
        self.checker.unlink()
        output = self.cli("status")
        self.assertIn('"state":"unsubmitted"', output)
        self.assertIn('"conceptrecid":"900001"', output)
        self.assertEqual(len(self.fake.calls), 6)
        self.assertFalse(self.fake.writes)
        self.assertFalse(self.subprocesses)

    def test_token_masked_in_http_errors_exceptions_outputs_and_state(self):
        self.stage()
        def error(method, url, body):
            if method == "POST":
                return response({"error": f"server echoed {TOKEN}"}, 500)
        self.fake.hook = error
        output = self.cli("publish", *self.flags(), expected=1)
        self.assertIn("[REDACTED]", output)
        self.assertNotIn(TOKEN, self.state.read_text())
        self.assertNotIn(TOKEN, json.loads(self.state.read_text())["error"])
        self.fake.hook = lambda *args: (_ for _ in ()).throw(RuntimeError(f"transport echoed {TOKEN}"))
        args = z.parser().parse_args(["status", "--repo", str(self.repo), "--records", str(self.config_path)])
        with self.assertRaises(z.Refusal) as caught:
            z.Deposit(args, self.fake).execute()
        self.assertNotIn(TOKEN, str(caught.exception))
        self.assertNotIn(TOKEN, repr(caught.exception))
        self.fake.hook = None
        self.remote("vectors")["metadata"]["version"] = TOKEN
        self.assertIn("[REDACTED]", self.cli("status"))

    def test_cross_origin_api_links_refused_without_leaking_auth(self):
        self.remote("vectors")["links"]["bucket"] = "https://evil.example/bucket"
        self.cli("stage-files", expected=1)
        self.assertFalse(any("evil.example" in c[1] for c in self.fake.calls))
        self.remote("vectors")["links"]["bucket"] = f"{self.fake.base}/files/{self.remote('vectors')['id']}"
        self.publish_all()
        self.remote("vectors")["files"][0]["links"]["download"] = "https://evil.example/file"
        self.cli("record")
        self.assertFalse(any("evil.example" in c[1] for c in self.fake.calls))

    def test_resume_outage_preserves_previous_public_progress(self):
        self.publish_all()
        flags = self.flags()
        known = json.loads(self.state.read_text())["public"]
        self.fake.hook = lambda *args: response({"error": "offline"}, 503)
        output = self.cli("publish", *flags, expected=1)
        self.assertIn("Unable to confirm current state", output)
        self.assertEqual(json.loads(self.state.read_text())["public"], known)
        self.assertEqual(len(self.fake.posts), 6)

    def test_record_preserves_existing_bytes_including_crlf_comments(self):
        self.publish_all()
        before = b"# Existing header with CRLF\r\n"
        self.deposited.write_bytes(before)
        self.cli("record")
        self.assertTrue(self.deposited.read_bytes().startswith(before))

    def test_all_six_preflight_before_first_publication(self):
        self.stage()
        self.remote("parity")["metadata"]["title"] = "wrong last record"
        self.assertIn("parity: metadata differs", self.cli("publish", *self.flags(), expected=1))
        self.assertFalse(self.fake.posts)

    def test_publish_refuses_remote_missing_record(self):
        self.stage()
        del self.fake.records[str(self.remote("parity")["id"])]
        self.assertIn("inventory inconsistent", self.cli("publish", *self.flags(), expected=1))
        self.assertFalse(self.fake.posts)

    def test_uncertain_post_is_not_reported_as_private_when_reads_fail(self):
        self.stage()
        flags = self.flags()
        endpoint = f"{self.fake.base}/deposit/depositions/{self.remote('vectors')['id']}"
        posted = False
        def broken(method, url, body):
            nonlocal posted
            if method == "POST":
                posted = True
                raise OSError("lost connection")
            if posted and url == endpoint:
                raise OSError("cannot reconcile")
        self.fake.hook = broken
        output = self.cli("publish", *flags, expected=1)
        self.assertIn("Unable to confirm current state for: vectors", output)
        self.assertEqual(json.loads(self.state.read_text())["uncertain"], ["vectors"])
        self.assertEqual(len(self.fake.posts), 1)

    def test_stage_metadata_readback_detects_server_change(self):
        endpoint = f"{self.fake.base}/deposit/depositions/{self.remote('vectors')['id']}"
        reads = 0
        def changed(method, url, body):
            nonlocal reads
            if method == "GET" and url == endpoint:
                reads += 1
                if reads == 2:
                    self.remote("vectors")["metadata"]["description"] = "Different meaning"
        self.fake.hook = changed
        self.assertIn("metadata differs: description", self.cli("stage-metadata", expected=1))
        self.assertFalse(self.fake.posts)

    def test_stage_files_readback_detects_corrupt_upload(self):
        original = self.fake.upload_file
        def corrupt(url, path, *, headers=None):
            reply = original(url, path, headers=headers)
            self.remote("vectors")["files"][0]["checksum"] = "0" * 32
            return reply
        with patch.object(self.fake, "upload_file", side_effect=corrupt):
            self.assertIn("file names, sizes or MD5 differ", self.cli("stage-files", expected=1))
        self.assertFalse(self.fake.posts)

    def test_escaped_tokens_and_argument_errors_are_masked(self):
        secret = 'secret-"quoted"-\\-é'
        def error(*args):
            return response({"error": secret}, 500)
        self.fake.hook = error
        # Supply the new bearer to the fake independently of its fixed test token.
        original = self.fake.request
        def request(method, url, **kwargs):
            self.assertEqual(kwargs["headers"]["Authorization"], f"Bearer {secret}")
            kwargs["headers"] = {"Authorization": f"Bearer {TOKEN}"}
            return original(method, url, **kwargs)
        with patch.dict(os.environ, {"ESCAPED_TOKEN": secret}):
            with patch.object(self.fake, "request", side_effect=request):
                output = self.cli("status", "--token-env", "ESCAPED_TOKEN", expected=1)
            self.assertNotIn(secret, output)
            self.assertNotIn(json.dumps(secret)[1:-1], output)
            self.assertIn("[REDACTED]", output)
            out = io.StringIO()
            with redirect_stderr(out):
                self.assertEqual(z.main([secret, "--token-env", "ESCAPED_TOKEN"]), 1)
            self.assertNotIn(secret, out.getvalue())
            self.assertNotIn(repr(secret)[1:-1], out.getvalue())

    def test_document_url_delimiters_and_entity_normalization(self):
        self.assertEqual(z.external_urls("[link](https://example.org/a_(b)). <https://example.org/x>"),
                         {"https://example.org/a_(b)", "https://example.org/x"})
        self.assertNotEqual(z.normalized_metadata({"description": "&amp;lt;"}),
                            z.normalized_metadata({"description": "&lt;"}))

    def test_missing_token_refuses_http_but_not_plan(self):
        with patch.dict(os.environ, {"ZENODO_TOKEN": ""}):
            self.cli("plan")
            self.assertIn("Missing authentication token", self.cli("status", expected=1))
        self.assertFalse(self.fake.calls)

    def test_custom_token_environment(self):
        with patch.dict(os.environ, {"ZENODO_TOKEN": "wrong", "OTHER_TOKEN": TOKEN}):
            self.cli("status", "--token-env", "OTHER_TOKEN")

    def test_fake_enforces_unpublishable_empty_and_immutable_published(self):
        r = self.remote("vectors")
        endpoint = f"{self.fake.base}/deposit/depositions/{r['id']}"
        auth = {"Authorization": f"Bearer {TOKEN}"}
        self.assertEqual(self.fake.request("POST", endpoint + "/actions/publish", headers=auth).status, 400)
        self.assertNotIn("conceptdoi", r)
        self.fake.put_file(r["id"], "tiny.txt", b"tiny")
        self.assertEqual(self.fake.request("POST", endpoint + "/actions/publish", headers=auth).status, 202)
        self.assertIn("conceptdoi", r)
        for method, url in (("PUT", endpoint), ("DELETE", endpoint),
                            ("DELETE", endpoint + "/files/1"), ("PUT", r["links"]["bucket"] + "/tiny.txt")):
            self.assertEqual(self.fake.request(method, url, headers=auth).status, 403)


class UrllibTests(unittest.TestCase):
    def test_origin_comparison_uses_effective_port(self):
        self.assertEqual(z.url_origin("https://zenodo.org/api"), z.url_origin("https://ZENODO.org:443/api"))
        self.assertNotEqual(z.url_origin("https://zenodo.org/api"), z.url_origin("https://zenodo.org:0/api"))

    def test_upload_streams_file_with_length_and_bearer_header(self):
        class Reply:
            status, headers = 201, {}
            def __enter__(self):
                return self
            def __exit__(self, *args):
                pass
            def read(self):
                return b"{}"
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "tiny"
            path.write_bytes(b"tiny data")
            transport = z.UrllibTransport()
            captured = []
            def open_request(req, **kwargs):
                self.assertTrue(hasattr(req.data, "read"))
                self.assertEqual(req.data.read(4), b"tiny")
                self.assertEqual(req.data.read(), b" data")
                self.assertEqual(req.get_header("Content-length"), "9")
                self.assertEqual(req.get_header("Authorization"), "Bearer secret")
                captured.append(req)
                return Reply()
            with patch.object(transport.opener, "open", side_effect=open_request):
                self.assertEqual(transport.upload_file("https://zenodo.org/api/files/1/tiny", path,
                                 headers={"Authorization": "Bearer secret"}).status, 201)
            self.assertEqual(len(captured), 1)
            self.assertTrue(captured[0].data.closed)

    def test_redirects_are_not_followed(self):
        self.assertIsNone(z._NoRedirect().redirect_request(None, None, 302, "redirect", {}, "https://evil.example"))

    def test_atomic_write_failure_leaves_old_file_and_no_temporary(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "state.json"
            target.write_text("old")
            with patch.object(z.os, "replace", side_effect=OSError("interrupted")):
                with self.assertRaises(OSError):
                    z.atomic_write(target, "new")
            self.assertEqual(target.read_text(), "old")
            self.assertEqual(list(Path(directory).iterdir()), [target])


class ServerAddedMetadataTests(unittest.TestCase):
    """Zenodo adds a null affiliation and a scheme; real differences must still show."""

    SENT = {"title": "T", "version": "1", "license": "cc-by-4.0", "upload_type": "publication",
            "publication_type": "technicalnote", "language": "eng", "description": "<p>D</p>",
            "creators": [{"name": "The ArchiveTech Project"}],
            "related_identifiers": [{"relation": "references", "identifier": "10.5281/zenodo.1",
                                     "resource_type": "dataset"}]}

    def stored(self, **changes):
        remote = copy.deepcopy(self.SENT)
        remote["creators"][0]["affiliation"] = None
        remote["related_identifiers"][0]["scheme"] = "doi"
        remote.update(changes)
        return remote

    def test_server_added_fields_are_not_a_difference(self):
        self.assertEqual(z.normalized_metadata(self.SENT), z.normalized_metadata(self.stored()))

    def test_a_changed_creator_is_a_difference(self):
        other = self.stored(creators=[{"name": "Someone Else", "affiliation": None}])
        self.assertNotEqual(z.normalized_metadata(self.SENT), z.normalized_metadata(other))

    def test_a_changed_relation_or_identifier_is_a_difference(self):
        for field, value in (("relation", "cites"), ("identifier", "10.5281/zenodo.2"),
                             ("resource_type", "software")):
            other = self.stored()
            other["related_identifiers"][0][field] = value
            self.assertNotEqual(z.normalized_metadata(self.SENT), z.normalized_metadata(other), field)

    def test_a_real_value_in_an_added_field_is_still_compared(self):
        other = self.stored()
        other["creators"][0]["affiliation"] = "An Institution"
        self.assertNotEqual(z.normalized_metadata(self.SENT), z.normalized_metadata(other))


if __name__ == "__main__":
    unittest.main()
