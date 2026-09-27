import copy
import io
import json
import os
import tempfile
import unittest
import urllib.error
from pathlib import Path
from unittest import mock

from scripts import gx_release as release

SHA = "a" * 40
OTHER_SHA = "b" * 40
VERSION = release.cargo_version()
MANUAL_ENV = {
    "GITHUB_ACTIONS": "true",
    "GITHUB_EVENT_NAME": "workflow_dispatch",
    "GITHUB_REPOSITORY": release.REPOSITORY,
    "GITHUB_WORKFLOW_REF": f"{release.REPOSITORY}/.github/workflows/gx-release.yml@refs/heads/master",
    "GX_PUBLISH_REQUESTED": "true",
    "GITHUB_ACTOR": "owner",
    "GITHUB_TRIGGERING_ACTOR": "owner",
    "GH_TOKEN": "test-token",
}


def remote_asset(path):
    return {"name": path.name, "size": path.stat().st_size,
            "digest": "sha256:" + release.digest(path), "state": "uploaded"}


class FakeGitHub:
    def __init__(self, folder):
        self.folder = folder
        self.commit = None
        self.release = None
        self.assets = []
        self.calls = []
        self.fail_upload = None
        self.fail_patch = False
        self.permission = "admin"
        self.change_tag = False

    def optional(self, path):
        if path.startswith("/git/ref/tags/"):
            return {"object": {"type": "commit", "sha": self.commit}} if self.commit else None
        raise AssertionError(path)

    def request(self, method, path, data=None, binary=None):
        self.calls.append((method, path, data, binary.name if binary else None))
        if method == "GET" and path.startswith("/collaborators/"):
            return {"permission": self.permission}
        if method == "GET" and path.startswith("/releases?per_page="):
            return [copy.deepcopy(self.release)] if self.release else []
        if method == "POST" and path == "/git/refs":
            self.commit = data["sha"]
            return {"object": {"sha": self.commit}}
        if method == "POST" and path == "/releases":
            self.release = {**data, "id": 42}
            return copy.deepcopy(self.release)
        if method == "GET" and path == "/releases/42/assets?per_page=100":
            if self.change_tag and len(self.assets) == 4:
                self.commit = OTHER_SHA
            return copy.deepcopy(self.assets)
        if method == "POST" and path.startswith("https://uploads.github.com/"):
            if binary.name == self.fail_upload:
                raise TimeoutError("simulated uncertain upload")
            self.assets.append(remote_asset(binary))
            return self.assets[-1]
        if method == "GET" and path == "/releases/42":
            return copy.deepcopy(self.release)
        if method == "PATCH" and path == "/releases/42":
            if self.fail_patch:
                raise TimeoutError("simulated uncertain publication")
            self.release.update(data)
            return copy.deepcopy(self.release)
        raise AssertionError((method, path))


class ArtifactFixture(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.folder = Path(self.tmp.name)
        self.source = mock.patch.object(release, "source_info", return_value=(SHA, False)).start()
        self.addCleanup(mock.patch.stopall)
        mock.patch.dict(os.environ, {}, clear=True).start()
        mock.patch.object(release, "open_api", side_effect=AssertionError("real network forbidden")).start()
        self.infos = {}
        for platform, (target, manager) in release.TARGETS.items():
            name = release.artifact_name(VERSION, platform)
            artifact = self.folder / name
            artifact.write_bytes(b"test artifact " + platform.encode())
            files = {path: value or "c" * 64 for path, value in release.expected_payload(platform).items()}
            info = {
                "schema_version": 1, "version": VERSION, "source_commit": SHA,
                "source_dirty": False, "platform": platform, "architecture": "x86_64",
                "target": target, "package_manager": manager,
                "binary": {"sha256": "c" * 64, "version_output": f"herdr {VERSION}-gx.{manager}.{SHA}"},
                "artifact": {"name": name, "size": artifact.stat().st_size, "sha256": release.digest(artifact)},
                "files": files,
            }
            self.infos[platform] = info
            self.save_info(platform)

    def save_info(self, platform):
        artifact = self.folder / release.artifact_name(VERSION, platform)
        metadata = self.folder / (artifact.name + ".manifest.json")
        metadata.write_text(json.dumps(self.infos[platform]), encoding="utf-8")
        self.save_checksum(artifact)

    def save_checksum(self, artifact):
        metadata = self.folder / (artifact.name + ".manifest.json")
        (self.folder / (artifact.name + ".sha256")).write_text(
            f"{release.digest(artifact)}  {artifact.name}\n{release.digest(metadata)}  {metadata.name}\n", encoding="ascii"
        )

    def verify(self):
        return release.verify_artifacts(self.folder, VERSION, SHA)


class ArtifactTests(ArtifactFixture):
    def test_normal_pair_generates_exactly_four_release_files_and_is_repeatable(self):
        paths = self.verify()
        self.assertEqual([path.name for path in paths], [
            release.artifact_name(VERSION, "windows"), release.artifact_name(VERSION, "linux"),
            "manifest.json", "SHA256SUMS",
        ])
        manifest = json.loads((self.folder / "manifest.json").read_text())
        self.assertEqual(manifest["repository"], "gx0404/herdr")
        self.assertEqual(manifest["source_commit"], SHA)
        self.assertIs(manifest["source_dirty"], False)
        self.assertEqual(manifest["packages"], list(self.infos.values()))
        self.assertEqual(len((self.folder / "SHA256SUMS").read_text().splitlines()), 3)
        self.assertEqual(paths, self.verify())

    def test_shared_packager_sidecars_pass_real_verify_entrypoint(self):
        from scripts import gx_package

        for platform, info in self.infos.items():
            artifact = self.folder / release.artifact_name(VERSION, platform)
            gx_package.write_manifest(artifact, platform, VERSION, SHA, False, info["binary"], info["files"])
        self.assertEqual(release.main([
            "verify", "--version", VERSION, "--sha", SHA, "--artifacts", str(self.folder),
        ]), 0)
        self.assertTrue((self.folder / "manifest.json").is_file())
        self.assertTrue((self.folder / "SHA256SUMS").is_file())

    def test_rejects_dirty_or_different_checkout_and_short_sha(self):
        for source, sha in [((SHA, True), SHA), ((OTHER_SHA, False), SHA), ((SHA, False), SHA[:12])]:
            with self.subTest(source=source, sha=sha), mock.patch.object(release, "source_info", return_value=source):
                with self.assertRaisesRegex(ValueError, "clean checkout"):
                    release.verify_artifacts(self.folder, VERSION, sha)

    def test_rejects_version_not_from_cargo(self):
        with self.assertRaisesRegex(ValueError, "Cargo.toml"):
            release.verify_artifacts(self.folder, "9999.0.0", SHA)

    def test_rejects_manifest_identity_and_schema_mismatch(self):
        original = copy.deepcopy(self.infos["linux"])
        for key, value in {
            "schema_version": True, "version": "9999.0.0", "source_commit": OTHER_SHA,
            "source_dirty": True, "platform": "windows", "architecture": "aarch64",
            "target": "x86_64-unknown-linux-gnu", "package_manager": "windows-installer",
        }.items():
            with self.subTest(key=key):
                self.infos["linux"] = {**copy.deepcopy(original), key: value}
                self.save_info("linux")
                with self.assertRaisesRegex(ValueError, key):
                    self.verify()
        self.infos["linux"] = {**original, "extra": "not allowed"}
        self.save_info("linux")
        with self.assertRaisesRegex(ValueError, "field set"):
            self.verify()

    def test_rejects_upstream_or_stale_binary_identity(self):
        for version_output in [f"herdr {VERSION}", f"herdr {VERSION}-gx.deb.{OTHER_SHA}",
                               f"herdr {VERSION}-gx.deb.{SHA[:12]}", f"herdr {VERSION}-gx.deb.{SHA}\n"]:
            with self.subTest(output=version_output):
                self.infos["linux"]["binary"]["version_output"] = version_output
                self.save_info("linux")
                with self.assertRaisesRegex(ValueError, "binary identity"):
                    self.verify()

    def test_rejects_missing_unexpected_or_invalid_payload_entries(self):
        original = copy.deepcopy(self.infos["windows"])
        variants = [dict(original["files"]), dict(original["files"]), dict(original["files"])]
        variants[0].pop("conpty/conpty.dll")
        variants[1]["../outside"] = "d" * 64
        variants[2]["herdr.exe"] = "bad hash"
        for files in variants:
            with self.subTest(files=files):
                self.infos["windows"] = {**copy.deepcopy(original), "files": files}
                self.save_info("windows")
                with self.assertRaisesRegex(ValueError, "payload file set"):
                    self.verify()

    def test_rejects_modified_pinned_conpty_license_or_marker(self):
        original = copy.deepcopy(self.infos["windows"])
        for path in ("conpty/conpty.dll", "LICENSE", "conpty/herdr-conpty.json"):
            with self.subTest(path=path):
                self.infos["windows"] = copy.deepcopy(original)
                self.infos["windows"]["files"][path] = "d" * 64
                self.save_info("windows")
                with self.assertRaisesRegex(ValueError, "payload digest mismatch"):
                    self.verify()

    def test_rejects_binary_payload_digest_disagreement(self):
        self.infos["linux"]["files"]["usr/bin/herdr"] = "d" * 64
        self.save_info("linux")
        with self.assertRaisesRegex(ValueError, "binary and payload"):
            self.verify()

    def test_rejects_wrong_artifact_size_or_digest(self):
        original = copy.deepcopy(self.infos["linux"])
        for key, value in [("size", 1), ("sha256", "e" * 64), ("name", "another.deb")]:
            with self.subTest(key=key):
                self.infos["linux"] = copy.deepcopy(original)
                self.infos["linux"]["artifact"][key] = value
                self.save_info("linux")
                with self.assertRaisesRegex(ValueError, "artifact size/SHA-256"):
                    self.verify()

    def test_rejects_changed_artifact_and_sidecar_checksums(self):
        artifact = self.folder / release.artifact_name(VERSION, "linux")
        artifact.write_bytes(b"changed")
        with self.assertRaisesRegex(ValueError, "artifact size/SHA-256"):
            self.verify()
        self.infos["linux"]["artifact"].update(size=artifact.stat().st_size, sha256=release.digest(artifact))
        self.save_info("linux")
        (self.folder / (artifact.name + ".sha256")).write_text("wrong\n", encoding="ascii")
        with self.assertRaisesRegex(ValueError, "sidecar checksum"):
            self.verify()

    def test_rejects_missing_empty_extra_and_symlinked_files(self):
        artifact = self.folder / release.artifact_name(VERSION, "linux")
        for kind in ("missing", "empty", "extra", "symlink"):
            with self.subTest(kind=kind):
                if kind == "missing":
                    artifact.rename(self.folder / "moved")
                elif kind == "empty":
                    artifact.write_bytes(b"")
                elif kind == "extra":
                    (self.folder / "extra").write_text("not an asset")
                else:
                    with mock.patch.object(Path, "is_symlink", side_effect=lambda: True):
                        with self.assertRaises(ValueError):
                            self.verify()
                    continue
                with self.assertRaises(ValueError):
                    self.verify()
                if kind == "missing":
                    (self.folder / "moved").rename(artifact)
                elif kind == "empty":
                    artifact.write_bytes(b"test artifact linux")
                else:
                    (self.folder / "extra").unlink()

    def test_rejects_duplicate_json_keys(self):
        artifact = self.folder / release.artifact_name(VERSION, "linux")
        metadata = self.folder / (artifact.name + ".manifest.json")
        metadata.write_text('{"version": "a", "version": "b"}', encoding="utf-8")
        self.save_checksum(artifact)
        with self.assertRaisesRegex(ValueError, "duplicate JSON"):
            self.verify()

    def test_refuses_overwriting_different_unified_metadata(self):
        self.verify()
        (self.folder / "manifest.json").write_text("{}", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "refusing to overwrite"):
            self.verify()

    def test_prepare_build_only_is_read_only_and_emits_full_sha_and_pins(self):
        output = self.folder / "outputs"
        with mock.patch.dict(os.environ, {"GITHUB_OUTPUT": str(output)}), mock.patch.object(release, "GitHub") as api:
            release.prepare(False, None)
        api.assert_not_called()
        self.assertIn(f"sha={SHA}\n", output.read_text())
        self.assertIn(f"version={VERSION}\n", output.read_text())
        self.assertIn("rust=1.96.1\nzig=0.16.0\n", output.read_text())

    def test_prepare_rejects_dirty_source(self):
        with mock.patch.object(release, "source_info", return_value=(SHA, True)):
            with self.assertRaisesRegex(ValueError, "clean checkout"):
                release.prepare(False, None)

    def test_toolchain_pins_match_sources(self):
        self.assertEqual(release.toolchain_versions(), ("1.96.1", "0.16.0"))
        with mock.patch.object(release, "RUST_VERSION", "1.1.0"):
            with self.assertRaisesRegex(ValueError, "pins"):
                release.toolchain_versions()


class PublishTests(ArtifactFixture):
    def setUp(self):
        super().setUp()
        mock.patch.dict(os.environ, MANUAL_ENV).start()
        self.api = FakeGitHub(self.folder)
        self.api_factory = mock.patch.object(release, "GitHub", return_value=self.api).start()

    def publish(self):
        release.publish(self.folder, VERSION, SHA, release.REPOSITORY)

    def draft(self):
        files = self.verify()
        self.api.commit = SHA
        self.api.release = {
            "id": 42, "draft": True, "target_commitish": SHA, "tag_name": f"gx-v{VERSION}",
            "body": release.release_body(VERSION, SHA, self.folder / "manifest.json"),
        }
        return files

    def test_publish_creates_draft_verifies_all_four_assets_then_publishes(self):
        self.publish()
        self.assertEqual(self.api.commit, SHA)
        self.assertFalse(self.api.release["draft"])
        self.assertEqual(len(self.api.assets), 4)
        mutations = [(method, path) for method, path, _, _ in self.api.calls if method != "GET"]
        self.assertEqual(mutations[:2], [("POST", "/git/refs"), ("POST", "/releases")])
        self.assertEqual(mutations[-1], ("PATCH", "/releases/42"))
        self.assertEqual(sum(method == "PATCH" for method, _ in mutations), 1)
        self.assertFalse(any(asset["name"].endswith(".manifest.json") for asset in self.api.assets))

    def test_guard_rejects_local_wrong_repo_event_workflow_or_implicit_publish(self):
        for key, value in [("GITHUB_ACTIONS", "false"), ("GITHUB_EVENT_NAME", "push"),
                           ("GITHUB_REPOSITORY", "herdrdev/herdr"), ("GX_PUBLISH_REQUESTED", "false"),
                           ("GITHUB_WORKFLOW_REF", "gx0404/herdr/.github/workflows/ci.yml@refs/heads/master")]:
            with self.subTest(key=key), mock.patch.dict(os.environ, {key: value}):
                with self.assertRaisesRegex(ValueError, "explicit manual"):
                    self.publish()
        with self.assertRaisesRegex(ValueError, "explicit manual"):
            release.publish(self.folder, VERSION, SHA, None)
        self.api_factory.assert_not_called()

    def test_non_admin_and_missing_rerun_actor_cannot_mutate(self):
        self.api.permission = "write"
        with self.assertRaisesRegex(ValueError, "admin"):
            self.publish()
        self.assertFalse(any(method != "GET" for method, _, _, _ in self.api.calls))
        with mock.patch.dict(os.environ, {"GITHUB_TRIGGERING_ACTOR": ""}):
            self.api.permission = "admin"
            with self.assertRaisesRegex(ValueError, "both triggering actors"):
                self.publish()

    def test_never_moves_existing_tag_or_overwrites_published_release(self):
        self.api.commit = OTHER_SHA
        with self.assertRaisesRegex(ValueError, "will not be moved"):
            self.publish()
        self.draft()
        self.api.release["draft"] = False
        with self.assertRaisesRegex(ValueError, "already published"):
            self.publish()
        self.assertFalse(any(method != "GET" for method, _, _, _ in self.api.calls))

    def test_resumes_only_matching_partial_draft_and_does_not_reupload(self):
        files = self.draft()
        self.api.assets = [remote_asset(files[0])]
        self.publish()
        uploads = [name for method, _, _, name in self.api.calls if method == "POST" and name]
        self.assertEqual(uploads, [path.name for path in files[1:]])
        self.assertFalse(any(path == "/git/refs" for _, path, _, _ in self.api.calls))

    def test_rejects_draft_with_different_source_or_manifest_even_without_assets(self):
        self.draft()
        for field, value in [("target_commitish", OTHER_SHA), ("body", "other source and hashes")]:
            with self.subTest(field=field):
                original = self.api.release[field]
                self.api.release[field] = value
                with self.assertRaises(ValueError):
                    self.publish()
                self.api.release[field] = original
        self.api.commit = None
        with self.assertRaisesRegex(ValueError, "immutable source tag"):
            self.publish()
        self.assertFalse(any(method != "GET" for method, _, _, _ in self.api.calls))

    def test_rejects_bad_remote_digest_size_state_and_unexpected_assets(self):
        files = self.draft()
        good = remote_asset(files[0])
        for asset in [{**good, "digest": "sha256:" + "0" * 64}, {**good, "size": 1},
                      {**good, "state": "starter"}, {**good, "name": "extra.txt"}]:
            with self.subTest(asset=asset):
                self.api.assets = [asset]
                with self.assertRaises(ValueError):
                    self.publish()
        self.assertFalse(any(method != "GET" for method, _, _, _ in self.api.calls))

    def test_rejects_duplicate_or_incomplete_remote_file_set(self):
        files = self.draft()
        with self.assertRaisesRegex(ValueError, "duplicated"):
            release.verify_remote_assets([remote_asset(files[0])] * 2, files, complete=False)
        with self.assertRaisesRegex(ValueError, "missing"):
            release.verify_remote_assets([remote_asset(files[0])], files, complete=True)
        self.assertEqual(release.verify_remote_assets([], files, complete=False), set())

    def test_uncertain_upload_leaves_draft_and_can_resume_identical_assets(self):
        self.api.fail_upload = "manifest.json"
        with self.assertRaises(TimeoutError):
            self.publish()
        self.assertTrue(self.api.release["draft"])
        self.assertEqual(len(self.api.assets), 2)
        self.assertFalse(any(method == "PATCH" for method, _, _, _ in self.api.calls))
        self.api.fail_upload = None
        self.publish()
        self.assertFalse(self.api.release["draft"])
        self.assertEqual(len(self.api.assets), 4)

    def test_tag_changed_after_upload_prevents_publication(self):
        self.api.change_tag = True
        with self.assertRaisesRegex(ValueError, "tag changed"):
            self.publish()
        self.assertTrue(self.api.release["draft"])
        self.assertFalse(any(method == "PATCH" for method, _, _, _ in self.api.calls))

    def test_uncertain_publication_is_not_blindly_retried(self):
        self.api.fail_patch = True
        with self.assertRaises(TimeoutError):
            self.publish()
        self.assertEqual(sum(method == "PATCH" for method, _, _, _ in self.api.calls), 1)

    def test_tag_rules_error_is_not_bypassed(self):
        original = self.api.request
        error = urllib.error.HTTPError("/git/refs", 403, "tag rules", {}, io.BytesIO())
        self.addCleanup(error.close)

        def denied(method, path, data=None, binary=None):
            if method == "POST" and path == "/git/refs":
                raise error
            return original(method, path, data, binary)

        self.api.request = denied
        with self.assertRaises(urllib.error.HTTPError):
            self.publish()
        self.assertIsNone(self.api.release)


class PreviousTests(ArtifactFixture):
    def setUp(self):
        super().setUp()
        self.old_version = "0.0.1"
        self.old_tag = "gx-v" + self.old_version
        self.output = self.folder / "previous"
        self.record = {"id": 73, "name": "Herdr GX " + self.old_version, "tag_name": self.old_tag,
                       "draft": False, "prerelease": False, "target_commitish": OTHER_SHA}
        self.listing = [self.record]
        self.commit = OTHER_SHA
        self.old_packages = []
        self.data = {}
        for platform, info in self.infos.items():
            old = copy.deepcopy(info)
            name = release.artifact_name(self.old_version, platform)
            body = ("old package " + platform).encode()
            self.data[name] = body
            old.update(version=self.old_version, source_commit=OTHER_SHA)
            old["binary"]["version_output"] = f"herdr {self.old_version}-gx.{old['package_manager']}.{OTHER_SHA}"
            old["artifact"] = {"name": name, "size": len(body), "sha256": release.hashlib.sha256(body).hexdigest()}
            old["files"] = {path: "f" * 64 for path in old["files"]}
            old["files"]["herdr.exe" if platform == "windows" else "usr/bin/herdr"] = old["binary"]["sha256"]
            self.old_packages.append(old)
        self.manifest = {"schema_version": 1, "repository": release.REPOSITORY, "version": self.old_version,
                         "source_commit": OTHER_SHA, "source_dirty": False, "packages": self.old_packages}
        self.refresh_remote()
        self.api = mock.Mock()
        self.api.request.side_effect = self.request
        self.api.optional.side_effect = lambda path: {"object": {"type": "commit", "sha": self.commit}} if self.commit else None
        self.api.download.side_effect = self.download
        mock.patch.object(release, "GitHub", return_value=self.api).start()

    def refresh_remote(self):
        self.data["manifest.json"] = (json.dumps(self.manifest) + "\n").encode()
        self.data["SHA256SUMS"] = "".join(
            f"{release.hashlib.sha256(self.data[name]).hexdigest()}  {name}\n"
            for name in [release.artifact_name(self.old_version, platform) for platform in release.TARGETS] + ["manifest.json"]
        ).encode()
        self.refresh_assets()

    def refresh_assets(self):
        self.assets = []
        for index, (name, body) in enumerate(self.data.items(), 100):
            self.assets.append({"id": index, "name": name, "state": "uploaded", "size": len(body),
                                "digest": "sha256:" + release.hashlib.sha256(body).hexdigest(),
                                "url": f"https://api.github.com/repos/{release.REPOSITORY}/releases/assets/{index}",
                                "browser_download_url": f"https://github.com/{release.REPOSITORY}/releases/download/{self.old_tag}/{name}"})

    def request(self, method, path):
        self.assertEqual(method, "GET")
        if path == "/releases?per_page=100&page=1":
            return copy.deepcopy(self.listing)
        if path in ("/releases/73", "/releases/tags/" + self.old_tag):
            return copy.deepcopy(self.record)
        if path == "/releases/73/assets?per_page=100":
            return copy.deepcopy(self.assets)
        raise AssertionError(path)

    def download(self, asset, destination):
        destination.write_bytes(self.data[asset["name"]])

    def previous(self, tag=""):
        return release.previous_packages(self.output, VERSION, SHA, release.REPOSITORY, tag)

    def test_auto_baseline_is_pinned_and_old_conpty_hashes_are_not_current_pins(self):
        with mock.patch.object(release, "expected_payload", side_effect=AssertionError("must not use current pins")):
            result = self.previous()
        self.assertEqual(result["sha"], OTHER_SHA)
        self.assertEqual(result["tag"], self.old_tag)
        self.assertEqual(result["available"], "true")
        self.assertEqual(len(list(self.output.iterdir())), 8)
        for package in self.old_packages:
            metadata = self.output / (package["artifact"]["name"] + ".manifest.json")
            self.assertEqual(json.loads(metadata.read_text()), package)
            artifact = self.output / package["artifact"]["name"]
            expected = f"{release.digest(artifact)}  {artifact.name}\n{release.digest(metadata)}  {metadata.name}\n"
            self.assertEqual((self.output / (artifact.name + ".sha256")).read_text(), expected)
        self.assertTrue(all(call.args[0] == "GET" for call in self.api.request.call_args_list))

    def test_explicit_previous_uses_cli_and_never_silently_falls_back(self):
        self.assertEqual(release.main(["previous", "--repository", release.REPOSITORY, "--version", VERSION,
                                       "--sha", SHA, "--previous-tag", self.old_tag, "--artifacts", str(self.output)]), 0)
        self.assertNotIn(mock.call("GET", "/releases?per_page=100&page=1"), self.api.request.call_args_list)

    def test_no_older_published_release_is_the_only_automatic_na(self):
        self.listing = [{**self.record, "draft": True},
                        {**self.record, "tag_name": "gx-v" + VERSION}, {"tag_name": "v0.0.1", "draft": False}]
        result = self.previous()
        self.assertEqual(result["available"], "false")
        self.assertFalse(self.output.exists())
        self.api.download.assert_not_called()

    def test_semantic_selection_chooses_greatest_older_cargo_version(self):
        self.listing = [{**self.record, "tag_name": "gx-v0.9.9", "name": "Herdr GX 0.9.9"},
                        {**self.record, "tag_name": "gx-v0.10.0", "name": "Herdr GX 0.10.0"}]
        self.assertEqual(release.select_previous(self.api, "1.0.0", "")["tag_name"], "gx-v0.10.0")
        self.assertEqual(release.select_previous(self.api, "0.10.0", "")["tag_name"], "gx-v0.9.9")

    def test_explicit_missing_draft_equal_newer_or_invalid_tags_fail(self):
        for tag in ("v0.0.1", "gx-v" + VERSION, "gx-v9999.0.0", "gx-v0.00.1", "gx-v0.0.1/../../other"):
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                self.previous(tag)
        self.record["draft"] = True
        with self.assertRaises(ValueError):
            self.previous(self.old_tag)
        error = urllib.error.HTTPError("url", 404, "missing", {}, io.BytesIO())
        self.addCleanup(error.close)
        self.api.request.side_effect = error
        with self.assertRaises(urllib.error.HTTPError):
            self.previous(self.old_tag)
        self.assertFalse(self.output.exists())

    def test_bad_public_release_never_becomes_na_or_falls_back(self):
        for field, value in [("name", "Other package"), ("draft", None), ("target_commitish", SHA[:12]),
                             ("prerelease", True), ("tag_name", "gx-vgarbage")]:
            with self.subTest(field=field):
                self.listing = [{**self.record, field: value}]
                with self.assertRaises(ValueError):
                    self.previous()
        self.listing = [self.record, self.record]
        with self.assertRaisesRegex(ValueError, "duplicate"):
            self.previous()

    def test_repository_and_source_mismatch_fail(self):
        with self.assertRaisesRegex(ValueError, "repository"):
            release.previous_packages(self.output, VERSION, SHA, "other/herdr")
        self.commit = SHA
        with self.assertRaisesRegex(ValueError, "tag/source"):
            self.previous()
        self.commit = None
        with self.assertRaisesRegex(ValueError, "tag/source"):
            self.previous()
        self.assertFalse(self.output.exists())

    def test_invalid_asset_names_state_size_hash_or_cross_repo_url_fail_before_download(self):
        good = copy.deepcopy(self.assets)
        for field, value in [("name", "unknown.exe"), ("state", "starter"), ("size", 0),
                             ("digest", "sha256:bad"), ("url", "https://api.github.com/repos/other/herdr/releases/assets/100"),
                             ("browser_download_url", "https://evil.invalid/installer")]:
            with self.subTest(field=field):
                self.assets = copy.deepcopy(good)
                self.assets[0][field] = value
                with self.assertRaises(ValueError):
                    self.previous()
        self.assets = good[:-1]
        with self.assertRaisesRegex(ValueError, "four assets"):
            self.previous()
        self.api.download.assert_not_called()

    def test_corrupt_download_and_missing_side_of_pair_fail_without_output(self):
        name = self.assets[0]["name"]
        self.data[name] += b"corrupt"
        with self.assertRaisesRegex(ValueError, "state/size/SHA-256"):
            self.previous()
        self.assertFalse(self.output.exists())
        self.old_packages.pop()
        self.refresh_remote()
        with self.assertRaisesRegex(ValueError, "both platform"):
            self.previous()

    def test_unified_schema_version_dirty_source_or_repository_mismatch_fail(self):
        original = copy.deepcopy(self.manifest)
        for field, value in [("schema_version", True), ("source_dirty", True), ("source_commit", SHA),
                             ("repository", "other/herdr"), ("version", VERSION)]:
            with self.subTest(field=field):
                self.manifest = {**copy.deepcopy(original), field: value}
                self.refresh_remote()
                with self.assertRaisesRegex(ValueError, "unified manifest"):
                    self.previous()
        self.assertFalse(self.output.exists())

    def test_unsafe_historical_payload_and_bad_identity_fail(self):
        self.old_packages[0]["files"]["../escape"] = "e" * 64
        self.refresh_remote()
        with self.assertRaisesRegex(ValueError, "unsafe previous"):
            self.previous()
        self.old_packages[0]["files"].pop("../escape")
        self.old_packages[0]["binary"]["version_output"] = "herdr upstream"
        self.refresh_remote()
        with self.assertRaisesRegex(ValueError, "binary identity"):
            self.previous()

    def test_checksums_are_verified_even_with_matching_server_digest(self):
        self.data["SHA256SUMS"] = b"wrong sums\n"
        self.refresh_assets()
        with self.assertRaisesRegex(ValueError, "SHA256SUMS"):
            self.previous()
        self.assertFalse(self.output.exists())

    def test_remote_tag_change_during_download_fails(self):
        def changed(asset, destination):
            self.download(asset, destination)
            self.commit = SHA

        self.api.download.side_effect = changed
        with self.assertRaisesRegex(ValueError, "source changed"):
            self.previous()
        self.assertFalse(self.output.exists())

    def test_download_failure_is_not_na_and_does_not_leave_partial_output(self):
        self.api.download.side_effect = TimeoutError("network unavailable")
        with self.assertRaises(TimeoutError):
            self.previous()
        self.assertFalse(self.output.exists())


class RequestTests(unittest.TestCase):
    def setUp(self):
        self.env = mock.patch.dict(os.environ, MANUAL_ENV, clear=True)
        self.env.start()
        self.addCleanup(self.env.stop)
        self.api = release.GitHub()

    def test_read_retries_transient_failure_only(self):
        with mock.patch.object(release, "open_api", side_effect=[
            urllib.error.HTTPError("url", 503, "temporary", {}, None), io.BytesIO(b'{"ok":true}'),
        ]) as request, mock.patch.object(release.time, "sleep"):
            self.assertEqual(self.api.request("GET", "/releases"), {"ok": True})
            self.assertEqual(request.call_count, 2)
        with mock.patch.object(release, "open_api", side_effect=urllib.error.HTTPError(
            "url", 403, "forbidden", {}, None
        )) as request:
            with self.assertRaises(urllib.error.HTTPError):
                self.api.request("GET", "/releases")
            self.assertEqual(request.call_count, 1)

    def test_writes_never_retry_timeouts_or_server_errors(self):
        for method in ("POST", "PATCH"):
            for error in (TimeoutError(), urllib.error.HTTPError("url", 503, "temporary", {}, None)):
                with self.subTest(method=method, error=error), mock.patch.object(
                    release, "open_api", side_effect=error
                ) as request:
                    with self.assertRaises(type(error)):
                        self.api.request(method, "/releases", {})
                    self.assertEqual(request.call_count, 1)

    def test_optional_accepts_only_not_found(self):
        for status in (404, 401, 403):
            with self.subTest(status=status), mock.patch.object(
                self.api, "request", side_effect=urllib.error.HTTPError("url", status, "error", {}, None)
            ):
                if status == 404:
                    self.assertIsNone(self.api.optional("/git/ref/tags/test"))
                else:
                    with self.assertRaises(urllib.error.HTTPError):
                        self.api.optional("/git/ref/tags/test")

    def test_never_sends_token_to_arbitrary_or_other_repo_upload_url(self):
        with mock.patch.object(release, "open_api") as request:
            for url in ("https://evil.example/upload", "https://uploads.github.com/repos/other/herdr/releases/1/assets",
                        "//evil.example", "http://uploads.github.com/repos/gx0404/herdr/releases/1/assets"):
                with self.subTest(url=url), self.assertRaises(ValueError):
                    self.api.request("POST", url)
            request.assert_not_called()

    def test_annotated_tag_resolution_is_bounded_and_validates_sha(self):
        api = mock.Mock()
        api.optional.return_value = {"object": {"type": "tag", "sha": OTHER_SHA}}
        api.request.return_value = {"object": {"type": "commit", "sha": SHA}}
        self.assertEqual(release.remote_commit(api, "gx-v1.0.0"), SHA)
        api.request.return_value = {"object": {"type": "tag", "sha": OTHER_SHA}}
        with self.assertRaisesRegex(ValueError, "valid commit"):
            release.remote_commit(api, "gx-v1.0.0")
        api.optional.return_value = {"object": {"type": "commit", "sha": "../not-a-sha"}}
        with self.assertRaisesRegex(ValueError, "valid commit"):
            release.remote_commit(api, "gx-v1.0.0")


class DownloadTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.destination = Path(self.tmp.name) / "package.exe"
        self.body = b"verified old asset"
        self.asset = {"id": 123, "size": len(self.body), "digest": "sha256:" + release.hashlib.sha256(self.body).hexdigest()}
        patch = mock.patch.dict(os.environ, {"GH_TOKEN": "test-token"}, clear=True)
        patch.start()
        self.addCleanup(patch.stop)
        self.api = release.GitHub()

    def test_download_uses_fixed_repository_asset_id_and_verifies_bytes(self):
        with mock.patch.object(release, "open_download", return_value=io.BytesIO(self.body)) as request:
            self.api.download(self.asset, self.destination)
        self.assertEqual(self.destination.read_bytes(), self.body)
        sent = request.call_args.args[0]
        self.assertEqual(sent.full_url, "https://api.github.com/repos/gx0404/herdr/releases/assets/123")
        self.assertEqual(sent.get_method(), "GET")
        self.assertEqual(sent.get_header("Accept"), "application/octet-stream")

    def test_download_rejects_wrong_hash_short_and_oversized_body(self):
        for data in (b"x" * len(self.body), b"short", self.body + b"extra"):
            with self.subTest(data=data), mock.patch.object(release, "open_download", return_value=io.BytesIO(data)):
                with self.assertRaises(ValueError):
                    self.api.download(self.asset, self.destination)
                self.assertFalse(self.destination.exists())

    def test_download_retries_temporary_read_failure_but_not_missing_asset(self):
        with mock.patch.object(release, "open_download", side_effect=[TimeoutError(), io.BytesIO(self.body)]) as request, mock.patch.object(release.time, "sleep"):
            self.api.download(self.asset, self.destination)
            self.assertEqual(request.call_count, 2)
        self.destination.unlink()
        error = urllib.error.HTTPError("url", 404, "missing", {}, io.BytesIO())
        self.addCleanup(error.close)
        with mock.patch.object(release, "open_download", side_effect=error) as request:
            with self.assertRaises(urllib.error.HTTPError):
                self.api.download(self.asset, self.destination)
            self.assertEqual(request.call_count, 1)

    def test_download_does_not_overwrite_existing_or_accept_invalid_id(self):
        with mock.patch.object(release, "open_download") as request:
            with self.assertRaises(ValueError):
                self.api.download({**self.asset, "id": "../other"}, self.destination)
            self.destination.write_bytes(b"existing")
            with self.assertRaises(ValueError):
                self.api.download(self.asset, self.destination)
            self.assertEqual(self.destination.read_bytes(), b"existing")
            request.assert_not_called()

    def test_api_redirects_never_follow_repository_renames_or_external_hosts(self):
        request = release.urllib.request.Request("https://api.github.com/repos/gx0404/herdr/releases", headers={"Authorization": "Bearer test-token"})
        policy = release.RejectRedirects()
        for url in ("https://api.github.com/repos/other/herdr/releases", "https://evil.invalid/"):
            with self.subTest(url=url), self.assertRaisesRegex(ValueError, "redirects are forbidden"):
                policy.redirect_request(request, None, 302, "redirect", {}, url)

    def test_asset_cdn_redirects_strip_token_and_other_urls_are_rejected(self):
        request = release.urllib.request.Request("https://api.github.com/repos/gx0404/herdr/releases/assets/123", headers={"Authorization": "Bearer test-token"})
        policy = release.AssetRedirects()
        accepted = policy.redirect_request(request, None, 302, "redirect", {}, "https://release-assets.githubusercontent.com/path?signature=example")
        self.assertFalse(accepted.has_header("Authorization"))
        for url in ("http://release-assets.githubusercontent.com/path", "https://github.com/other/herdr/release",
                    "https://evil.invalid/path", "https://release-assets.githubusercontent.com.evil.invalid/path",
                    "https://user@release-assets.githubusercontent.com/path"):
            with self.subTest(url=url), self.assertRaisesRegex(ValueError, "approved GitHub asset CDN"):
                policy.redirect_request(request, None, 302, "redirect", {}, url)


if __name__ == "__main__":
    unittest.main()
