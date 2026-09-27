#!/usr/bin/env python3
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

if __package__:
    from .package_windows_conpty import marker_data
    from .setup_zig import ZIG_VERSION
else:
    from package_windows_conpty import marker_data
    from setup_zig import ZIG_VERSION

ROOT = Path(__file__).resolve().parents[1]
REPOSITORY = "gx0404/herdr"
RUST_VERSION = "1.96.1"
TARGETS = {
    "windows": ("x86_64-pc-windows-msvc", "windows-installer"),
    "linux": ("x86_64-unknown-linux-musl", "deb"),
}


def is_hash(value, length=64) -> bool:
    return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{%d}" % length, value) is not None


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def json_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON field: {key}")
        result[key] = value
    return result


def load_json(path: Path):
    return json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=json_object)


def cargo_version(expected: str | None = None) -> str:
    data = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    version = data["package"]["version"]
    if not isinstance(version, str) or not re.fullmatch(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)", version):
        raise ValueError("GX packages require Cargo.toml version X.Y.Z")
    if expected is not None and expected != version:
        raise ValueError("requested version differs from Cargo.toml")
    return version


def source_info() -> tuple[str, bool]:
    def git(*args):
        return subprocess.run(["git", *args], cwd=ROOT, check=True, capture_output=True, text=True).stdout.strip()

    sha = git("rev-parse", "HEAD")
    if not is_hash(sha, 40):
        raise ValueError("source commit must be a complete 40-character SHA")
    return sha, bool(git("status", "--porcelain", "--untracked-files=all"))


def require_source(version: str, sha: str) -> None:
    cargo_version(version)
    actual, dirty = source_info()
    if not is_hash(sha, 40) or actual != sha or dirty:
        raise ValueError("release verification requires the clean checkout at the expected full SHA")


def toolchain_versions() -> tuple[str, str]:
    toolchain = tomllib.loads((ROOT / "rust-toolchain.toml").read_text(encoding="utf-8"))
    if toolchain["toolchain"]["channel"] != RUST_VERSION or ZIG_VERSION != "0.16.0":
        raise ValueError("GX toolchain pins must match rust-toolchain.toml and scripts/setup_zig.py")
    return RUST_VERSION, ZIG_VERSION


def summary(text: str) -> None:
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as stream:
            stream.write(text + "\n")


def artifact_name(version: str, platform: str) -> str:
    if platform == "windows":
        return f"herdr-gx-{version}-windows-x86_64-setup.exe"
    return f"herdr-gx_{version}_amd64.deb"


def regular_file(path: Path) -> None:
    if path.is_symlink() or not path.is_file() or path.stat().st_size == 0:
        raise ValueError(f"missing, empty or symlinked release file: {path.name}")


def expected_payload(platform: str) -> dict[str, str | None]:
    if platform == "linux":
        return {"usr/bin/herdr": None, "usr/share/doc/herdr-gx/copyright": digest(ROOT / "LICENSE")}
    metadata = load_json(ROOT / "packaging/windows/conpty.json")
    files = {entry["destination"]: entry["sha256"] for entry in metadata["bundles"]["x86_64"]["files"]}
    files.update({entry["destination"]: entry["sha256"] for entry in metadata["notices"]})
    files["conpty/herdr-conpty.json"] = hashlib.sha256(marker_data(metadata, "x86_64")).hexdigest()
    files["LICENSE"] = digest(ROOT / "LICENSE")
    files["herdr.exe"] = None
    return files


def historical_payload(files, platform: str) -> dict[str, None]:
    required = {"herdr.exe", "LICENSE", "conpty/herdr-conpty.json", "conpty/conpty.dll", "conpty/x64/OpenConsole.exe"}
    if platform == "linux":
        required = {"usr/bin/herdr", "usr/share/doc/herdr-gx/copyright"}
    if not isinstance(files, dict) or not required <= set(files):
        raise ValueError("previous manifest is missing required payload paths")
    for path in files:
        if (not isinstance(path, str) or not path or "\\" in path or ":" in path
                or any(part in {"", ".", ".."} or part.endswith((" ", ".")) for part in path.split("/"))
                or any(ord(char) < 32 for char in path)):
            raise ValueError("unsafe previous payload path")
    if len({path.casefold() for path in files}) != len(files):
        raise ValueError("previous payload has case-colliding paths")
    return dict.fromkeys(files)


def verify_manifest(info, artifact: Path, version: str, sha: str, platform: str, *, historical=False) -> None:
    target, manager = TARGETS[platform]
    fixed = {
        "schema_version": 1, "version": version, "source_commit": sha,
        "source_dirty": False, "platform": platform, "architecture": "x86_64",
        "target": target, "package_manager": manager,
    }
    if not isinstance(info, dict) or set(info) != set(fixed) | {"binary", "artifact", "files"}:
        raise ValueError(f"{artifact.name}: invalid manifest field set")
    for key, value in fixed.items():
        if type(info.get(key)) is not type(value) or info[key] != value:
            raise ValueError(f"{artifact.name}: manifest {key} differs from release inputs")
    binary = info["binary"]
    if (not isinstance(binary, dict) or set(binary) != {"sha256", "version_output"}
            or not is_hash(binary["sha256"])
            or binary["version_output"] != f"herdr {version}-gx.{manager}.{sha}"):
        raise ValueError(f"{artifact.name}: invalid packaged binary identity")
    expected = {"name": artifact.name, "size": artifact.stat().st_size, "sha256": digest(artifact)}
    if (info["artifact"] != expected or type(info["artifact"].get("size")) is not int):
        raise ValueError(f"{artifact.name}: artifact size/SHA-256 mismatch")
    files = info["files"]
    payload = historical_payload(files, platform) if historical else expected_payload(platform)
    if not isinstance(files, dict) or set(files) != set(payload) or not all(is_hash(value) for value in files.values()):
        raise ValueError(f"{artifact.name}: incomplete or unexpected payload file set")
    for path, value in payload.items():
        if value is not None and files[path] != value:
            raise ValueError(f"{artifact.name}: payload digest mismatch: {path}")
    executable = "herdr.exe" if platform == "windows" else "usr/bin/herdr"
    if files[executable] != binary["sha256"]:
        raise ValueError(f"{artifact.name}: binary and payload digests differ")


def write_or_verify(path: Path, content: str) -> None:
    if path.exists() or path.is_symlink():
        regular_file(path)
        if path.read_bytes() != content.encode("utf-8"):
            raise ValueError(f"existing {path.name} differs from verified release; refusing to overwrite")
    else:
        with path.open("x", encoding="utf-8", newline="\n") as stream:
            stream.write(content)


def verify_artifacts(folder: Path, version: str, sha: str) -> list[Path]:
    require_source(version, sha)
    if folder.is_symlink() or not folder.is_dir():
        raise ValueError("release artifact directory must be a real directory")
    required = {artifact_name(version, platform) + suffix
                for platform in TARGETS for suffix in ("", ".manifest.json", ".sha256")}
    names = {path.name for path in folder.iterdir()}
    if not required <= names or not names <= required | {"manifest.json", "SHA256SUMS"}:
        raise ValueError("release artifact directory has missing or unexpected files")
    manifests, artifacts = [], []
    for platform in TARGETS:
        artifact = folder / artifact_name(version, platform)
        metadata = folder / (artifact.name + ".manifest.json")
        checksums = folder / (artifact.name + ".sha256")
        for path in (artifact, metadata, checksums):
            regular_file(path)
        info = load_json(metadata)
        verify_manifest(info, artifact, version, sha, platform)
        expected = f"{digest(artifact)}  {artifact.name}\n{digest(metadata)}  {metadata.name}\n"
        if checksums.read_text(encoding="ascii") != expected:
            raise ValueError(f"{artifact.name}: sidecar checksum mismatch")
        manifests.append(info)
        artifacts.append(artifact)
    manifest = folder / "manifest.json"
    record = {"schema_version": 1, "repository": REPOSITORY, "version": version,
              "source_commit": sha, "source_dirty": False, "packages": manifests}
    write_or_verify(manifest, json.dumps(record, indent=2, sort_keys=True) + "\n")
    files = [*artifacts, manifest]
    sums = folder / "SHA256SUMS"
    write_or_verify(sums, "".join(f"{digest(path)}  {path.name}\n" for path in files))
    return [*files, sums]


class RejectRedirects(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, message, headers, url):
        raise ValueError("GitHub API redirects are forbidden; repository identity must not change")


class AssetRedirects(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, message, headers, url):
        parsed = urllib.parse.urlsplit(url)
        if (parsed.scheme != "https" or parsed.netloc not in {
                "release-assets.githubusercontent.com", "objects.githubusercontent.com"}
                or parsed.username or parsed.password):
            raise ValueError("release download redirect is not an approved GitHub asset CDN")
        redirected = super().redirect_request(request, fp, code, message, headers, url)
        if redirected is not None:
            redirected.remove_header("Authorization")
        return redirected


def open_api(request, timeout=180):
    return urllib.request.build_opener(RejectRedirects()).open(request, timeout=timeout)


def open_download(request, timeout=180):
    return urllib.request.build_opener(AssetRedirects()).open(request, timeout=timeout)


def transient_read_error(error) -> bool:
    return not isinstance(error, urllib.error.HTTPError) or error.code in (429, 500, 502, 503, 504)


class GitHub:
    def __init__(self):
        self.token = os.environ["GH_TOKEN"]
        if not self.token:
            raise ValueError("GH_TOKEN is required")
        self.base = f"https://api.github.com/repos/{REPOSITORY}"

    def request(self, method: str, path: str, data=None, binary: Path | None = None):
        if path.startswith("https://"):
            parsed = urllib.parse.urlsplit(path)
            if (parsed.scheme != "https" or parsed.netloc != "uploads.github.com"
                    or not re.fullmatch(r"/repos/gx0404/herdr/releases/[1-9]\d*/assets", parsed.path)):
                raise ValueError("refusing credentials for an unexpected upload URL")
            url = path
        elif path.startswith("/") and not path.startswith("//"):
            url = self.base + path
        else:
            raise ValueError("invalid GitHub API path")
        body = binary.read_bytes() if binary else (json.dumps(data).encode() if data is not None else None)
        headers = {"Authorization": f"Bearer {self.token}", "Accept": "application/vnd.github+json",
                   "X-GitHub-Api-Version": "2022-11-28", "User-Agent": "herdr-gx-release",
                   "Content-Type": "application/octet-stream" if binary else "application/json"}
        request = urllib.request.Request(url, data=body, headers=headers, method=method)
        for attempt in range(3):
            try:
                with open_api(request, timeout=180) as response:
                    payload = response.read()
                    return json.loads(payload, object_pairs_hook=json_object) if payload else None
            except (urllib.error.URLError, TimeoutError) as error:
                if method != "GET" or not transient_read_error(error) or attempt == 2:
                    raise
                if isinstance(error, urllib.error.HTTPError) and error.fp is not None:
                    error.close()
                time.sleep(2 ** attempt)

    def download(self, asset: dict, destination: Path) -> None:
        asset_id = asset.get("id")
        if type(asset_id) is not int or asset_id <= 0 or destination.exists() or destination.is_symlink():
            raise ValueError("invalid asset ID or existing download destination")
        expected_size, expected_digest = asset.get("size"), asset.get("digest", "")
        if (type(expected_size) is not int or not 0 < expected_size <= 2 * 1024 ** 3
                or not isinstance(expected_digest, str) or not expected_digest.startswith("sha256:")
                or not is_hash(expected_digest[7:])):
            raise ValueError("invalid server asset size/digest")
        request = urllib.request.Request(self.base + f"/releases/assets/{asset_id}", headers={
            "Authorization": f"Bearer {self.token}", "Accept": "application/octet-stream",
            "X-GitHub-Api-Version": "2022-11-28", "User-Agent": "herdr-gx-release",
        })
        for attempt in range(3):
            try:
                with open_download(request, timeout=180) as response, destination.open("xb") as stream:
                    size, checksum = 0, hashlib.sha256()
                    for chunk in iter(lambda: response.read(1024 * 1024), b""):
                        size += len(chunk)
                        if size > expected_size:
                            raise ValueError("download exceeds the server-declared asset size")
                        stream.write(chunk)
                        checksum.update(chunk)
                if size != expected_size or checksum.hexdigest() != expected_digest[7:]:
                    raise ValueError("downloaded asset size/SHA-256 differs from GitHub metadata")
                return
            except (urllib.error.URLError, TimeoutError) as error:
                destination.unlink(missing_ok=True)
                if not transient_read_error(error) or attempt == 2:
                    raise
                if isinstance(error, urllib.error.HTTPError) and error.fp is not None:
                    error.close()
                time.sleep(2 ** attempt)
            except Exception:
                destination.unlink(missing_ok=True)
                raise

    def optional(self, path: str):
        try:
            return self.request("GET", path)
        except urllib.error.HTTPError as error:
            if error.code != 404:
                raise
            if error.fp is not None:
                error.close()
            return None


def remote_commit(api: GitHub, tag: str) -> str | None:
    ref = api.optional("/git/ref/tags/" + urllib.parse.quote(tag, safe=""))
    if ref is None:
        return None
    obj = ref["object"]
    for _ in range(8):
        if not is_hash(obj.get("sha"), 40):
            break
        if obj["type"] == "commit":
            return obj["sha"]
        if obj["type"] != "tag":
            break
        obj = api.request("GET", "/git/tags/" + obj["sha"])["object"]
    raise ValueError("release tag does not resolve to a valid commit")


def release_state(api: GitHub, version: str, sha: str):
    tag = f"gx-v{version}"
    commit = remote_commit(api, tag)
    if commit is not None and commit != sha:
        raise ValueError("release tag points at a different commit; it will not be moved")
    matches = []
    for page in range(1, 101):
        releases = api.request("GET", f"/releases?per_page=100&page={page}")
        matches.extend(release for release in releases if release.get("tag_name") == tag)
        if len(releases) < 100:
            break
    else:
        raise ValueError("release listing exceeded safety limit")
    if len(matches) > 1:
        raise ValueError("multiple releases use the same tag; manual inspection required")
    release = matches[0] if matches else None
    if release:
        if release.get("draft") is not True:
            raise ValueError(f"{tag} is already published; use a new Cargo.toml version")
        if commit is None or release.get("target_commitish") != sha:
            raise ValueError("existing draft is not bound to the same immutable source tag")
    return commit, release


def version_tuple(version: str) -> tuple[int, int, int]:
    if not isinstance(version, str) or not re.fullmatch(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)", version):
        raise ValueError("GX release version must be canonical X.Y.Z")
    return tuple(map(int, version.split(".")))


def published_identity(release: dict, version: str) -> None:
    if (not isinstance(release, dict) or release.get("draft") is not False or release.get("prerelease") is not False
            or release.get("tag_name") != f"gx-v{version}" or release.get("name") != f"Herdr GX {version}"
            or type(release.get("id")) is not int or release["id"] <= 0
            or not is_hash(release.get("target_commitish"), 40)):
        raise ValueError("previous GX release has invalid published identity/source")


def select_previous(api: GitHub, version: str, tag: str):
    current = version_tuple(version)
    if tag:
        if not tag.startswith("gx-v") or version_tuple(tag[4:]) >= current:
            raise ValueError("explicit previous tag must be gx-vX.Y.Z strictly older than Cargo.toml")
        selected = api.request("GET", "/releases/tags/" + urllib.parse.quote(tag, safe=""))
        published_identity(selected, tag[4:])
        return selected
    candidates, seen = [], set()
    for page in range(1, 101):
        releases = api.request("GET", f"/releases?per_page=100&page={page}")
        if not isinstance(releases, list) or any(not isinstance(item, dict) for item in releases):
            raise ValueError("invalid release listing")
        for item in releases:
            name = item.get("tag_name", "")
            if not isinstance(name, str) or not name.startswith("gx-v") or item.get("draft") is True:
                continue
            old_version = version_tuple(name[4:])
            if old_version >= current:
                continue
            published_identity(item, name[4:])
            if name in seen:
                raise ValueError("duplicate published GX tag")
            seen.add(name)
            candidates.append(item)
        if len(releases) < 100:
            break
    else:
        raise ValueError("release listing exceeded safety limit")
    return max(candidates, key=lambda item: version_tuple(item["tag_name"][4:])) if candidates else None


def previous_assets(api: GitHub, release_id: int, version: str) -> list[dict]:
    assets = api.request("GET", f"/releases/{release_id}/assets?per_page=100")
    expected = {artifact_name(version, platform) for platform in TARGETS} | {"manifest.json", "SHA256SUMS"}
    if not isinstance(assets, list) or len(assets) != 4 or any(not isinstance(asset, dict) for asset in assets):
        raise ValueError("previous release must have exactly four assets")
    names = [asset.get("name") for asset in assets]
    if any(not isinstance(name, str) for name in names) or set(names) != expected:
        raise ValueError("previous release has missing, duplicated or unexpected asset names")
    ids = set()
    for asset in assets:
        asset_id = asset.get("id")
        size, checksum = asset.get("size"), asset.get("digest", "")
        if (type(asset_id) is not int or asset_id <= 0 or asset_id in ids
                or asset.get("state") != "uploaded" or type(size) is not int or not 0 < size <= 2 * 1024 ** 3
                or not isinstance(checksum, str) or not checksum.startswith("sha256:") or not is_hash(checksum[7:])
                or asset.get("url") != f"https://api.github.com/repos/{REPOSITORY}/releases/assets/{asset_id}"
                or asset.get("browser_download_url") != f"https://github.com/{REPOSITORY}/releases/download/gx-v{version}/{asset['name']}"):
            raise ValueError("previous asset has invalid ID/state/size/digest/repository URL")
        if asset["name"] in {"manifest.json", "SHA256SUMS"} and size > 1024 * 1024:
            raise ValueError("previous release metadata exceeds the size limit")
        ids.add(asset_id)
    return assets


def verify_previous(folder: Path, version: str, sha: str) -> list[dict]:
    manifest = folder / "manifest.json"
    info = load_json(manifest)
    fixed = {"schema_version": 1, "repository": REPOSITORY, "version": version,
             "source_commit": sha, "source_dirty": False}
    if not isinstance(info, dict) or set(info) != set(fixed) | {"packages"}:
        raise ValueError("previous unified manifest has invalid schema fields")
    for key, value in fixed.items():
        if type(info.get(key)) is not type(value) or info[key] != value:
            raise ValueError(f"previous unified manifest {key} mismatch")
    packages = info["packages"]
    if (not isinstance(packages, list) or len(packages) != 2
            or any(not isinstance(package, dict) for package in packages)
            or {package.get("platform") for package in packages} != set(TARGETS)):
        raise ValueError("previous unified manifest must contain both platform packages")
    for package in packages:
        platform = package["platform"]
        artifact = folder / artifact_name(version, platform)
        verify_manifest(package, artifact, version, sha, platform, historical=True)
    paths = [folder / artifact_name(version, platform) for platform in TARGETS] + [manifest]
    expected = "".join(f"{digest(path)}  {path.name}\n" for path in paths)
    if (folder / "SHA256SUMS").read_text(encoding="ascii") != expected:
        raise ValueError("previous SHA256SUMS differs from the downloaded packages/manifest")
    return packages


def previous_packages(folder: Path, version: str, sha: str, repository: str | None, tag: str = "") -> dict:
    if repository != REPOSITORY:
        raise ValueError("previous release reads require --repository gx0404/herdr")
    require_source(version, sha)
    if folder.exists() or folder.is_symlink():
        raise ValueError("previous package output must not already exist")
    api = GitHub()
    selected = select_previous(api, version, tag)
    if selected is None:
        result = {"available": "false", "tag": "", "sha": "", "version": "", "windows": "", "linux": ""}
        summary(f"Old-to-new upgrade: N/A — no published GX release has a Cargo version below `{version}`.")
    else:
        old_tag = selected["tag_name"]
        old_version, release_id = old_tag[4:], selected["id"]
        old_sha = remote_commit(api, old_tag)
        if old_sha is None or old_sha != selected["target_commitish"]:
            raise ValueError("previous release tag/source SHA mismatch")
        current = api.request("GET", f"/releases/{release_id}")
        published_identity(current, old_version)
        if current["id"] != release_id or current["target_commitish"] != old_sha:
            raise ValueError("previous release source changed during selection")
        assets = previous_assets(api, release_id, old_version)
        folder.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="gx-previous-", dir=folder.parent) as tmp:
            stage = Path(tmp)
            for asset in assets:
                api.download(asset, stage / asset["name"])
            files = [stage / asset["name"] for asset in assets]
            verify_remote_assets(assets, files, complete=True)
            packages = verify_previous(stage, old_version, old_sha)
            latest = api.request("GET", f"/releases/{release_id}")
            published_identity(latest, old_version)
            if (latest["id"] != release_id or latest["target_commitish"] != old_sha
                    or remote_commit(api, old_tag) != old_sha):
                raise ValueError("previous source changed during download")
            verify_remote_assets(previous_assets(api, release_id, old_version), files, complete=True)
            for package in packages:
                artifact = stage / package["artifact"]["name"]
                metadata = stage / (artifact.name + ".manifest.json")
                write_or_verify(metadata, json.dumps(package, indent=2, sort_keys=True) + "\n")
                write_or_verify(stage / (artifact.name + ".sha256"),
                                f"{digest(artifact)}  {artifact.name}\n{digest(metadata)}  {metadata.name}\n")
            shutil.copytree(stage, folder)
        result = {"available": "true", "tag": old_tag, "sha": old_sha, "version": old_version,
                  **{platform: artifact_name(old_version, platform) for platform in TARGETS}}
        summary(f"Upgrade baseline: [`{old_tag}`](https://github.com/{REPOSITORY}/releases/tag/{old_tag})\n\n"
                f"- Fixed source: `{old_sha}`\n- Validated all four published assets; upgrade `{old_version}` → `{version}` is required.")
    record = "".join(f"{key}={value}\n" for key, value in result.items())
    print(record, end="")
    if os.environ.get("GITHUB_OUTPUT"):
        with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
            stream.write(record)
    return result


def require_manual_workflow(repository: str | None) -> None:
    if (repository != REPOSITORY or os.environ.get("GITHUB_ACTIONS") != "true"
            or os.environ.get("GITHUB_EVENT_NAME") != "workflow_dispatch"
            or os.environ.get("GITHUB_REPOSITORY") != REPOSITORY
            or os.environ.get("GX_PUBLISH_REQUESTED") != "true"
            or not os.environ.get("GITHUB_WORKFLOW_REF", "").startswith(
                f"{REPOSITORY}/.github/workflows/gx-release.yml@refs/")):
        raise ValueError("publishing requires --repository gx0404/herdr and its explicit manual gx-release workflow")


def require_admins(api: GitHub) -> None:
    for key in ("GITHUB_ACTOR", "GITHUB_TRIGGERING_ACTOR"):
        actor = os.environ.get(key, "")
        if not actor or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9-]*", actor):
            raise ValueError("publishing requires both triggering actors")
        permission = api.request("GET", "/collaborators/" + actor + "/permission")
        if permission.get("permission") != "admin":
            raise ValueError(f"{actor} must have repository admin permission; tag rules are not bypassed")


def prepare(publish_requested: bool, repository: str | None) -> None:
    version = cargo_version()
    sha, dirty = source_info()
    if dirty:
        raise ValueError("release preparation requires a clean checkout")
    rust, zig = toolchain_versions()
    if publish_requested:
        require_manual_workflow(repository)
        api = GitHub()
        require_admins(api)
        release_state(api, version, sha)
    record = f"sha={sha}\nversion={version}\ntag=gx-v{version}\nrust={rust}\nzig={zig}\n"
    print(record, end="")
    if os.environ.get("GITHUB_OUTPUT"):
        with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
            stream.write(record)
    mode = "Publish after verification" if publish_requested else "Build and verify only; no remote writes"
    summary(f"## Herdr GX {version}\n\n- Source: `{sha}`\n- Mode: {mode}\n"
            "- Planned validation: Windows x64 EXE; the same amd64 deb on Ubuntu 22.04/24.04.\n"
            "- Upgrade baseline resolution is pending; only absence of an older published GX version permits N/A.")


def verify_remote_assets(assets, files: list[Path], complete: bool) -> set[str]:
    expected = {path.name: path for path in files}
    if not isinstance(assets, list) or any(not isinstance(asset, dict) for asset in assets):
        raise ValueError("invalid remote asset list")
    names = [asset.get("name") for asset in assets]
    if (any(not isinstance(name, str) for name in names) or len(set(names)) != len(names)
            or not set(names) <= set(expected) or (complete and set(names) != set(expected))):
        raise ValueError("draft assets are missing, duplicated or unexpected; refusing publication")
    for asset in assets:
        path = expected[asset["name"]]
        if (asset.get("state") != "uploaded" or type(asset.get("size")) is not int
                or asset["size"] != path.stat().st_size or asset.get("digest") != "sha256:" + digest(path)):
            raise ValueError(f"draft asset failed state/size/SHA-256 verification: {path.name}")
    return set(names)


def release_body(version: str, sha: str, manifest: Path) -> str:
    return (f"<!-- herdr-gx source={sha} manifest-sha256={digest(manifest)} -->\n"
            f"Herdr GX {version}\n\nSource: `{sha}`\n\n"
            "Windows x64 current-user installer and one amd64 deb for Ubuntu 22.04/24.04. "
            "Windows installer is unsigned and may trigger SmartScreen. "
            "Verify downloads with SHA256SUMS. Stop existing Herdr sessions before upgrading.\n\n"
            "Installation smoke results are in the workflow summary. Without a previous package, "
            "old-to-new upgrade coverage is N/A; same-version reinstall is not an upgrade.\n")


def publish(folder: Path, version: str, sha: str, repository: str | None) -> None:
    require_manual_workflow(repository)
    files = verify_artifacts(folder, version, sha)
    body = release_body(version, sha, folder / "manifest.json")
    tag = f"gx-v{version}"
    api = GitHub()
    require_admins(api)
    commit, release = release_state(api, version, sha)
    if release and release.get("body") != body:
        raise ValueError("draft source/manifest binding differs; refusing to resume")
    if commit is None:
        api.request("POST", "/git/refs", {"ref": "refs/tags/" + tag, "sha": sha})
    if remote_commit(api, tag) != sha:
        raise ValueError("source tag missing or changed; refusing draft creation")
    if release is None:
        release = api.request("POST", "/releases", {
            "tag_name": tag, "target_commitish": sha, "name": f"Herdr GX {version}",
            "draft": True, "prerelease": False, "make_latest": "false", "body": body,
        })
    release_id = release.get("id")
    if type(release_id) is not int or release_id <= 0:
        raise ValueError("invalid release ID")
    endpoint = f"/releases/{release_id}"
    assets_path = endpoint + "/assets?per_page=100"
    present = verify_remote_assets(api.request("GET", assets_path), files, complete=False)
    def require_unchanged_draft():
        current = api.request("GET", endpoint)
        if (current.get("draft") is not True or current.get("body") != body
                or current.get("tag_name") != tag or current.get("target_commitish") != sha):
            raise ValueError("draft changed concurrently; refusing publication")

    upload = f"https://uploads.github.com/repos/{REPOSITORY}/releases/{release_id}/assets"
    for path in files:
        if path.name not in present:
            require_unchanged_draft()
            api.request("POST", upload + "?name=" + urllib.parse.quote(path.name, safe=""), binary=path)
    verify_artifacts(folder, version, sha)
    verify_remote_assets(api.request("GET", assets_path), files, complete=True)
    if remote_commit(api, tag) != sha:
        raise ValueError("release tag changed; draft remains unpublished")
    require_unchanged_draft()
    result = api.request("PATCH", endpoint, {"draft": False, "make_latest": "false"})
    if result.get("draft") is not False or result.get("tag_name") != tag:
        raise ValueError("publication response is ambiguous; inspect the release before retrying")
    url = f"https://github.com/{REPOSITORY}/releases/tag/{tag}"
    print(url)
    summary(f"Published [Herdr GX {version}]({url}) with exactly four verified assets.")


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description="Verify GX installers and publish only from the manual fork workflow")
    parser.add_argument("action", choices=["prepare", "previous", "verify", "publish"])
    parser.add_argument("--version", help="must match Cargo.toml; never changes it")
    parser.add_argument("--sha", help="complete current source SHA required for previous/verify/publish")
    parser.add_argument("--artifacts", type=Path, default=ROOT / "target/gx-release")
    parser.add_argument("--repository", help="explicit gx0404/herdr confirmation for remote checks/publication")
    parser.add_argument("--previous-tag", default="", help="previous: explicit older gx-vX.Y.Z; empty selects the greatest older published version")
    parser.add_argument("--publish", action="store_true", help="prepare: read-only remote release conflict check")
    args = parser.parse_args(argv)
    try:
        version = cargo_version(args.version)
        if args.action == "prepare":
            prepare(args.publish, args.repository)
        elif args.action == "previous":
            previous_packages(args.artifacts, version, args.sha or "", args.repository, args.previous_tag)
        elif args.action == "verify":
            files = verify_artifacts(args.artifacts, version, args.sha or "")
            print(f"PASS: verified {len(files)} release assets for {args.sha}")
            summary(f"Verified both packages and four release assets for `{args.sha}` (including build-only runs).")
        else:
            publish(args.artifacts, version, args.sha or "", args.repository)
        return 0
    except (ValueError, OSError, KeyError, TypeError, subprocess.CalledProcessError) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
