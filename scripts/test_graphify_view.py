from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import graphify_view as view


class GraphifyViewTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary_root = Path(__file__).resolve().parents[1] / "target" / "tmp"
        temporary_root.mkdir(parents=True, exist_ok=True)
        temporary = tempfile.TemporaryDirectory(prefix="graphify-view-", dir=temporary_root)
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.paths = ["src/main.rs", "src/api/mod.rs"]
        for name, data in {
            "src/main.rs": b'fn main() {\n  // <script>& \\"quoted\\"\n}\n',
            "src/api/mod.rs": b"pub fn api() {}\n",
            "scripts/graphify_view.py": b"adapter\n",
            "scripts/graphify_fingerprint.py": b"fingerprint\n",
            "scripts/graphify.sh": b"wrapper\n",
            "scripts/graphify_view_assets/index.html": b"<!doctype html><script src='view-data.js'></script>\n",
            "scripts/graphify_view_assets/viewer.js": b"// viewer\n",
            "scripts/graphify_view_assets/viewer.css": b"body{}\n",
            "scripts/graphify_view_assets/vendor/three.bundle.js": b"// vendor\n",
        }.items():
            self.write(name, data)
        self.graph = {"directed": False, "nodes": [
            {"id": "main", "label": "main", "file_type": "code", "source_file": "main.rs",
             "source_location": "L2", "community": 1, "community_name": "Entry", "_origin": "ast"},
            {"id": "external", "label": "</script><script>alert(1)</script>",
             "source_file": "", "source_location": "", "community": 1},
            {"id": "api", "label": "api", "source_file": "api/mod.rs", "source_location": "L1",
             "community": 2}], "links": [
                 {"source": "main", "target": "external", "relation": "calls",
                  "confidence": "EXTRACTED", "confidence_score": 1, "source_file": "main.rs",
                  "source_location": "L2", "context": "original context", "_origin": "ast"}],
            "hyperedges": []}
        self.save_graph()

    def write(self, name: str, raw: bytes) -> None:
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(raw)

    def save_graph(self) -> None:
        self.write("graphify-out/graph.json", json.dumps(self.graph).encode("utf-8"))

    def export(self) -> dict:
        return view.export_view(self.root, self.paths)

    def check(self) -> dict:
        return view.check_view(self.root, self.paths)

    def snapshot(self) -> dict:
        return {p.relative_to(self.root).as_posix(): (p.read_bytes(), p.stat().st_mtime_ns)
                for p in self.root.rglob("*") if p.is_file()}

    def test_full_graph_original_evidence_and_unlocated_nodes(self) -> None:
        outputs, manifest = view.build_view(self.root, self.paths)
        js = outputs["view-data.js"].decode("utf-8")
        data = json.loads(js.removeprefix("window.GRAPHIFY_VIEW_DATA=").removesuffix(";\n"))
        self.assertEqual(data["meta"]["node_count"], 3)
        self.assertEqual(data["meta"]["edge_count"], 1)
        self.assertEqual(data["meta"]["unlocated_node_count"], 1)
        for original, node in zip(self.graph["nodes"], data["nodes"], strict=True):
            for key, value in original.items():
                self.assertEqual(node[key], value)
        for key, value in self.graph["links"][0].items():
            self.assertEqual(data["links"][0][key], value)
        self.assertEqual(data["nodes"][0]["source_path"], "src/main.rs")
        self.assertTrue(data["nodes"][0]["source_url"].endswith("#L2"))
        self.assertEqual(data["nodes"][0]["kind"], "unknown")
        self.assertEqual(data["nodes"][1]["module"], "未定位引用")
        self.assertIsNone(data["nodes"][1]["source_url"])
        self.assertEqual(data["nodes"][2]["module"], "api")
        self.assertNotIn("</script>", js)
        self.assertEqual(len(manifest["sources"]), 2)
        source = outputs[data["nodes"][0]["source_url"].split("#")[0]].decode("utf-8")
        self.assertIn('id="L2"', source)
        self.assertIn("&lt;script&gt;&amp;", source)
        self.assertIn("../../src/main.rs", source)
        self.assertNotIn("<script>", source)

    def test_absolute_script_help_outside_checkout_ignores_foreign_scripts_package(self) -> None:
        # 整个沙箱仍在 target/tmp；cwd 位于隔离 checkout 之外。
        checkout = self.root / "checkout"
        scripts = checkout / "scripts"
        scripts.mkdir(parents=True)
        source_scripts = Path(view.__file__).resolve().parent
        for name in ("graphify_view.py", "graphify_fingerprint.py"):
            (scripts / name).write_bytes((source_scripts / name).read_bytes())
        outside = self.root / "outside"
        outside.mkdir()
        foreign_scripts = outside / "scripts"
        foreign_scripts.mkdir()
        (foreign_scripts / "__init__.py").write_text("# Unrelated package\n", encoding="utf-8")
        environment = os.environ.copy()
        environment.update({"PYTHONPATH": str(outside), "PYTHONDONTWRITEBYTECODE": "1",
                            "TMP": str(outside), "TEMP": str(outside), "TMPDIR": str(outside)})
        before = self.snapshot()
        result = subprocess.run(
            [sys.executable, "-B", "-X", "utf8", str(scripts / "graphify_view.py"), "--help"],
            cwd=outside, env=environment, capture_output=True, text=True, encoding="utf-8",
            check=False, timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("{export,check}", result.stdout)
        self.assertEqual(before, self.snapshot())

    def test_no_node_limit_or_edge_deduplication(self) -> None:
        self.graph["nodes"].extend({"id": f"ref-{i}", "label": "ref", "community": 1} for i in range(6000))
        self.graph["links"] *= 2
        self.save_graph()
        _, manifest = view.build_view(self.root, self.paths)
        self.assertEqual(manifest["counts"]["node_count"], 6003)
        self.assertEqual(manifest["counts"]["edge_count"], 2)

    def test_check_is_read_only_and_preserves_foreign_graph_artifacts(self) -> None:
        self.write("graphify-out/other-cache.json", b"foreign\n")
        self.export()
        before = self.snapshot()
        self.check()
        self.assertEqual(self.snapshot(), before)
        self.export()
        self.assertEqual((self.root / "graphify-out/other-cache.json").read_bytes(), b"foreign\n")

    def test_missing_manifest_does_not_create_outputs(self) -> None:
        before = self.snapshot()
        with self.assertRaisesRegex(view.ViewError, "缺少 view-manifest"):
            self.check()
        self.assertEqual(before, self.snapshot())

    def test_inputs_and_manifest_drift_fail_without_repair(self) -> None:
        self.export()
        targets = ["src/main.rs", "scripts/graphify_view.py", "scripts/graphify_fingerprint.py",
                   "scripts/graphify.sh", "scripts/graphify_view_assets/viewer.js", "graphify-out/graph.json"]
        for name in targets:
            with self.subTest(name=name):
                path = self.root / name
                original = path.read_bytes()
                path.write_bytes(original + b"\n")
                before = self.snapshot()
                with self.assertRaisesRegex(view.ViewError, "过期"):
                    self.check()
                self.assertEqual(before, self.snapshot())
                path.write_bytes(original)
        manifest_path = self.root / "graphify-out" / view.MANIFEST
        manifest = json.loads(manifest_path.read_bytes())
        manifest["counts"]["node_count"] = 0
        manifest_path.write_bytes(view._json(manifest))
        with self.assertRaisesRegex(view.ViewError, "过期"):
            self.check()

    def test_output_missing_tampered_extra_and_coverage_guard(self) -> None:
        manifest = self.export()
        targets = ["GRAPH_TREE.html", "view-data.js", "view-assets/viewer.js",
                   next(name for name in manifest["outputs"] if name.startswith("view-sources/"))]
        for name in targets:
            with self.subTest(name=name):
                path = self.root / "graphify-out" / name
                original = path.read_bytes()
                path.write_bytes(b"tampered")
                before = self.snapshot()
                with self.assertRaisesRegex(view.ViewError, "摘要不一致"):
                    self.check()
                with self.assertRaisesRegex(view.ViewError, "拒绝覆盖已修改"):
                    self.export()
                self.assertEqual(before, self.snapshot())
                path.unlink()
                with self.assertRaisesRegex(view.ViewError, "产物缺失"):
                    self.check()
                path.write_bytes(original)
        self.write("graphify-out/view-assets/foreign.txt", b"keep")
        with self.assertRaisesRegex(view.ViewError, "未登记"):
            self.export()
        with self.assertRaisesRegex(view.ViewError, "未登记"):
            self.check()

    def test_first_export_refuses_unowned_html(self) -> None:
        self.write("graphify-out/GRAPH_TREE.html", b"old view")
        before = self.snapshot()
        with self.assertRaisesRegex(view.ViewError, "未登记"):
            self.export()
        self.assertEqual(before, self.snapshot())

    def test_rebuild_removes_only_obsolete_owned_outputs(self) -> None:
        manifest = self.export()
        old_snapshot = "view-sources/" + view._sha(b"src/api/mod.rs") + ".html"
        self.assertIn(old_snapshot, manifest["outputs"])
        self.graph["nodes"].pop()
        self.save_graph()
        (self.root / "src/api/mod.rs").unlink()
        self.paths.pop()
        with self.assertRaisesRegex(view.ViewError, "过期"):
            self.check()
        self.export()
        self.assertFalse((self.root / "graphify-out" / old_snapshot).exists())
        self.check()

    def test_rejects_path_escape_missing_source_and_invalid_line(self) -> None:
        for source in ("../private.rs", "/etc/passwd", "C:/secret.rs", "api/../../secret.rs",
                       "api\\mod.rs", "api//mod.rs", "missing.rs", "src/main.rs"):
            with self.subTest(source=source):
                self.graph["nodes"][0]["source_file"] = source
                self.save_graph()
                with self.assertRaises(view.ViewError):
                    self.export()
                self.assertFalse((self.root / "graphify-out" / view.MANIFEST).exists())
        self.graph["nodes"][0]["source_file"] = "main.rs"
        self.graph["nodes"][0]["source_location"] = "L999"
        self.save_graph()
        with self.assertRaisesRegex(view.ViewError, "行号越界"):
            self.export()

    def test_unknown_location_retained_without_guessing(self) -> None:
        self.graph["nodes"][0]["source_location"] = "unknown range"
        self.save_graph()
        data = view.adapt_graph(self.graph, {p: (self.root / p).read_bytes() for p in self.paths}, "hash")
        self.assertIsNone(data["nodes"][0]["line"])
        self.assertNotIn("#", data["nodes"][0]["source_url"])
        self.assertEqual(data["nodes"][0]["source_location"], "unknown range")

    def test_source_anchors_follow_lf_not_unicode_line_separators(self) -> None:
        raw = '// first\u2028still first\r\n// second\n'.encode("utf-8")
        rendered = view._snapshot("src/main.rs", raw).decode("utf-8")
        self.assertIn('id="L2"', rendered)
        self.assertNotIn('id="L3"', rendered)
        self.assertEqual(view._source_lines(raw, "src/main.rs"),
                         ["// first\u2028still first", "// second"])
        self.assertIn('id="L1"', view._snapshot("src/empty.rs", b"").decode("utf-8"))

    def test_rejects_duplicate_ids_and_dangling_edges(self) -> None:
        self.graph["nodes"].append(self.graph["nodes"][0].copy())
        self.save_graph()
        with self.assertRaisesRegex(view.ViewError, "重复"):
            self.export()
        self.graph["nodes"].pop()
        self.graph["links"][0]["target"] = "missing"
        self.save_graph()
        with self.assertRaisesRegex(view.ViewError, "悬空"):
            self.export()

    def test_rejects_symlinks_before_reading(self) -> None:
        original = Path.is_symlink
        with mock.patch.object(Path, "is_symlink", lambda path: path.name == "main.rs" or original(path)):
            with self.assertRaisesRegex(view.ViewError, "拒绝链接"):
                self.export()

    def test_rejects_missing_assets_and_invalid_source_bytes(self) -> None:
        asset = self.root / "scripts/graphify_view_assets/viewer.js"
        original = asset.read_bytes()
        asset.unlink()
        with self.assertRaisesRegex(view.ViewError, "缺少视图资源"):
            self.export()
        asset.write_bytes(original)
        self.write("src/main.rs", b"\xff")
        with self.assertRaisesRegex(view.ViewError, "UTF-8"):
            self.export()

    def test_manifest_cannot_claim_external_files(self) -> None:
        self.export()
        path = self.root / "graphify-out" / view.MANIFEST
        manifest = json.loads(path.read_bytes())
        manifest["outputs"]["../src/main.rs"] = view._sha(b"source")
        path.write_bytes(view._json(manifest))
        before = self.snapshot()
        with self.assertRaisesRegex(view.ViewError, "不安全"):
            self.export()
        self.assertEqual(before, self.snapshot())


if __name__ == "__main__":
    unittest.main()
