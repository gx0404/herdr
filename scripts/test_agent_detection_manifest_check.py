import hashlib
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from scripts import agent_detection_manifest_check as check


def manifest(agent_id: str, version: str, contains: str = "ready") -> str:
    return f'''id = "{agent_id}"
version = "{version}"
min_engine_version = 1
updated_at = "2026-06-10T00:00:00Z"

[[rules]]
id = "idle"
state = "idle"
contains = ["{contains}"]
'''


def catalog(agent_id: str = "codex", path: str = "codex.toml") -> str:
    return f'''schema_version = 1

[[agents]]
id = "{agent_id}"
path = "{path}"
'''


# The staged-exception mechanism is exercised with self-made manifests so it
# does not depend on any real bundled/published manifest staying in the repo.
STAGED_TEST_BUNDLED = manifest("testagent", "2026.06.10.2").replace(
    "min_engine_version = 1", "min_engine_version = 3"
)
STAGED_TEST_PUBLISHED = manifest("testagent", "2026.06.10.1").replace(
    "min_engine_version = 1", "min_engine_version = 2"
)
STAGED_TEST_EXCEPTION = {
    "testagent": (
        "2026.06.10.2",
        "2026.06.10.1",
        hashlib.sha256(STAGED_TEST_PUBLISHED.encode()).hexdigest(),
    ),
}


def staged_manifest_dirs(root: Path) -> tuple[Path, Path]:
    bundled = root / "bundled"
    published = root / "published"
    bundled.mkdir()
    published.mkdir()
    (bundled / "testagent.toml").write_text(STAGED_TEST_BUNDLED, encoding="utf-8", newline="\n")
    (published / "testagent.toml").write_text(STAGED_TEST_PUBLISHED, encoding="utf-8", newline="\n")
    (published / "index.toml").write_text(catalog("testagent", "testagent.toml"))
    return bundled, published


UNPUBLISHED_TEST_MANIFEST = manifest("testagent", "2026.06.10.1")
UNPUBLISHED_TEST_EXCEPTION = {
    "testagent": (
        "2026.06.10.1",
        hashlib.sha256(UNPUBLISHED_TEST_MANIFEST.encode()).hexdigest(),
    ),
}


# A fork-local rule lives only in the bundled manifest: the published copy stays
# upstream's, one version behind, and the exception pins both sides exactly.
FORK_AHEAD_TEST_BUNDLED = manifest("testagent", "2026.06.10.2", "fork-rule")
FORK_AHEAD_TEST_PUBLISHED = manifest("testagent", "2026.06.10.1")
FORK_AHEAD_TEST_EXCEPTION = {
    "testagent": (
        "2026.06.10.2",
        hashlib.sha256(FORK_AHEAD_TEST_BUNDLED.encode()).hexdigest(),
        "2026.06.10.1",
    ),
}


def fork_ahead_manifest_dirs(root: Path, published: str = FORK_AHEAD_TEST_PUBLISHED) -> tuple[Path, Path]:
    bundled_dir = root / "bundled"
    published_dir = root / "published"
    bundled_dir.mkdir()
    published_dir.mkdir()
    (bundled_dir / "testagent.toml").write_text(FORK_AHEAD_TEST_BUNDLED, encoding="utf-8", newline="\n")
    (published_dir / "testagent.toml").write_text(published, encoding="utf-8", newline="\n")
    (published_dir / "index.toml").write_text(catalog("testagent", "testagent.toml"))
    return bundled_dir, published_dir


def unpublished_manifest_dirs(root: Path) -> tuple[Path, Path]:
    bundled = root / "bundled"
    published = root / "published"
    bundled.mkdir()
    published.mkdir()
    (bundled / "testagent.toml").write_text(UNPUBLISHED_TEST_MANIFEST, encoding="utf-8", newline="\n")
    (published / "index.toml").write_text("schema_version = 1\nagents = []\n")
    return bundled, published


class RepositoryManifestTests(unittest.TestCase):
    # The fork's exact exceptions pin real files, so the repository itself must
    # pass the check that CI and the release gate run; a manifest edit without
    # a matching exception update fails here instead of only in CI.
    def test_repository_manifests_match_the_published_catalog(self):
        engine_version = check.read_engine_version(None)
        bundled = check.load_manifest_dir(check.DEFAULT_BUNDLED_DIR, engine_version)
        check.validate_catalog(
            check.DEFAULT_PUBLISHED_DIR,
            bundled,
            engine_version,
            allow_unpublished=False,
        )


class AgentDetectionManifestCheckTests(unittest.TestCase):
    def setUp(self):
        # Self-made manifests reuse real agent ids; keep the repository's own
        # fork exceptions out of their way unless a test patches its own.
        patcher = patch.dict(check.FORK_AHEAD_BUNDLED_MANIFESTS, {}, clear=True)
        patcher.start()
        self.addCleanup(patcher.stop)

    def test_validates_bundled_and_matching_published_catalog(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            bundled = root / "bundled"
            website = root / "website"
            bundled.mkdir()
            website.mkdir()
            content = manifest("codex", "2026.06.10.1")
            (bundled / "codex.toml").write_text(content)
            (website / "codex.toml").write_text(content)
            (website / "index.toml").write_text(catalog())

            bundled_manifests = check.load_manifest_dir(bundled, engine_version=1)
            check.validate_catalog(website, bundled_manifests, engine_version=1)

    def test_rejects_published_version_lower_than_bundled(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            bundled = root / "bundled"
            website = root / "website"
            bundled.mkdir()
            website.mkdir()
            (bundled / "codex.toml").write_text(manifest("codex", "2026.06.10.2"))
            (website / "codex.toml").write_text(manifest("codex", "2026.06.10.1"))
            (website / "index.toml").write_text(catalog())

            bundled_manifests = check.load_manifest_dir(bundled, engine_version=1)
            with self.assertRaisesRegex(check.CheckError, "lower than bundled"):
                check.validate_catalog(website, bundled_manifests, engine_version=1)

    def test_allows_explicitly_staged_published_manifest(self):
        with tempfile.TemporaryDirectory() as tmp:
            bundled, website = staged_manifest_dirs(Path(tmp))

            bundled_manifests = check.load_manifest_dir(bundled, engine_version=3)
            with patch.object(check, "STAGED_PUBLISHED_MANIFESTS", STAGED_TEST_EXCEPTION):
                check.validate_catalog(website, bundled_manifests, engine_version=3)

    def test_rejects_staged_published_manifest_without_an_exception(self):
        with tempfile.TemporaryDirectory() as tmp:
            bundled, website = staged_manifest_dirs(Path(tmp))

            bundled_manifests = check.load_manifest_dir(bundled, engine_version=3)
            with self.assertRaisesRegex(check.CheckError, "lower than bundled"):
                check.validate_catalog(website, bundled_manifests, engine_version=3)

    def test_rejects_mutated_staged_published_manifest(self):
        with tempfile.TemporaryDirectory() as tmp:
            bundled, website = staged_manifest_dirs(Path(tmp))
            with (website / "testagent.toml").open("a") as manifest_file:
                manifest_file.write("\n# unexpected mutation\n")

            bundled_manifests = check.load_manifest_dir(bundled, engine_version=3)
            with patch.object(check, "STAGED_PUBLISHED_MANIFESTS", STAGED_TEST_EXCEPTION):
                with self.assertRaisesRegex(check.CheckError, "lower than bundled"):
                    check.validate_catalog(website, bundled_manifests, engine_version=3)

    def test_rejects_unlisted_published_manifest_lag_for_new_engine(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            bundled = root / "bundled"
            website = root / "website"
            bundled.mkdir()
            website.mkdir()
            bundled_content = manifest("codex", "2026.06.10.2").replace(
                "min_engine_version = 1", "min_engine_version = 2"
            )
            (bundled / "codex.toml").write_text(bundled_content)
            (website / "codex.toml").write_text(manifest("codex", "2026.06.10.1"))
            (website / "index.toml").write_text(catalog())

            bundled_manifests = check.load_manifest_dir(bundled, engine_version=2)
            with self.assertRaisesRegex(check.CheckError, "lower than bundled"):
                check.validate_catalog(website, bundled_manifests, engine_version=2)

    def test_rejects_same_version_content_drift(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            bundled = root / "bundled"
            website = root / "website"
            bundled.mkdir()
            website.mkdir()
            (bundled / "codex.toml").write_text(manifest("codex", "2026.06.10.1", "ready"))
            (website / "codex.toml").write_text(manifest("codex", "2026.06.10.1", "changed"))
            (website / "index.toml").write_text(catalog())

            bundled_manifests = check.load_manifest_dir(bundled, engine_version=1)
            with self.assertRaisesRegex(check.CheckError, "same version"):
                check.validate_catalog(website, bundled_manifests, engine_version=1)

    @patch.dict(check.UNPUBLISHED_BUNDLED_MANIFESTS, UNPUBLISHED_TEST_EXCEPTION, clear=True)
    def test_allows_exact_unpublished_bundled_manifest(self):
        with tempfile.TemporaryDirectory() as tmp:
            bundled, published = unpublished_manifest_dirs(Path(tmp))
            bundled_manifests = check.load_manifest_dir(bundled, engine_version=3)
            check.validate_catalog(
                published,
                bundled_manifests,
                engine_version=3,
                allow_unpublished=True,
            )

    @patch.dict(check.UNPUBLISHED_BUNDLED_MANIFESTS, UNPUBLISHED_TEST_EXCEPTION, clear=True)
    def test_release_gate_rejects_exact_unpublished_bundled_manifest(self):
        with tempfile.TemporaryDirectory() as tmp:
            bundled, published = unpublished_manifest_dirs(Path(tmp))
            bundled_manifests = check.load_manifest_dir(bundled, engine_version=3)
            with self.assertRaisesRegex(check.CheckError, "missing bundled agent"):
                check.validate_catalog(published, bundled_manifests, engine_version=3)

    @patch.dict(check.UNPUBLISHED_BUNDLED_MANIFESTS, UNPUBLISHED_TEST_EXCEPTION, clear=True)
    def test_rejects_mutated_unpublished_bundled_manifest(self):
        with tempfile.TemporaryDirectory() as tmp:
            bundled, published = unpublished_manifest_dirs(Path(tmp))
            with (bundled / "testagent.toml").open("a") as manifest_file:
                manifest_file.write("\n# unexpected mutation\n")
            bundled_manifests = check.load_manifest_dir(bundled, engine_version=3)
            with self.assertRaisesRegex(check.CheckError, "missing bundled agent"):
                check.validate_catalog(
                    published,
                    bundled_manifests,
                    engine_version=3,
                    allow_unpublished=True,
                )

    @patch.dict(check.FORK_AHEAD_BUNDLED_MANIFESTS, FORK_AHEAD_TEST_EXCEPTION, clear=True)
    def test_allows_exact_fork_ahead_bundled_manifest_including_release_gate(self):
        with tempfile.TemporaryDirectory() as tmp:
            bundled, published = fork_ahead_manifest_dirs(Path(tmp))
            bundled_manifests = check.load_manifest_dir(bundled, engine_version=1)
            for allow_unpublished in (True, False):
                check.validate_catalog(
                    published,
                    bundled_manifests,
                    engine_version=1,
                    allow_unpublished=allow_unpublished,
                )

    def test_rejects_fork_ahead_bundled_manifest_without_an_exception(self):
        with tempfile.TemporaryDirectory() as tmp:
            bundled, published = fork_ahead_manifest_dirs(Path(tmp))
            bundled_manifests = check.load_manifest_dir(bundled, engine_version=1)
            with self.assertRaisesRegex(check.CheckError, "lower than bundled"):
                check.validate_catalog(published, bundled_manifests, engine_version=1)

    @patch.dict(check.FORK_AHEAD_BUNDLED_MANIFESTS, FORK_AHEAD_TEST_EXCEPTION, clear=True)
    def test_rejects_mutated_fork_ahead_bundled_manifest(self):
        with tempfile.TemporaryDirectory() as tmp:
            bundled, published = fork_ahead_manifest_dirs(Path(tmp))
            with (bundled / "testagent.toml").open("a") as manifest_file:
                manifest_file.write("\n# unexpected mutation\n")
            bundled_manifests = check.load_manifest_dir(bundled, engine_version=1)
            with self.assertRaisesRegex(check.CheckError, "lower than bundled"):
                check.validate_catalog(published, bundled_manifests, engine_version=1)

    @patch.dict(check.FORK_AHEAD_BUNDLED_MANIFESTS, FORK_AHEAD_TEST_EXCEPTION, clear=True)
    def test_rejects_fork_ahead_exception_once_published_catches_up(self):
        with tempfile.TemporaryDirectory() as tmp:
            bundled, published = fork_ahead_manifest_dirs(
                Path(tmp), published=manifest("testagent", "2026.06.10.3")
            )
            bundled_manifests = check.load_manifest_dir(bundled, engine_version=1)
            with self.assertRaisesRegex(check.CheckError, "stale FORK_AHEAD_BUNDLED_MANIFESTS"):
                check.validate_catalog(published, bundled_manifests, engine_version=1)

    @patch.dict(
        check.FORK_AHEAD_BUNDLED_MANIFESTS,
        {"otheragent": FORK_AHEAD_TEST_EXCEPTION["testagent"]},
        clear=True,
    )
    def test_rejects_fork_ahead_exception_for_an_agent_outside_the_catalog(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            bundled = root / "bundled"
            website = root / "website"
            bundled.mkdir()
            website.mkdir()
            content = manifest("codex", "2026.06.10.1")
            (bundled / "codex.toml").write_text(content)
            (website / "codex.toml").write_text(content)
            (website / "index.toml").write_text(catalog())

            bundled_manifests = check.load_manifest_dir(bundled, engine_version=1)
            with self.assertRaisesRegex(check.CheckError, "stale FORK_AHEAD_BUNDLED_MANIFESTS"):
                check.validate_catalog(website, bundled_manifests, engine_version=1)

    def test_rejects_unknown_catalog_agent(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            bundled = root / "bundled"
            website = root / "website"
            bundled.mkdir()
            website.mkdir()
            (bundled / "codex.toml").write_text(manifest("codex", "2026.06.10.1"))
            (website / "newagent.toml").write_text(manifest("newagent", "2026.06.10.1"))
            (website / "index.toml").write_text(catalog("newagent", "newagent.toml"))

            bundled_manifests = check.load_manifest_dir(bundled, engine_version=1)
            with self.assertRaisesRegex(check.CheckError, "unknown agent"):
                check.validate_catalog(website, bundled_manifests, engine_version=1)

    def test_rejects_manifest_requiring_newer_engine(self):
        with tempfile.TemporaryDirectory() as tmp:
            bundled = Path(tmp) / "bundled"
            bundled.mkdir()
            (bundled / "codex.toml").write_text(
                manifest("codex", "2026.06.10.1").replace(
                    "min_engine_version = 1", "min_engine_version = 2"
                )
            )

            with self.assertRaisesRegex(check.CheckError, "exceeds engine"):
                check.load_manifest_dir(bundled, engine_version=1)

    def test_rejects_top_non_empty_lines_below_engine_three(self):
        with tempfile.TemporaryDirectory() as tmp:
            bundled = Path(tmp) / "bundled"
            bundled.mkdir()
            content = manifest("codex", "2026.06.10.1").replace(
                'contains = ["ready"]',
                'region = "top_non_empty_lines(1)"\ncontains = ["ready"]',
            )
            (bundled / "codex.toml").write_text(content)

            with self.assertRaisesRegex(check.CheckError, "requires min_engine_version 3"):
                check.load_manifest_dir(bundled, engine_version=3)

    def test_top_non_empty_lines_requires_canonical_positive_bounded_count(self):
        base_rule = {
            "id": "test",
            "state": "working",
            "contains": ["ready"],
        }
        name = "top_non_empty_lines"
        for count in ("1", str(check.MAX_TOP_REGION_LINE_COUNT)):
            rule = {**base_rule, "region": f"{name}({count})"}
            check.validate_rule(Path("test.toml"), 0, rule, {"gates": 0, "matchers": 0})
        for count in (
            "0",
            "01",
            "+1",
            str(check.MAX_TOP_REGION_LINE_COUNT + 1),
            "9" * 40,
        ):
            rule = {**base_rule, "region": f"{name}({count})"}
            with self.subTest(region=rule["region"]):
                with self.assertRaisesRegex(check.CheckError, "invalid region"):
                    check.validate_rule(
                        Path("test.toml"), 0, rule, {"gates": 0, "matchers": 0}
                    )


if __name__ == "__main__":
    unittest.main()
