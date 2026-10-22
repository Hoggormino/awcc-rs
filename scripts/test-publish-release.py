#!/usr/bin/env python3
"""Exercise the real publisher with isolated assets and a local gh replacement."""

import copy
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SOURCE_DIR = Path(__file__).resolve().parent
TAG = "v9.8.7"
VERSION = TAG[1:]
REPOSITORY = "fixture/awcc-rs"
ASSET_NAMES = [
    f"awcc-rs-{VERSION}-x86_64-unknown-linux-gnu.tar.gz",
    f"awcc-rs-{VERSION}-x86_64.AppImage",
    f"awcc-rs_{VERSION}_amd64.deb",
    f"awcc-rs-{VERSION}-1.x86_64.rpm",
    f"awcc-rs-{VERSION}-1-x86_64.pkg.tar.zst",
    "PKGBUILD",
    ".SRCINFO",
    "awcc-rs.install",
    "build-manifest.json",
    "SHA256SUMS",
]

MOCK_GH = r'''#!/usr/bin/env python3
import json
import os
from pathlib import Path
import shutil
import sys

root = Path(os.environ["AWCC_PUBLISH_FIXTURE"])
config = json.loads((root / "response.json").read_text())
args = sys.argv[1:]
with (root / "calls.jsonl").open("a") as log:
    log.write(json.dumps(args) + "\n")

if args[0] == "api":
    endpoint = next(arg for arg in args[1:] if arg.startswith("repos/"))
    if "/releases/tags/" in endpoint:
        response = config["lookup"]
        if response.get("transport_error"):
            print("fixture connection failure", file=sys.stderr)
            sys.exit(1)
        if "--include" in args:
            sys.stdout.write(f"HTTP/2.0 {response['status']} Fixture\nX-Fixture: true\r\n\r\n")
        body = response["body"]
        sys.stdout.write(body if isinstance(body, str) else json.dumps(body))
        if response["exit_code"]:
            print(f"gh: fixture error (HTTP {response['status']})", file=sys.stderr)
        sys.exit(response["exit_code"])
    if "/releases?" in endpoint:
        if config.get("list_error"):
            print("fixture draft discovery failure", file=sys.stderr)
            sys.exit(1)
        pages = config.get("pages", [[]])
        print(json.dumps(pages if "--slurp" in args else pages[0]))
        sys.exit(0)
elif args[:2] == ["release", "download"]:
    if not config.get("published"):
        print("release not found", file=sys.stderr)
        sys.exit(1)
    destination = Path(args[args.index("--dir") + 1])
    destination.mkdir(parents=True, exist_ok=True)
    (destination / "build-manifest.json").write_text(json.dumps(config["manifest"]))
    shutil.copyfile(root / "dist" / "SHA256SUMS", destination / "SHA256SUMS")
    sys.exit(0)
elif args[:2] == ["release", "view"]:
    release = config["uploaded_release"]
    print(json.dumps({"tagName": release["tag_name"], "isDraft": release["draft"], "assets": release["assets"]}))
    sys.exit(0)
elif args[:2] in (["release", "create"], ["release", "edit"], ["release", "upload"]):
    if args[1] == "upload" and config.get("upload_error"):
        print("fixture upload failure", file=sys.stderr)
        sys.exit(1)
    sys.exit(0)

print(f"Unexpected mock gh call: {args!r}", file=sys.stderr)
sys.exit(99)
'''


class PublishReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="awcc-publisher-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.bin_dir = self.root / "bin"
        self.bin_dir.mkdir()
        self.scripts = self.root / "scripts"
        self.scripts.mkdir()
        for name in ("publish-release.sh", "release-meta.sh"):
            shutil.copy2(SOURCE_DIR / name, self.scripts / name)
        (self.root / "Cargo.toml").write_text(
            f'[workspace]\nmembers = []\n\n[workspace.package]\nversion = "{VERSION}"\n'
        )
        for command, source in {
            "gh": MOCK_GH,
            "cargo": '#!/bin/sh\nprintf \'%s\\n\' \'{"packages": []}\'\n',
            "rustc": '#!/bin/sh\nprintf \'%s\\n\' \'rustc fixture\'\n',
        }.items():
            executable = self.bin_dir / command
            executable.write_text(source)
            executable.chmod(0o755)

        self.env = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith(("AWCC_", "GITHUB_", "GH_"))
        }
        self.env.update(
            PATH=f"{self.bin_dir}{os.pathsep}{os.environ['PATH']}",
            AWCC_PUBLISH_FIXTURE=str(self.root),
            AWCC_TARGET="x86_64-unknown-linux-gnu",
            AWCC_PKGREL="1",
        )
        self.git("init", "--quiet")
        self.git("add", "Cargo.toml")
        self.git("-c", "user.name=Publisher fixture", "-c", "user.email=fixture@example.invalid", "commit", "--quiet", "-m", "Fixture")
        self.commit = self.git("rev-parse", "HEAD").stdout.strip()
        self.dist = self.root / "dist"
        self.dist.mkdir()
        manifest_assets = []
        for name in ASSET_NAMES[:-2]:
            content = f"fixture asset {name}\n".encode()
            (self.dist / name).write_bytes(content)
            manifest_assets.append({"name": name, "size_bytes": len(content), "sha256": hashlib.sha256(content).hexdigest()})
        manifest = {"git_commit": self.commit, "version": VERSION, "assets": manifest_assets}
        (self.dist / "build-manifest.json").write_text(json.dumps(manifest))
        (self.dist / "SHA256SUMS").write_text("".join(
            f"{hashlib.sha256((self.dist / name).read_bytes()).hexdigest()}  {name}\n"
            for name in ASSET_NAMES[:-1]
        ))
        release = {"id": 123, "tag_name": TAG, "draft": True, "assets": [
            {"name": "default.SRCINFO" if name == ".SRCINFO" else name,
             "size": (self.dist / name).stat().st_size,
             "digest": f"sha256:{hashlib.sha256((self.dist / name).read_bytes()).hexdigest()}"}
            for name in ASSET_NAMES
        ]}
        self.config = {
            "lookup": {"status": 404, "exit_code": 1, "body": {"message": "Not Found", "status": "404"}},
            "pages": [[]],
            "uploaded_release": release,
            "manifest": manifest,
        }

    def git(self, *args):
        return subprocess.run(["git", "-C", str(self.root), *args], env=self.env, text=True, capture_output=True, check=True)

    def run_publisher(self, *args):
        (self.root / "response.json").write_text(json.dumps(self.config))
        (self.root / "calls.jsonl").unlink(missing_ok=True)
        result = subprocess.run(
            ["bash", str(self.scripts / "publish-release.sh"), "--tag", TAG,
             "--dist-dir", str(self.dist), "--repo", REPOSITORY, *args],
            cwd=self.root, env=self.env, text=True, capture_output=True, check=False, timeout=15,
        )
        call_log = self.root / "calls.jsonl"
        self.calls = [json.loads(line) for line in call_log.read_text().splitlines()] if call_log.exists() else []
        self.mutations = [call for call in self.calls if call[:2] in (
            ["release", "create"], ["release", "edit"], ["release", "upload"]
        )]
        return result

    def assert_success(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def assert_rejected(self, result):
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.mutations, [], self.calls)

    def use_published_release(self):
        release = copy.deepcopy(self.config["uploaded_release"])
        release["draft"] = False
        self.config["lookup"] = {"status": 200, "exit_code": 0, "body": release}
        self.config["published"] = True
        return release

    def test_first_release_handles_404_and_uploads_all_assets_before_publication(self):
        self.assert_success(self.run_publisher())
        self.assertEqual([call[1] for call in self.mutations], ["create", "upload", "edit"])
        self.assertIn("--verify-tag", self.mutations[0])
        upload_paths = {arg for arg in self.mutations[1] if arg.startswith(str(self.dist))}
        self.assertEqual(upload_paths, {str(self.dist / name) for name in ASSET_NAMES})
        self.assertIn("--draft=false", self.mutations[-1])
        for call in self.mutations:
            self.assertEqual(call[2], TAG)
            self.assertEqual(call[call.index("--repo") + 1], REPOSITORY)

    def test_http_404_does_not_require_status_in_error_body(self):
        self.config["lookup"]["body"] = {"message": "Not Found"}
        self.assert_success(self.run_publisher())

    def test_resume_draft_returned_by_tag_lookup(self):
        self.config["lookup"] = {"status": 200, "exit_code": 0, "body": self.config["uploaded_release"]}
        self.assert_success(self.run_publisher())
        self.assertEqual([call[1] for call in self.mutations], ["edit", "upload", "edit"])
        self.assertIn("--draft", self.mutations[0])

    def test_resume_draft_on_later_discovery_page(self):
        self.config["pages"] = [[], [self.config["uploaded_release"]]]
        self.assert_success(self.run_publisher())
        self.assertEqual([call[1] for call in self.mutations], ["edit", "upload", "edit"])

    def test_matching_published_release_is_a_read_only_retry(self):
        self.use_published_release()
        result = self.run_publisher()
        self.assert_success(result)
        self.assertIn("Safe retry complete", result.stdout)
        self.assertEqual(self.mutations, [])

    def test_published_manifest_conflicts_are_rejected(self):
        self.use_published_release()
        for key, value in (("git_commit", "0" * 40), ("version", "0.0.0")):
            with self.subTest(field=key):
                previous = self.config["manifest"][key]
                self.config["manifest"][key] = value
                self.assert_rejected(self.run_publisher())
                self.config["manifest"][key] = previous

    def test_incomplete_or_corrupt_published_assets_are_rejected(self):
        release = self.use_published_release()
        original_assets = copy.deepcopy(release["assets"])
        for defect in ("missing", "size", "digest"):
            with self.subTest(defect=defect):
                release["assets"] = copy.deepcopy(original_assets)
                if defect == "missing":
                    release["assets"].pop()
                elif defect == "size":
                    release["assets"][0]["size"] += 1
                else:
                    release["assets"][0]["digest"] = "sha256:" + "0" * 64
                self.assert_rejected(self.run_publisher())

    def test_api_errors_cannot_be_mistaken_for_missing_or_published_releases(self):
        for status in (401, 403, 429, 500):
            with self.subTest(status=status):
                # The HTTP status must take precedence over even a misleading error body.
                self.config["lookup"] = {"status": status, "exit_code": 1, "body": {"message": "fixture error", "status": "404"}}
                result = self.run_publisher()
                self.assert_rejected(result)
                self.assertIn(str(status), result.stderr)

    def test_transport_failure_aborts_before_any_release_mutation(self):
        self.config["lookup"]["transport_error"] = True
        result = self.run_publisher()
        self.assert_rejected(result)
        self.assertIn("fixture connection failure", result.stderr)

    def test_invalid_success_responses_are_rejected(self):
        release = self.config["uploaded_release"]
        invalid_bodies = [
            {}, [], "invalid JSON", {**release, "draft": "false"},
            {**release, "tag_name": "v0.0.0"}, {**release, "id": 0},
            {**release, "assets": {}},
        ]
        for body in invalid_bodies:
            with self.subTest(body=body):
                self.config["lookup"] = {"status": 200, "exit_code": 0, "body": body}
                self.assert_rejected(self.run_publisher())

    def test_inconsistent_http_and_command_status_are_rejected(self):
        for status, exit_code in ((200, 1), (404, 0), (201, 0)):
            with self.subTest(status=status, exit_code=exit_code):
                self.config["lookup"] = {"status": status, "exit_code": exit_code, "body": self.config["uploaded_release"]}
                self.assert_rejected(self.run_publisher())

    def test_draft_discovery_failure_aborts_without_creating_release(self):
        self.config["list_error"] = True
        self.assert_rejected(self.run_publisher())

    def test_upload_failure_cannot_publish_release(self):
        self.config["upload_error"] = True
        result = self.run_publisher()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual([call[1] for call in self.mutations], ["create", "upload"])

    def test_uploaded_asset_mismatch_prevents_promotion(self):
        self.config["uploaded_release"]["assets"][0]["digest"] = "sha256:" + "0" * 64
        result = self.run_publisher()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual([call[1] for call in self.mutations], ["create", "upload"])

    def test_dry_run_never_calls_github(self):
        self.assert_success(self.run_publisher("--dry-run"))
        self.assertEqual(self.calls, [])

    def test_checksum_failure_aborts_before_github_lookup(self):
        (self.dist / ASSET_NAMES[0]).write_text("corrupted fixture asset")
        self.assert_rejected(self.run_publisher())
        self.assertEqual(self.calls, [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
