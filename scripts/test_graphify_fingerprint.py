from __future__ import annotations

import hashlib
import json
import tempfile
import unittest
from contextlib import ExitStack
from pathlib import Path
from unittest import mock

from scripts import graphify_fingerprint


class GraphifyFingerprintTests(unittest.TestCase):
    def setUp(self) -> None:
        stack = ExitStack()
        self.addCleanup(stack.close)
        root = Path(stack.enter_context(tempfile.TemporaryDirectory()))
        graph = root / "graphify-out"
        graph.mkdir()
        mirror = root / "docs" / "graphify" / "GRAPH_REPORT.md"
        mirror.parent.mkdir(parents=True)
        self.report = graph / "GRAPH_REPORT.md"
        self.mirror = mirror
        self.fingerprint = graph / "source-fingerprint.json"
        for name, value in {
            "REPO_ROOT": root,
            "GRAPH_DIR": graph,
            "MIRROR_REPORT": mirror,
            "FINGERPRINT_PATH": self.fingerprint,
            "PIPELINE_INPUTS": [],
        }.items():
            stack.enter_context(mock.patch.object(graphify_fingerprint, name, value))
        stack.enter_context(mock.patch.object(graphify_fingerprint, "_graph_sources", return_value=[]))

    def test_write_uses_git_canonical_report_bytes(self) -> None:
        report = "# 图谱\r\n\r\n内容\r\n".encode("utf-8")
        self.report.write_bytes(report)
        self.mirror.write_bytes(report)

        graphify_fingerprint.write_fingerprint()

        expected = report.replace(b"\r\n", b"\n")
        self.assertEqual(self.report.read_bytes(), expected)
        self.assertEqual(self.mirror.read_bytes(), expected)
        payload = json.loads(self.fingerprint.read_text(encoding="utf-8"))
        self.assertEqual(payload["artifacts"]["GRAPH_REPORT.md"], hashlib.sha256(expected).hexdigest())
        self.assertNotIn(b"\r\n", self.fingerprint.read_bytes())
        graphify_fingerprint.check_fingerprint()

    def test_check_reports_drift_without_rewriting_files(self) -> None:
        self.report.write_bytes(b"# graph\n")
        self.mirror.write_bytes(b"# graph\n")
        graphify_fingerprint.write_fingerprint()
        changed = b"# changed\r\n"
        self.report.write_bytes(changed)
        fingerprint = self.fingerprint.read_bytes()

        with self.assertRaisesRegex(graphify_fingerprint.FingerprintError, "图谱产物与指纹不一致"):
            graphify_fingerprint.check_fingerprint()

        self.assertEqual(self.report.read_bytes(), changed)
        self.assertEqual(self.fingerprint.read_bytes(), fingerprint)

    def test_write_does_not_hide_semantic_mirror_drift(self) -> None:
        self.report.write_bytes(b"# graph\r\n")
        self.mirror.write_bytes(b"# different graph\r\n")

        graphify_fingerprint.write_fingerprint()

        with self.assertRaisesRegex(graphify_fingerprint.FingerprintError, "与 graphify-out/GRAPH_REPORT.md 不一致"):
            graphify_fingerprint.check_fingerprint()


if __name__ == "__main__":
    unittest.main()
