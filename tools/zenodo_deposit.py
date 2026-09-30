#!/usr/bin/env python3
"""Stage and verify Remanence's five-record Zenodo milestone.

Publication is irreversible. This tool never creates, edits, or deletes a public
record. Resume publication with the SAME records and digest; the external state
file is a journal, never authority to skip remote verification. All HTTP is behind
Transport, so tests need neither a network nor a listening socket.
Publish-time remote file verification uses size and MD5, all the API offers.
SHA-256 is checked locally before staging and on downloaded bytes by record.

Example: python3 tools/zenodo_deposit.py plan --records records.json --json
Publishing additionally requires --plan-digest, --publish-permanently, and
--webhook-off-confirmed. The default state is in the repo's parent directory.
Read-only API check: probe --reference-record ID [--records records.json].
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import hashlib
from html.parser import HTMLParser
import json
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys
import tempfile
from typing import BinaryIO
import urllib.error
import urllib.parse
import urllib.request

ORDER = ["vectors", "companion", "object", "encrypt", "parity"]
DOI = re.compile(r"10\.5281/zenodo\.([1-9][0-9]*)\Z")
FIELDS = ("title", "version", "creators", "license", "upload_type",
          "publication_type", "keywords", "related_identifiers", "language",
          "description")


class Refusal(Exception):
    """An unsafe or unverifiable operation; messages are masked at the boundary."""


def require(condition, message):
    if not condition:
        raise Refusal(message)


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"),
                      ensure_ascii=True, allow_nan=False)


def redact(value, token):
    text = str(value)
    if token:
        # API error bodies and JSON output may escape the token before it reaches
        # this boundary. Mask those spellings as well as the original bytes.
        spellings = {token, json.dumps(token)[1:-1],
                     json.dumps(token, ensure_ascii=False)[1:-1], repr(token)[1:-1]}
        for spelling in sorted(spellings, key=len, reverse=True):
            text = text.replace(spelling, "[REDACTED]")
    return text


def contains_token(value, token):
    if isinstance(value, dict):
        return any(contains_token(k, token) or contains_token(v, token) for k, v in value.items())
    if isinstance(value, list):
        return any(contains_token(v, token) for v in value)
    return isinstance(value, str) and bool(token) and token in value


def external_urls(text):
    result = set()
    for url in re.findall(r"https?://[^\s<>\"'|`]+", text):
        url = url.rstrip(".,;:!?")
        # Strip Markdown delimiters, retaining balanced parentheses in URLs.
        for left, right in (("(", ")"), ("[", "]"), ("{", "}")):
            while url.endswith(right) and url.count(right) > url.count(left):
                url = url[:-1]
        result.add(url)
    return result


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, f"Duplicate JSON key: {key}")
        result[key] = value
    return result


def read_json(path):
    return json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=unique_object)


@dataclass
class Response:
    status: int
    headers: dict
    body: bytes


class Transport:
    def request(self, method, url, *, json=None, data=None, headers=None) -> Response:
        raise NotImplementedError

    def upload_file(self, url, path: Path, *, headers=None) -> Response:
        """Stream the file, with bounded memory, to a bucket URL."""
        raise NotImplementedError


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        # Never forward the bearer token to a redirect target, even on upload.
        return None


class UrllibTransport(Transport):
    def __init__(self):
        self.opener = urllib.request.build_opener(_NoRedirect())

    def request(self, method, url, *, json=None, data=None, headers=None):
        headers = dict(headers or {})
        if json is not None:
            data = canonical(json).encode()
            headers["Content-Type"] = "application/json"
        request = urllib.request.Request(url, data=data, headers=headers, method=method)
        try:
            with self.opener.open(request, timeout=120) as response:
                return Response(response.status, dict(response.headers), response.read())
        except urllib.error.HTTPError as error:
            with error:
                return Response(error.code, dict(error.headers), error.read())

    def upload_file(self, url, path, *, headers=None):
        with path.open("rb") as source:
            return self.request("PUT", url, data=source, headers={
                **(headers or {}), "Content-Type": "application/octet-stream",
                "Content-Length": str(os.fstat(source.fileno()).st_size),
            })


class _Text(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.parts = []

    def handle_data(self, data):
        self.parts.append(data)

    def handle_starttag(self, tag, attrs):
        if tag in {"p", "br", "div", "li", "ul", "ol", "h1", "h2", "h3", "pre"}:
            self.parts.append(" ")

    def handle_endtag(self, tag):
        self.handle_starttag(tag, [])


def normalized_metadata(metadata):
    result = {key: metadata.get(key) for key in FIELDS}
    parser = _Text()
    parser.feed(metadata.get("description", ""))
    parser.close()
    result["description"] = " ".join("".join(parser.parts).split())
    for key in ("keywords", "related_identifiers"):
        result[key] = sorted({canonical(item) for item in metadata.get(key, [])})
    if isinstance(result["license"], dict):
        result["license"] = result["license"].get("id")
    return result


def hash_stream(source: BinaryIO):
    sha, md5, size = hashlib.sha256(), hashlib.md5(), 0
    while chunk := source.read(1024 * 1024):
        sha.update(chunk)
        md5.update(chunk)
        size += len(chunk)
    return {"size": size, "sha256": sha.hexdigest(), "md5": md5.hexdigest()}


def checksum(value):
    require(isinstance(value, str), "Invalid remote MD5")
    value = value.removeprefix("md5:")
    require(re.fullmatch(r"[0-9a-fA-F]{32}", value), "Invalid remote MD5")
    return value.lower()


def url_origin(url):
    parsed = urllib.parse.urlsplit(url)
    return parsed.scheme, parsed.hostname, 443 if parsed.port is None else parsed.port


def local_path(repo, raw):
    require(isinstance(raw, str) and raw and "\\" not in raw,
            "File path must be a nonempty relative POSIX path")
    path = PurePosixPath(raw)
    require(not path.is_absolute() and ".." not in path.parts,
            f"Unsafe file path: {raw}")
    resolved = (repo / raw).resolve()
    require(resolved.is_relative_to(repo), f"File escapes repository: {raw}")
    return resolved


def filename(value):
    require(isinstance(value, str) and value not in {"", ".", ".."} and not value.startswith("#")
            and not re.search(r"[\s/\\\x00-\x1f\x7f]", value),
            f"Unsafe filename: {value!r}")


def run_local(args, cwd):
    result = subprocess.run(args, cwd=cwd, capture_output=True, text=True, check=False)
    require(result.returncode == 0,
            f"{' '.join(args)} failed: {result.stdout}{result.stderr}")
    return result.stdout.strip()


def atomic_write(path, text):
    # Same-directory replacement: a reader sees either the old or complete new file.
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent,
                                         prefix=f".{path.name}.", delete=False) as out:
            temporary = Path(out.name)
            out.write(text)
            out.flush()
            os.fsync(out.fileno())
        os.replace(temporary, path)
        temporary = None
        directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


class Deposit:
    def __init__(self, args, transport=None):
        self.args = args
        self.repo = Path(args.repo).resolve(strict=True)
        self.token = os.environ.get(args.token_env, "")
        self.transport = transport if transport is not None else UrllibTransport()
        self.public = {}
        self.uncertain = set()
        self.config = None

    def mask(self, value):
        return redact(value, self.token)

    def emit(self, value):
        print(self.mask(value))

    def load(self):
        self.config = read_json(Path(self.args.records))
        require(not contains_token(self.config, self.token),
                "Records configuration contains the authentication token")
        c = self.config
        require(c["publish_order"] == ORDER, f"publish_order must be {ORDER}")
        require(set(c["records"]) == set(ORDER), "All five records are required, with no extra records")
        self.configure_api(c["api_base"])
        seen_ids, seen_concepts, lines = set(), set(), set()
        for key in ORDER:
            r = c["records"][key]
            draft_id = str(r["draft_id"])
            require(re.fullmatch(r"[1-9][0-9]*", draft_id), f"{key}: invalid draft_id")
            require(DOI.fullmatch(r["version_doi"]) and
                    r["version_doi"] == f"10.5281/zenodo.{draft_id}", f"{key}: version DOI must match draft_id")
            concept = DOI.fullmatch(r["concept_doi"])
            require(concept is not None and concept[1] != draft_id, f"{key}: invalid concept DOI")
            require(draft_id not in seen_ids and concept[1] not in seen_concepts,
                    "Record and concept ids must be unique")
            seen_ids.add(draft_id)
            seen_concepts.add(concept[1])
            require(r["kind"] == ("dataset" if key == "vectors" else "document"),
                    f"{key}: incorrect record kind")
            metadata = r["metadata"]
            require(isinstance(metadata, dict) and "prereserve_doi" not in metadata,
                    f"{key}: never supply prereserve_doi")
            require(isinstance(metadata.get("version"), str) and metadata["version"]
                    and not re.search(r"\s", metadata["version"]), f"{key}: missing or invalid metadata.version")
            normalized_metadata(metadata)
            require(isinstance(r["files"], list) and r["files"], f"{key}: at least one file is required")
            names = set()
            for f in r["files"]:
                filename(f["name"])
                require(f["name"] not in names, f"{key}: duplicate filename")
                names.add(f["name"])
                local_path(self.repo, f["path"])
            for line in r.get("deposit_lines", []):
                filename(line["name"])
                require(line["name"] in names and isinstance(line["version"], str)
                        and line["version"] and not line["version"].startswith("#")
                        and not re.search(r"\s", line["version"]),
                        f"{key}: deposit line must name an uploaded file and a version")
                pair = (line["name"], line["version"])
                require(pair not in lines, "Duplicate deposit line in records")
                lines.add(pair)
        require(not seen_ids & seen_concepts, "Concept ids must not be version ids")

    def configure_api(self, base):
        url = urllib.parse.urlsplit(base)
        require(url.scheme == "https" and url.hostname and not url.username
                and not url.password and not url.query and not url.fragment
                and url.path == "/api", "api_base must be an HTTPS origin followed by /api")
        self.origin = url_origin(base)

    def plan(self):
        require(not run_local(["git", "status", "--porcelain", "--untracked-files=all"], self.repo),
                "Plan requires a clean git tree")
        commit = run_local(["git", "rev-parse", "HEAD"], self.repo)
        require(re.fullmatch(r"[0-9a-f]{40,64}", commit), "Invalid git commit")
        records, urls = {}, {}
        for key in ORDER:
            r = self.config["records"][key]
            files, external = [], set()
            for f in r["files"]:
                path = local_path(self.repo, f["path"])
                require(path.is_file(), f"{key}: missing file {f['path']}")
                with path.open("rb") as source:
                    hashes = hash_stream(source)
                files.append({"name": f["name"], **hashes})
                if r["kind"] == "document":
                    data = path.read_bytes()
                    require(hashlib.sha256(data).hexdigest() == hashes["sha256"],
                            f"{key}: document changed while planning")
                    text = data.decode("utf-8")
                    versions = re.findall(r"^\|\s*Version\s*\|\s*([^|]+?)\s*\|\s*$", text, re.M)
                    require(versions == [r["metadata"]["version"]], f"{key}: document Version row differs")
                    for field in ("version_doi", "concept_doi"):
                        require(re.search(re.escape(r[field]) + r"(?![\w])", text),
                                f"{key}: document must cite {field}")
                    external.update(external_urls(text))
            records[key] = {field: r[field] for field in
                            ("draft_id", "version_doi", "concept_doi", "kind", "metadata")}
            records[key]["files"] = sorted(files, key=lambda f: f["name"])
            records[key]["deposit_lines"] = r.get("deposit_lines", [])
            urls[key] = sorted(external)
        # One funnel: the repository checker, including DEPOSITED consistency.
        run_local(["python3", "tools/check_spec_versioning.py"], self.repo)
        require(not run_local(["git", "status", "--porcelain", "--untracked-files=all"], self.repo)
                and run_local(["git", "rev-parse", "HEAD"], self.repo) == commit,
                "Repository changed during planning")
        payload = {"api_base": self.config["api_base"], "commit": commit,
                   "publish_order": ORDER, "records": records}
        return {**payload, "plan_digest": hashlib.sha256(canonical(payload).encode()).hexdigest(),
                "external_urls": urls}

    def checked_url(self, url):
        parsed = urllib.parse.urlsplit(url)
        require(url_origin(url) == self.origin and not parsed.username
                and not parsed.password and not parsed.fragment,
                "Refusing an API link outside the configured HTTPS origin")
        require(not self.token or self.token not in url, "Token must not occur in a URL")
        return url

    def request(self, method, url, *, body=None, path=None, binary=False):
        require(self.token, f"Missing authentication token in {self.args.token_env}")
        require(not re.search(r"[\x00-\x20\x7f]", self.token), "Invalid authentication token")
        self.checked_url(url)
        headers = {"Authorization": f"Bearer {self.token}"}
        if path is None:
            response = self.transport.request(method, url, json=body, headers=headers)
        else:
            response = self.transport.upload_file(url, path, headers=headers)
        if not 200 <= response.status < 300:
            raise Refusal(f"HTTP {response.status}: {response.body.decode('utf-8', errors='replace')}")
        if binary:
            return response.body
        return json.loads(response.body, object_pairs_hook=unique_object) if response.body else None

    def content(self, ident, name, *, byte_range=False):
        """Only public content GETs may redirect; credentials never cross origins."""
        require(re.fullmatch(r"[1-9][0-9]*", str(ident)), "Invalid record id")
        url = (f"{self.config['api_base']}/records/{ident}/files/"
               f"{urllib.parse.quote(name, safe='')}/content")
        self.checked_url(url)
        require(self.token and not re.search(r"[\x00-\x20\x7f]", self.token),
                f"Missing or invalid authentication token in {self.args.token_env}")
        headers = {"Authorization": f"Bearer {self.token}"}
        if byte_range:
            headers["Range"] = "bytes=0-15"
        for hop in range(4):
            result = self.transport.request("GET", url, headers=dict(headers))
            if result.status not in {301, 302, 303, 307, 308}:
                require(result.status in ({200, 206} if byte_range else {200}),
                        f"Content HTTP {result.status}: {result.body.decode('utf-8', errors='replace')}")
                return result
            require(hop < 3, "Too many content redirects")
            location = next((v for k, v in result.headers.items() if k.lower() == "location"), None)
            require(isinstance(location, str) and location, "Missing content redirect location")
            target = urllib.parse.urljoin(url, location)
            parsed = urllib.parse.urlsplit(target)
            require(parsed.scheme == "https" and parsed.hostname and not parsed.username
                    and not parsed.password and not parsed.fragment
                    and not re.search(r"[\x00-\x20\x7f]", target)
                    and self.token not in target, "Unsafe content redirect")
            if url_origin(target) != url_origin(url):
                headers.pop("Authorization", None)
            url = target

    def endpoint(self, key):
        return f"{self.config['api_base']}/deposit/depositions/{self.config['records'][key]['draft_id']}"

    def get(self, key):
        record = self.request("GET", self.endpoint(key))
        if record.get("state") == "done":
            self.public[key] = {field: record.get(field) for field in ("id", "doi", "conceptdoi")}
            self.uncertain.discard(key)
        else:
            require(key not in self.public, f"{key}: previously public record no longer reports done")
        return record

    def identity(self, key, remote, *, done=False):
        r = self.config["records"][key]
        require(str(remote.get("id")) == str(r["draft_id"]), f"{key}: deposition id differs")
        require(remote.get("state") == ("done" if done else "unsubmitted"),
                f"{key}: expected {'published' if done else 'unsubmitted'} record")
        if done:
            doi = remote.get("doi")
        else:
            reserved = remote.get("metadata", {}).get("prereserve_doi", {})
            doi = reserved.get("doi")
            require(str(reserved.get("recid")) == str(r["draft_id"]),
                    f"{key}: reserved record id differs")
        require(doi == r["version_doi"], f"{key}: version DOI differs")
        require(str(remote.get("conceptrecid")) == DOI.fullmatch(r["concept_doi"])[1],
                f"{key}: concept id differs")
        if done:
            require(remote.get("conceptdoi") == r["concept_doi"], f"{key}: concept DOI differs")

    def metadata(self, key, remote):
        expected = normalized_metadata(self.config["records"][key]["metadata"])
        actual = normalized_metadata(remote["metadata"])
        differing = [field for field in FIELDS if expected[field] != actual[field]]
        require(not differing, f"{key}: metadata differs: {', '.join(differing)}")

    def file_map(self, key, remote, *, records_api=False):
        files = remote["files"]
        require(isinstance(files, list), f"{key}: invalid remote file list")
        result = {}
        for f in files:
            name = f["key" if records_api else "filename"]
            require(name not in result, f"{key}: duplicate remote filename")
            size = f["size" if records_api else "filesize"]
            require(type(size) is int and size >= 0, f"{key}: invalid remote size")
            result[name] = {"size": size, "md5": checksum(f["checksum"])}
        return result

    def published(self, key):
        r = self.config["records"][key]
        remote = self.request("GET", f"{self.config['api_base']}/records/{r['draft_id']}")
        require(str(remote.get("id")) == str(r["draft_id"])
                and remote.get("doi") == r["version_doi"]
                and remote.get("conceptdoi") == r["concept_doi"]
                and str(remote.get("conceptrecid")) == DOI.fullmatch(r["concept_doi"])[1],
                f"{key}: published records API identity differs")
        return remote

    def files(self, key, remote, plan, *, records_api=False):
        if remote.get("state") == "done" and not records_api:
            remote = self.published(key)
            records_api = True
        expected = {f["name"]: {"size": f["size"], "md5": f["md5"]}
                    for f in plan["records"][key]["files"]}
        require(self.file_map(key, remote, records_api=records_api) == expected,
                f"{key}: file names, sizes or MD5 differ")

    def inventory(self, expected=()):
        for attempt in range(2):
            found, consistent = self.inventory_once()
            if consistent and set(expected) <= found:
                return found
        raise Refusal("Account inventory inconsistent after re-listing (missing or repeated ids)")

    def inventory_once(self):
        found, page = set(), 1
        while True:
            rows = self.request("GET", f"{self.config['api_base']}/deposit/depositions?size=100&page={page}&sort=mostrecent")
            require(isinstance(rows, list), "Invalid account inventory")
            if not rows:
                return found, True
            ids = {str(row["id"]) for row in rows}
            if ids & found or len(ids) != len(rows):
                return found, False
            found.update(ids)
            page += 1

    def stage_metadata(self):
        for key in ORDER:
            remote = self.get(key)
            self.identity(key, remote)
            self.request("PUT", self.endpoint(key), body={"metadata": self.config["records"][key]["metadata"]})
            remote = self.get(key)
            self.identity(key, remote)
            self.metadata(key, remote)
            self.emit(f"{key}: metadata verified")

    def stage_files(self):
        plan = self.plan()
        for key in ORDER:
            remote = self.get(key)
            self.identity(key, remote)
            desired = {f["name"]: f for f in plan["records"][key]["files"]}
            existing = self.file_map(key, remote)
            for f in remote["files"]:
                name = f["filename"]
                if name not in desired or existing[name] != {k: desired[name][k] for k in ("size", "md5")}:
                    file_id = str(f["id"])
                    require(re.fullmatch(r"[A-Za-z0-9_-]+", file_id),
                            f"{key}: unsafe remote file id")
                    self.request("DELETE", f"{self.endpoint(key)}/files/{file_id}")
                    del existing[name]
            bucket = self.checked_url(remote["links"]["bucket"])
            require(not urllib.parse.urlsplit(bucket).query, "Bucket URL must not have a query")
            for f in self.config["records"][key]["files"]:
                if f["name"] not in existing:
                    path = local_path(self.repo, f["path"])
                    with path.open("rb") as source:
                        require(hash_stream(source) == {k: desired[f['name']][k] for k in ("size", "sha256", "md5")},
                                f"{key}: local bytes changed since plan")
                    self.request("PUT", f"{bucket.rstrip('/')}/{urllib.parse.quote(f['name'], safe='')}", path=path)
            remote = self.get(key)
            self.identity(key, remote)
            self.files(key, remote, plan)
            self.emit(f"{key}: files verified")

    def state_path(self):
        path = Path(self.args.state) if self.args.state else self.repo.parent / "zenodo-deposit-state.json"
        require(not path.is_symlink(), "State path must not be a symlink")
        path = path.resolve()
        require(path.parent.is_dir(), "State parent directory must exist")
        require(not path.is_relative_to(self.repo), "State must be outside a git repository")
        probe = subprocess.run(["git", "rev-parse", "--git-dir"], cwd=path.parent,
                               capture_output=True, text=True, check=False)
        require(probe.returncode != 0 and "not a git repository" in probe.stderr,
                "Cannot establish that state is outside a git repository")
        return path

    def publish(self):
        missing = [flag for flag, value in (("--plan-digest", self.args.plan_digest),
                   ("--publish-permanently", self.args.publish_permanently),
                   ("--webhook-off-confirmed", self.args.webhook_off_confirmed)) if not value]
        require(not missing, "Publish requires " + ", ".join(missing))
        plan = self.plan()
        require(self.args.plan_digest == plan["plan_digest"], "Plan digest differs; nothing published")
        path = self.state_path()
        # Lock the directory, not the atomically replaced inode. Fail closed if
        # another invocation could race this journal (Linux deployment host).
        with _DirectoryLock(path.parent):
            self.publish_locked(plan, path)

    def publish_locked(self, plan, path):
        baseline = None
        if path.exists():
            previous = read_json(path)
            require(previous["plan_digest"] == plan["plan_digest"],
                    "State belongs to a different plan; keep the original recovery journal")
            require(isinstance(previous["inventory"], list)
                    and all(isinstance(i, str) and re.fullmatch(r"[1-9][0-9]*", i)
                            for i in previous["inventory"]), "Invalid state inventory")
            baseline = set(previous["inventory"])
            known = previous.get("public", {})
            pending = previous.get("uncertain", [])
            require(isinstance(known, dict) and set(known) <= set(ORDER)
                    and all(isinstance(r, dict) for r in known.values())
                    and isinstance(pending, list) and set(pending) <= set(ORDER),
                    "Invalid publication progress in state")
            # Preserve observations across an outage. These are evidence for
            # reporting only: every resume still verifies all remote records.
            self.public.update(known)
            self.uncertain.update(pending)
        planned = {str(r["draft_id"]) for r in self.config["records"].values()}
        unexpected = set()

        def save(error=None):
            state = {"plan_digest": plan["plan_digest"], "commit": plan["commit"],
                     "inventory": sorted(baseline or []), "public": self.public,
                     "uncertain": sorted(self.uncertain), "unexpected_records": sorted(unexpected)}
            if error is not None:
                state["error"] = self.mask(str(error) or type(error).__name__)
            # Mask values before serialization, so escaped token characters also
            # cannot survive in a decoded state file.
            def scrub(value):
                if isinstance(value, str):
                    return self.mask(value)
                if isinstance(value, dict):
                    return {self.mask(k): scrub(v) for k, v in value.items()}
                if isinstance(value, list):
                    return [scrub(v) for v in value]
                return value
            atomic_write(path, canonical(scrub(state)) + "\n")

        try:
            current = self.inventory(planned | (baseline or set()))
            if baseline is None:
                baseline = current
            initial = {key: self.get(key) for key in ORDER}
            unexpected = current - baseline - planned
            require(not unexpected, f"Unexpected new record(s): {', '.join(sorted(unexpected))}")
            # Refuse known errors anywhere in the milestone before publishing
            # even its first record. Still read each record fresh before its POST.
            for key, remote in initial.items():
                self.identity(key, remote, done=remote.get("state") == "done")
                self.metadata(key, remote)
                self.files(key, remote, plan)
            save()  # Establish journal writability before the first irreversible POST.
            for key in ORDER:
                remote = self.get(key)
                done = remote.get("state") == "done"
                self.identity(key, remote, done=done)
                self.metadata(key, remote)
                self.files(key, remote, plan)
                if not done:
                    self.uncertain.add(key)
                    save()  # POST may succeed even if its response is lost.
                    self.request("POST", f"{self.endpoint(key)}/actions/publish")
                    remote = self.get(key)
                    self.identity(key, remote, done=True)
                    self.metadata(key, remote)
                    self.files(key, remote, plan)
                save()
                unexpected = self.inventory(baseline | planned) - baseline - planned
                save()
                require(not unexpected, f"Unexpected new record(s): {', '.join(sorted(unexpected))}")
                self.emit(f"{key}: public and verified")
        except (Exception, KeyboardInterrupt) as error:
            unavailable = []
            for key in ORDER:
                try:
                    self.get(key)
                except Exception:
                    unavailable.append(key)
            try:
                if baseline is not None:
                    save(error)
            except Exception as state_error:
                self.emit(f"Could not save progress: {str(state_error) or type(state_error).__name__}")
            self.emit("Public records: " + canonical(self.public))
            if self.uncertain or unavailable:
                self.emit("Unable to confirm current state for: " + ", ".join(sorted(self.uncertain | set(unavailable)))
                          + "; rerun the same plan to reconcile")
            raise
        self.emit("Public records: " + canonical(self.public))

    def record(self):
        with _DirectoryLock(self.state_path().parent):
            self.record_locked()

    def record_locked(self):
        plan = self.plan()
        for key in ORDER:
            remote = self.get(key)
            self.identity(key, remote, done=True)
            require(remote["metadata"].get("version") == self.config["records"][key]["metadata"]["version"],
                    f"{key}: published version differs")
            remote = self.published(key)
            require(remote["metadata"].get("version") == self.config["records"][key]["metadata"]["version"],
                    f"{key}: records API published version differs")
            self.files(key, remote, plan, records_api=True)
            expected = {f["name"]: f for f in plan["records"][key]["files"]}
            for f in remote["files"]:
                data = self.content(remote["id"], f["key"]).body
                e = expected[f["key"]]
                require(len(data) == e["size"] and hashlib.sha256(data).hexdigest() == e["sha256"]
                        and hashlib.md5(data).hexdigest() == e["md5"] == checksum(f["checksum"]),
                        f"{key}: downloaded bytes differ for {f['key']}")
        target = local_path(self.repo, "specs/publication/DEPOSITED.sha256")
        before = target.read_bytes()
        existing = {}
        for line in before.decode("utf-8").splitlines():
            if not line.strip() or line.lstrip().startswith("#"):
                continue
            parts = line.split()
            require(len(parts) == 3 and re.fullmatch(r"[0-9a-f]{64}", parts[2]),
                    "Malformed DEPOSITED.sha256 entry")
            pair = tuple(parts[:2])
            require(pair not in existing or existing[pair] == parts[2], "Conflicting DEPOSITED.sha256 entries")
            existing[pair] = parts[2]
        additions = []
        for key in ORDER:
            hashes = {f["name"]: f["sha256"] for f in plan["records"][key]["files"]}
            for line in self.config["records"][key].get("deposit_lines", []):
                pair, sha = (line["name"], line["version"]), hashes[line["name"]]
                require(pair not in existing or existing[pair] == sha,
                        f"Conflicting deposited bytes for {pair[0]} {pair[1]}")
                if pair not in existing:
                    additions.append(f"{pair[0]}  {pair[1]}  {sha}\n")
                    existing[pair] = sha
        if additions:
            require(target.read_bytes() == before, "DEPOSITED.sha256 changed during verification")
            atomic_write(target, before.decode("utf-8") + ("\n" if before and not before.endswith(b"\n") else "") + "".join(additions))
        self.emit(f"All published bytes verified; recorded {len(additions)} new digest lines")

    def probe(self):
        """Check observed API contracts using GETs only; never plan or write state."""
        ident = self.args.reference_record
        require(ident and re.fullmatch(r"[1-9][0-9]*", ident),
                "Probe requires --reference-record with a positive record id")
        reference, failures = {}, []

        def check(label, action):
            try:
                action()
            except Exception as error:
                failures.append(label)
                message = " ".join((str(error) or type(error).__name__).split())
                self.emit(f"FAIL {label}: {message}")
            else:
                self.emit(f"PASS {label}")

        def deposit():
            r = self.request("GET", f"{self.config['api_base']}/deposit/depositions/{ident}")
            reference["deposit"] = r
            require(str(r.get("id")) == ident and r.get("state") == "done"
                    and r.get("doi") == f"10.5281/zenodo.{ident}"
                    and r.get("conceptdoi") == f"10.5281/zenodo.{r.get('conceptrecid')}"
                    and re.fullmatch(r"[1-9][0-9]*", str(r.get("conceptrecid"))),
                    "Invalid published deposit identity")
            require(isinstance(r.get("files"), list) and r["files"], "No reference files")
            for f in r["files"]:
                require(isinstance(f.get("checksum"), str)
                        and re.fullmatch(r"[0-9a-fA-F]{32}", f["checksum"]),
                        "Legacy checksum must be 32 hex digits without a prefix")
            self.file_map("reference", r)

        def records():
            r = self.request("GET", f"{self.config['api_base']}/records/{ident}")
            reference["records"] = r
            require(isinstance(r.get("files"), list) and r["files"], "No reference files")
            for f in r["files"]:
                require(isinstance(f.get("key"), str) and f["key"]
                        and isinstance(f.get("checksum"), str)
                        and f["checksum"].startswith("md5:"), "Invalid records API file shape")
            require(self.file_map("reference", r, records_api=True)
                    == self.file_map("reference", reference["deposit"]),
                    "Deposit and records API file sizes or MD5 differ")

        def content():
            name = reference["records"]["files"][0]["key"]
            result = self.content(ident, name, byte_range=True)
            require(len(result.body) == 16, "Range request did not return 16 bytes")

        check("reference deposit API", deposit)
        check("reference records API and matching MD5", records)
        check("reference content range", content)
        for key in self.config.get("records", {}):
            def draft(key=key):
                r = self.get(key)
                self.identity(key, r)
                require("doi" not in r and "conceptdoi" not in r,
                        "Draft unexpectedly has top-level DOI fields")
                self.checked_url(r["links"]["bucket"])
            check(f"draft {key}", draft)
        expected = {str(r["draft_id"]) for r in self.config.get("records", {}).values()}
        check("account inventory", lambda: self.inventory(expected))
        require(not failures, f"Probe failed: {len(failures)} check(s)")

    def execute(self):
        # This boundary prevents even an untrusted transport exception containing
        # credentials from escaping as an exception or chained traceback.
        try:
            mode = self.args.mode
            if self.args.records:
                self.load()
            else:
                require(mode == "probe", "--records is required for this mode")
                self.config = {"api_base": self.args.api_base}
                self.configure_api(self.config["api_base"])
            if mode == "plan":
                plan = self.plan()
                if self.args.json:
                    self.emit(canonical(plan))
                else:
                    self.emit(f"Commit: {plan['commit']}\nPlan digest: {plan['plan_digest']}")
                    for key in ORDER:
                        self.emit(f"{key}: " + canonical(plan["records"][key]))
                        self.emit("External URLs: " + canonical(plan["external_urls"][key]))
            elif mode == "status":
                for key in ORDER:
                    r = self.get(key)
                    self.emit(canonical({"record": key, "state": r.get("state"),
                                         "doi": r.get("doi"), "conceptdoi": r.get("conceptdoi"),
                                         "conceptrecid": r.get("conceptrecid"),
                                         "version": r.get("metadata", {}).get("version"),
                                         "files": [f["filename"] for f in r.get("files", [])]}))
            else:
                getattr(self, mode.replace("-", "_"))()
        except (Exception, KeyboardInterrupt) as error:
            raise Refusal(self.mask(str(error) or type(error).__name__)) from None


class _DirectoryLock:
    def __init__(self, path):
        self.path = path

    def __enter__(self):
        import fcntl
        self.fd = os.open(self.path, os.O_RDONLY | os.O_DIRECTORY)
        try:
            fcntl.flock(self.fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BaseException:
            os.close(self.fd)
            raise Refusal("Another publication is using this state directory") from None

    def __exit__(self, *args):
        os.close(self.fd)


class _SafeParser(argparse.ArgumentParser):
    def __init__(self, *args, mask_token=None, **kwargs):
        super().__init__(*args, **kwargs)
        self.mask_token = os.environ.get("ZENODO_TOKEN", "") if mask_token is None else mask_token

    def error(self, message):
        raise Refusal(message)

    def _print_message(self, message, file=None):
        super()._print_message(redact(message, self.mask_token), file)


def parser(mask_token=None):
    p = _SafeParser(description=__doc__, mask_token=mask_token, allow_abbrev=False)
    p.add_argument("mode", choices=["plan", "stage-metadata", "stage-files", "publish", "record", "status", "probe"])
    p.add_argument("--records")
    p.add_argument("--reference-record")
    p.add_argument("--api-base", default="https://zenodo.org/api",
                   help="Probe API base when --records is omitted")
    p.add_argument("--repo", default=".")
    p.add_argument("--token-env", default="ZENODO_TOKEN")
    p.add_argument("--json", action="store_true")
    p.add_argument("--plan-digest")
    p.add_argument("--publish-permanently", action="store_true")
    p.add_argument("--webhook-off-confirmed", action="store_true")
    p.add_argument("--state")
    return p


def main(argv=None, transport=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    # Know the chosen secret even if argument parsing itself fails.
    token_env = "ZENODO_TOKEN"
    for index, arg in enumerate(argv):
        if arg.startswith("--token-env="):
            token_env = arg.partition("=")[2]
        elif arg == "--token-env" and index + 1 < len(argv):
            token_env = argv[index + 1]
    token = os.environ.get(token_env, "")
    try:
        args = parser(mask_token=token).parse_args(argv)
        Deposit(args, transport).execute()
    except (Exception, KeyboardInterrupt) as error:
        message = str(error) or type(error).__name__
        print(redact("Refused: " + message, token), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
