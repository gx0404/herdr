"""Input-oracle and isolated headless smoke contracts, not GUI input qualification."""
import copy
import unittest
from scripts.windows_input.report import classification_of_client_events, catalogue, channel_identity_errors, herdr_protocol_label, known_host_gap, qualification_matrix, summarize, verdict


class WindowsInputGauntletTests(unittest.TestCase):
    def setUp(self):
        self.matrix = catalogue()
        self.cases = {case["id"]: case for case in self.matrix["cases"]}
        self.evidence = dict(ready=True, focus_verified=True, complete=True, width=121, height=24,
                             outer_geometry=[121, 24, 0x298], pane_geometry=[100, 20, 0x200],
                             final_outer_geometry=[121, 24, 0x298], final_pane_geometry=[100, 20, 0x200])

    def test_catalogue_has_unique_cases_and_geometry_boundaries(self):
        self.assertEqual(len(self.cases), len(self.matrix["cases"]))
        self.assertEqual(self.matrix["widths"], [80, 119, 120, 121, 132, 160, 240])
        self.assertEqual(self.matrix["heights"], [24, 50])
        for case in self.cases.values():
            self.assertTrue(case["id"])
            self.assertLessEqual(set(case["expected"]), set(self.matrix["modes"]))
            for expected in case["expected"].values():
                for value in expected.get("hex", []):
                    self.assertTrue(bytes.fromhex(value))
        self.assertEqual(self.cases["dead-acute"]["kind"], "layout-key")
        self.assertEqual(self.cases["altgr-euro"]["kind"], "manual")
        for spec in self.matrix["release_plan"]:
            for case_id in spec["cases"]:
                self.assertIn(spec["mode"], self.cases[case_id]["expected"])
                self.assertNotIn(self.cases[case_id]["kind"], {"manual", "qualification"})
        self.assertFalse({"page-up", "page-down"} & {case_id for spec in self.matrix["release_plan"] for case_id in spec["cases"]})

    def test_default_catalogue_does_not_inject_terminal_host_actions(self):
        for case_id in ["ctrl-v", "alt-enter", "ctrl-shift-up", "ctrl-shift-down", "ctrl-shift-home", "ctrl-shift-end"]:
            self.assertEqual(self.cases[case_id]["kind"], "qualification", case_id)

    def test_kitty_function_keys_accept_native_and_legacy_encodings(self):
        self.assertEqual(self.cases["shift-tab"]["expected"]["kitty"]["hex"], ["1b5b5a", "1b5b393b3275"])
        self.assertEqual(self.cases["up"]["expected"]["kitty"]["hex"], ["1b5b41", "1b5b353734313975"])

    def test_shift_enter_cannot_pass_with_plain_enter_or_trailing_duplicates(self):
        case = self.cases["shift-enter"]
        correct = "1b5b32373b323b31337e"
        for value, status in [("0d", "fail"), (correct, "pass"), (correct + "0d", "fail")]:
            self.assertEqual(verdict(case, "mok2", {**self.evidence, "hex": value})[0], status)
        self.assertEqual(verdict(case, "legacy", {**self.evidence, "hex": "0d"})[0], "unsupported")

    def test_focus_readiness_and_actual_geometry_are_required(self):
        case = self.cases["letter-a"]
        good = {**self.evidence, "hex": "61"}
        for field in ["ready", "focus_verified", "complete", "pane_geometry", "outer_geometry", "final_pane_geometry", "final_outer_geometry"]:
            value = copy.deepcopy(good)
            value.pop(field)
            self.assertEqual(verdict(case, "legacy", value)[0], "inconclusive", field)
        self.assertEqual(verdict(case, "legacy", {**good, "outer_geometry": [120, 24]})[0], "inconclusive")
        for malformed in [1, "121,24", {}, [121], [121, "24"], [True, 24]]:
            self.assertEqual(verdict(case, "legacy", {**good, "pane_geometry": malformed})[0], "inconclusive")

    def test_native_modifiers_release_and_repeat_are_not_discarded(self):
        # type, down, repeat, vk, scan, Unicode, control-state
        self.evidence["scans"] = [42, 28]
        records = [[1, 1, 1, 13, 28, 13, 16], [1, 0, 1, 13, 28, 13, 16]]
        case = self.cases["shift-enter"]
        self.assertEqual(verdict(case, "native", {**self.evidence, "records": records})[0], "pass")
        for index, value in [(6, 0), (2, 2), (3, 10), (4, 0), (5, 122)]:
            bad = copy.deepcopy(records)
            bad[0][index] = value
            self.assertEqual(verdict(case, "native", {**self.evidence, "records": bad})[0], "fail")
        self.assertEqual(verdict(case, "native", {**self.evidence, "records": records[:1]})[0], "fail")
        extra = [[1, 1, 1, 16, 42, 0, 16]] + records
        self.assertEqual(verdict(case, "native", {**self.evidence, "records": extra})[0], "fail")
        balanced = extra + [[1, 0, 1, 16, 42, 0, 0]]
        self.assertEqual(verdict(case, "native", {**self.evidence, "records": balanced})[0], "pass")

    def test_herdr_native_page_keys_must_be_consumed_before_the_pane(self):
        case = self.cases["page-up"]
        evidence = {**self.evidence, "path": "herdr", "scans": [73]}
        records = [[1, 1, 1, 33, 73, 0, 0], [1, 0, 1, 33, 73, 0, 0]]
        self.assertEqual(verdict(case, "native", {**evidence, "records": []})[0], "inconclusive")
        self.assertEqual(verdict(case, "native", {**evidence, "records": [[4, 0, 0, 0, 0, 0, 0]]})[0], "inconclusive")
        self.assertEqual(verdict(case, "native", {**evidence, "records": records})[0], "fail")
        self.assertEqual(verdict(case, "native", {**evidence, "records": [None]})[0], "inconclusive")
        self.assertEqual(verdict(case, "native", {**evidence, "path": "direct", "records": records})[0], "pass")
        for mode in ("legacy", "mok2", "kitty"):
            self.assertEqual(verdict(case, mode, {**evidence, "hex": ""})[0], "inconclusive")
            self.assertEqual(verdict(case, mode, {**evidence, "hex": "1b5b357e"})[0], "fail")

    def test_paste_requires_one_envelope_and_complete_unicode_payload(self):
        case = self.cases["paste-unicode"]
        payload = case["text"].replace("\n", "\r\n").encode()
        framed = b"\x1b[200~" + payload + b"\x1b[201~"
        for data, expected in [(framed, "pass"), (payload, "fail"), (framed * 2, "fail"),
                               (framed[:-1], "fail"), (framed + b"\r", "fail"),
                               (b"\x1b[200~\xff\x1b[201~", "fail")]:
            self.assertEqual(verdict(case, "kitty", {**self.evidence, "hex": data.hex()})[0], expected)

    def test_remote_clipboard_image_requires_empty_host_paste_and_staged_png_path(self):
        case = self.cases["clipboard-image"]
        empty_paste = b"\x1b[200~\x1b[201~"
        staged = b"\x1b[200~C:\\Temp\\herdr-clipboard-images-user\\image.png\x1b[201~"
        self.assertEqual(verdict(case, "legacy", {**self.evidence, "path": "direct", "hex": empty_paste.hex()})[0], "pass")
        remote = {**self.evidence, "path": "herdr-remote", "hex": staged.hex(),
                  "paste_origin": "empty-paste",
                  "staged_image_sha256": case["expected"]["legacy"]["sha256"]}
        self.assertEqual(verdict(case, "legacy", remote)[0], "pass")
        self.assertEqual(verdict(case, "legacy", {**remote, "staged_image_sha256": "0" * 64})[0], "fail")
        self.assertEqual(verdict(case, "legacy", {**self.evidence, "path": "herdr-remote", "hex": empty_paste.hex()})[0], "fail")
        self.assertEqual(verdict(case, "legacy", {**self.evidence, "path": "herdr", "hex": empty_paste.hex()})[0], "not_run")

    def test_remote_clipboard_image_rejects_a_paste_the_terminal_issued(self):
        # A staged PNG that came from a terminal-issued text paste proves nothing
        # about the empty-paste bridge (#4314), and a staged PNG with no mapper
        # evidence at all must not qualify the bridge either.
        case = self.cases["clipboard-image"]
        staged = b"\x1b[200~C:\\Temp\\herdr-clipboard-images-user\\image.png\x1b[201~"
        remote = {**self.evidence, "path": "herdr-remote", "hex": staged.hex(),
                  "staged_image_sha256": case["expected"]["legacy"]["sha256"]}
        self.assertEqual(verdict(case, "legacy", {**remote, "paste_origin": "terminal-paste"})[0], "fail")
        for weak in ("none", "key-event", None):
            self.assertEqual(verdict(case, "legacy", {**remote, "paste_origin": weak})[0], "inconclusive", weak)
        self.assertEqual(verdict(case, "legacy", {**remote, "paste_origin": "empty-paste"})[0], "pass")
        direct = {**self.evidence, "path": "direct", "hex": b"\x1b[200~\x1b[201~".hex()}
        self.assertEqual(verdict(case, "legacy", {**direct, "paste_origin": "terminal-paste"})[0], "fail")
        self.assertEqual(verdict(case, "legacy", {**direct, "paste_origin": "empty-paste"})[0], "pass")

    def test_client_event_trace_classifies_paste_origin(self):
        self.assertEqual(classification_of_client_events(None), None)
        self.assertEqual(classification_of_client_events([]), "none")
        self.assertEqual(
            classification_of_client_events(['mapped_event_groups=[Paste { text: "" }]']),
            "empty-paste")
        self.assertEqual(
            classification_of_client_events(['mapped_event_groups=[Paste { text: "hello" }]']),
            "terminal-paste")
        self.assertEqual(
            classification_of_client_events(
                ["mapped_event_groups=[Key { code: Char('v'), modifiers: 0, kind: Press }]"]),
            "key-event")
        # A release alone is not a consumed press.
        self.assertEqual(
            classification_of_client_events(
                ["mapped_event_groups=[Key { code: Char('v'), modifiers: 0, kind: Release }]"]),
            "none")
        self.assertEqual(classification_of_client_events(["not a trace line", 7]), None)

    def test_mouse_interleave_requires_ordered_motion_and_paste(self):
        case = self.cases["mouse-interleave"]
        motion = b"\x1b[<35;10;5M"
        paste = b"\x1b[200~mouse\r\npaste\x1b[201~"
        good = b"a" + motion + paste + motion + b"b"
        self.assertEqual(verdict(case, "kitty", {**self.evidence, "hex": good.hex()})[0], "pass")
        for bad in (good.replace(motion, b"", 1), good + b"b", b"a" + motion + motion + paste + b"b"):
            self.assertEqual(verdict(case, "kitty", {**self.evidence, "hex": bad.hex()})[0], "fail")

    def test_runtime_mode_transition_has_one_exact_order(self):
        case = self.cases["mode-transitions"]
        for mode in ("legacy", "mok2", "kitty"):
            expected = case["expected"][mode]["hex"][0]
            self.assertEqual(verdict(case, mode, {**self.evidence, "hex": expected})[0], "pass")
            self.assertEqual(verdict(case, mode, {**self.evidence, "hex": expected + "0d"})[0], "fail")
        direct = {**self.evidence, "path": "direct"}
        for observed in ("a\rb\rc\rd\re\rf", "a\rb\rc\x1b[13;2ud\re\rf"):
            self.assertEqual(verdict(case, "legacy", {**direct, "hex": observed.encode().hex()})[0], "unsupported")

    def test_mouse_focus_refresh_requires_reports_on_both_sides(self):
        case = self.cases["mouse-focus-refresh"]
        mouse = b"\x1b[<35;10;5M\x1b[<0;10;5M\x1b[<0;10;5m\x1b[<64;10;5M"
        good = b"a" + mouse + b"bc" + mouse + b"d"
        self.assertEqual(verdict(case, "legacy", {**self.evidence, "hex": good.hex()})[0], "pass")
        for bad in (b"ab" + b"c" + mouse + b"d", b"a" + mouse + b"bcd"):
            self.assertEqual(verdict(case, "legacy", {**self.evidence, "hex": bad.hex()})[0], "fail")

    def test_qualification_matrix_uses_observed_results_only(self):
        observations = [
            {"host": "stable", "case": "shift-enter", "path": "herdr", "mode": "mok2", "status": "pass"},
            {"host": "stable", "case": "shift-enter", "path": "direct", "mode": "legacy", "status": "unsupported"},
            {"host": "stable", "case": "shift-enter", "path": "direct", "mode": "kitty", "status": "pass"},
            {"host": "stable", "case": "shift-enter", "path": "direct", "mode": "kitty", "status": "inconclusive"},
        ]
        rows = {row[0]: row[1:] for row in qualification_matrix({"channels": ["stable"], "observations": observations})}
        self.assertEqual(rows["Shift+Enter"], ("PASS", "X - becomes Enter", "PASS**"))
        self.assertEqual(rows["Multiline paste"], ("NOT TESTED", "NOT TESTED", "NOT TESTED"))
        observations.append({"host": "stable", "case": "dead-acute", "path": "herdr", "mode": "mok2", "status": "pass"})
        observations.extend([
            {"host": "stable", "case": "letter-a", "path": "direct", "mode": "legacy", "width": 80, "status": "pass"},
            {"host": "stable", "case": "shift-enter", "path": "direct", "mode": "legacy", "width": 80, "status": "unsupported"},
            {"host": "stable", "case": "paste-lf", "path": "direct", "mode": "legacy", "width": 80, "status": "pass"},
            {"host": "stable", "case": "mouse-focus-refresh", "path": "herdr", "mode": "legacy", "width": 80, "status": "pass"},
            {"host": "stable", "case": "mouse-focus-refresh", "path": "direct", "mode": "legacy", "width": 80, "status": "pass"},
            {"host": "stable", "case": "mouse-focus-refresh", "path": "direct", "mode": "kitty", "width": 80, "status": "pass"},
        ])
        rows = {row[0]: row[1:] for row in qualification_matrix({"channels": ["stable"], "observations": observations})}
        self.assertEqual(rows["Dead-key composition"][0], "PASS")
        self.assertEqual(rows["Resize 120 -> 80"][1], "PASS")
        self.assertEqual(rows["Mouse after resize"], ("PASS", "PASS", "PASS"))
        self.assertEqual(rows["AltGr"], ("MANUAL", "MANUAL", "MANUAL"))
        self.assertEqual(rows["IME composition"], ("MANUAL", "MANUAL", "MANUAL"))

    def test_qualification_matrix_requires_every_selected_channel(self):
        observations = [
            {"host": "stable", "case": "shift-enter", "path": "herdr", "mode": "mok2", "status": "pass"},
        ]
        for result in ({"channels": ["stable", "preview"], "observations": observations},
                       {"observations": observations}):
            rows = {row[0]: row[1:] for row in qualification_matrix(result)}
            self.assertEqual(rows["Shift+Enter"][0], "PARTIAL")

    def test_release_matrix_uses_its_paired_hosts(self):
        observations = [{"host": spec["channel"], "path": spec["path"], "mode": spec["mode"],
                         "case": case, "status": "pass", "width": width}
                        for spec in self.matrix["release_plan"] for width in (120, 80) for case in spec["cases"]]
        nominal_only = [row for row in observations if row["width"] == 120]
        rows = {row[0]: row[1:] for row in qualification_matrix({"campaign": "release", "observations": nominal_only})}
        self.assertEqual(rows["Resize 120 -> 80"], ("PARTIAL", "PARTIAL"))
        rows = {row[0]: row[1:] for row in qualification_matrix({"campaign": "release", "observations": observations})}
        for name, cells in rows.items():
            self.assertEqual(cells, ("MANUAL", "MANUAL") if name in {"PageUp/PageDown scroll", "AltGr", "IME composition"}
                             else ("PASS", "PASS"), name)
        failure = next(row for row in observations if row["host"] == "stable" and row["mode"] == "mok2"
                       and row["case"] == "letter-a" and row["width"] == 120)
        failure["status"] = "fail"
        rows = {row[0]: row[1:] for row in qualification_matrix({"campaign": "release", "observations": observations})}
        self.assertEqual(rows["Printable keys"][0], "FAIL")
        self.assertEqual(rows["Resize 120 -> 80"][0], "FAIL")
        self.assertEqual(rows["PageUp/PageDown scroll"], ("MANUAL", "MANUAL"))
        failure["status"] = "pass"
        missing_mode = [row for row in observations if not (row["host"] == "stable" and row["mode"] == "mok2"
                        and row["case"] == "paste-lf")]
        rows = {row[0]: row[1:] for row in qualification_matrix({"campaign": "release", "observations": missing_mode})}
        self.assertEqual(rows["Multiline paste"][0], "PARTIAL")
        stable_partial = next(row for row in observations if row["host"] == "stable" and row["mode"] == "mok2"
                              and row["case"] == "paste-lf" and row["width"] == 120)
        for status, expected in (("inconclusive", "INCONCLUSIVE"), ("unsupported", "UNSUPPORTED"),
                                 ("not_run", "PARTIAL")):
            stable_partial["status"] = status
            rows = {row[0]: row[1:] for row in qualification_matrix({"campaign": "release", "observations": observations})}
            self.assertEqual(rows["Multiline paste"][0], expected)
        stable_partial["status"] = "pass"
        partial = next(row for row in observations if row["host"] == "preview" and row["mode"] == "kitty"
                       and row["case"] == "shift-enter" and row["width"] == 80)
        partial["status"] = "inconclusive"
        rows = {row[0]: row[1:] for row in qualification_matrix({"campaign": "release", "observations": observations})}
        self.assertEqual(rows["Shift+Enter"][1], "INCONCLUSIVE")

    def test_run_spec_planning_does_not_require_other_combinations(self):
        document = {"channels": ["stable"], "widths": [80], "heights": [24],
                    "run_specs": [{"channel": "stable", "path": "herdr", "mode": "legacy", "cases": ["letter-a"]}],
                    "observations": [{"host": "stable", "path": "herdr", "mode": "legacy", "phase": 1,
                                      "width": 120, "height": 30, "case": "letter-a", "status": "not_run"}]}
        self.assertEqual(summarize(document)["coverage_missing"], 2)
        document["observations"][0]["mode"] = "kitty"
        with self.assertRaisesRegex(ValueError, "outside the declared run matrix"):
            summarize(document)

    def test_direct_legacy_limit_requires_unsupported_from_every_channel(self):
        observations = [{"host": host, "case": "shift-enter", "path": "direct", "mode": "legacy", "status": status}
                        for host, status in (("stable", "unsupported"), ("preview", "not_run"))]
        rows = {row[0]: row[1:] for row in qualification_matrix({"channels": ["stable", "preview"], "observations": observations})}
        self.assertEqual(rows["Shift+Enter"][1], "PARTIAL")

    def test_qualification_matrix_keeps_proven_input_visible_when_geometry_is_unavailable(self):
        observations = [{"host": host, "case": case, "path": "herdr", "mode": "legacy", "status": "pass", "width": 120}
                        for host in ("stable", "preview") for case in ("letter-a", "shift-letter")]
        observations += [{"host": host, "case": "letter-a", "path": "herdr", "mode": "legacy", "status": "not_run", "width": 160}
                         for host in ("stable", "preview")]
        observations += [{"host": host, "case": "shift-enter", "path": "direct", "mode": "legacy", "status": status}
                         for host in ("stable", "preview") for status in ("unsupported", "not_run")]
        observations += [{"host": "stable", "case": "shift-enter", "path": "direct", "mode": "kitty", "status": "inconclusive"},
                         {"host": "preview", "case": "shift-enter", "path": "direct", "mode": "kitty", "status": "pass"},
                         {"host": "preview", "case": "shift-enter", "path": "direct", "mode": "kitty", "status": "not_run"}]
        result = {"channels": ["stable", "preview"], "observations": observations}
        rows = {row[0]: row[1:] for row in qualification_matrix(result)}
        self.assertEqual(rows["Printable keys"][0], "PARTIAL")
        self.assertEqual(rows["Shift+Enter"][1:], ("X - becomes Enter", "INCONCLUSIVE"))
        self.assertEqual(rows["Multiline paste"][0], "NOT TESTED")
        self.assertEqual(rows["Resize 120 -> 80"][0], "NOT TESTED")
        observations[3]["status"] = "not_run"  # Preview has no passing shift-letter observation.
        self.assertEqual({row[0]: row[1] for row in qualification_matrix(result)}["Printable keys"], "NOT TESTED")
        observations[3]["status"] = "fail"
        self.assertEqual({row[0]: row[1] for row in qualification_matrix(result)}["Printable keys"], "FAIL")

    def test_herdr_protocol_label_requires_every_run_to_prove_transport(self):
        result = {"hosts": [{"channel": "stable", "runs": [{"nonce": "one", "path": "herdr", "mode": "native"},
                                                                  {"nonce": "two", "path": "herdr", "mode": "native"}]}],
                  "observations": [{"host": "stable", "nonce": "one", "path": "herdr", "mode": "native",
                                    "input_reader": "windows-console",
                                    "input_transport": "win32-serialized"}]}
        self.assertEqual(herdr_protocol_label(result), "Herdr default (UNKNOWN)*")
        result["observations"].append({"host": "stable", "nonce": "two", "path": "herdr", "mode": "native",
                                       "input_reader": "windows-console",
                                       "input_transport": "win32-serialized"})
        self.assertEqual(herdr_protocol_label(result), "Win32 (Herdr)*")
        result["hosts"].append({"channel": "preview", "runs": [{"nonce": "one", "path": "herdr", "mode": "native"}]})
        self.assertEqual(herdr_protocol_label(result), "Herdr default (UNKNOWN)*")

    def test_release_success_requires_stable_win32_runtime_evidence(self):
        observations = [{**self.evidence, "case": "letter-a", "host": "stable", "path": "herdr", "mode": "legacy",
                         "hex": "61", "phase": phase, "width": width, "height": height,
                         "outer_geometry": [width, height], "final_outer_geometry": [width, height],
                         "nonce": "owned", "capture_id": str(phase)}
                        for phase, (width, height) in enumerate(((120, 30), (80, 24), (80, 30)), 1)]
        run = {"nonce": "owned", "path": "herdr", "mode": "legacy", "pid": 123, "hwnd": 456,
               "elevated": False, "image_identity": "image-s", "installation_identity": "install-s"}
        document = {"campaign": "release", "channels": ["stable"], "widths": [80], "heights": [24],
                    "run_specs": [{"channel": "stable", "path": "herdr", "mode": "legacy", "cases": ["letter-a"]}],
                    "observations": observations, "hosts": [{"channel": "stable", "runs": [run]}],
                    "controller_elevated": False}
        incomplete = summarize(document)
        self.assertEqual(incomplete["counts"]["pass"], 3)
        self.assertEqual(incomplete["coverage_missing"], 0)
        self.assertFalse(incomplete["observed_checks_passed"])
        observations[0].update(input_reader="windows-console", input_transport="win32-serialized")
        self.assertTrue(summarize(document)["observed_checks_passed"])

    def test_empty_partial_and_duplicate_reports_never_become_green(self):
        self.assertFalse(summarize({})["observed_checks_passed"])
        row = {**self.evidence, "case": "letter-a", "host": "stable", "path": "herdr", "mode": "legacy", "hex": "61", "phase": 1,
               "width": 120, "height": 30, "outer_geometry": [120, 30], "final_outer_geometry": [120, 30]}
        self.assertFalse(summarize({"observations": [row]})["observed_checks_passed"])
        with self.assertRaisesRegex(ValueError, "Duplicate"):
            summarize({"observations": [row, row]})
        later = {**row, "phase": 2, "width": 80, "height": 24, "outer_geometry": [80, 24], "final_outer_geometry": [80, 24]}
        host = {"channel": "stable", "runs": [{"nonce": "owned", "path": "herdr", "mode": "legacy", "pid": 123, "hwnd": 456,
                                                "elevated": False, "image_identity": "image-s", "installation_identity": "install-s"}]}
        row.update(nonce="owned", capture_id="first")
        later.update(nonce="owned", capture_id="second")
        self.assertEqual(summarize({"observations": [row, later], "hosts": [host], "controller_elevated": False})["counts"]["pass"], 2)
        stale = {**later, "capture_id": "first"}
        self.assertEqual(summarize({"observations": [row, stale], "hosts": [host], "controller_elevated": False})["counts"]["inconclusive"], 1)
        forged_hosts = [{"channel": name, "runs": [{}]} for name in ("stable", "preview")]
        partial = summarize({"observations": [row], "hosts": forged_hosts})
        self.assertFalse(partial["observed_checks_passed"])
        self.assertGreater(partial["coverage_missing"], 0)
        for status in ["unsupported", "not_run", "inconclusive"]:
            self.assertEqual(verdict(self.cases["letter-a"], "legacy", {**row, "status": status})[0], status)

    def test_malformed_native_records_are_inconclusive_not_exceptions(self):
        evidence = {**self.evidence, "scans": [30]}
        for records in [None, 7, "records", [None], [[1, 1, 1, 65, 30, 97, None]],
                        [[True, 1, 1, 65, 30, 97, 0]], [[1, 1, 1, 65, 30, 97, "0"]]]:
            self.assertEqual(verdict(self.cases["letter-a"], "native", {**evidence, "records": records})[0], "inconclusive")

    def test_known_host_gap_only_exempts_observed_terminal_versions_and_cases(self):
        row = {**self.evidence, "case": "shift-enter", "host": "stable", "path": "direct", "mode": "mok2", "hex": "0d", "phase": 1,
               "width": 120, "height": 30, "outer_geometry": [120, 30], "final_outer_geometry": [120, 30],
               "nonce": "owned", "capture_id": "fresh"}
        for path, version, raw, expected in [("direct", "1.24.11911.0", "0d", "unsupported"),
                                             ("direct", "1.25.1912.0", "0d", "unsupported"),
                                             ("direct", "1.26.1.0", "0d", "fail"),
                                             ("herdr", "1.24.11911.0", "0d", "fail"),
                                             ("direct", "1.24.11911.0", "", "fail")]:
            observation = {**row, "path": path, "hex": raw}
            run = {"nonce": "owned", "path": path, "mode": "mok2", "pid": 123, "hwnd": 456, "terminal_version": version,
                   "elevated": False, "image_identity": "image-s", "installation_identity": "install-s"}
            document = {"observations": [observation], "hosts": [{"channel": "stable", "runs": [run]}], "controller_elevated": False}
            result = summarize(document)["observations"][0]
            self.assertEqual(result["status"], expected)
            self.assertEqual(result["failure_scope"], "direct_host" if path == "direct" else "through_herdr_not_yet_attributed")
            document["controller_elevated"] = True
            self.assertEqual(summarize(document)["observations"][0]["status"], "inconclusive")
            document["controller_elevated"] = False
            run["elevated"] = True
            self.assertEqual(summarize(document)["observations"][0]["status"], "inconclusive")

    def test_direct_mok_legacy_fallback_is_case_and_version_bounded(self):
        case = self.cases["shift-tab"]
        direct = {**self.evidence, "path": "direct", "mode": "mok2", "hex": "1b5b5a"}
        self.assertTrue(known_host_gap(direct, case, "1.25.2607.10002"))
        self.assertFalse(known_host_gap(direct, case, "1.26.0.0"))
        self.assertFalse(known_host_gap({**direct, "path": "herdr"}, case, "1.25.2607.10002"))
        self.assertFalse(known_host_gap({**direct, "hex": "1b5b32373b323b397e"}, case, "1.25.2607.10002"))

    def test_duplicate_channel_identity_is_rejected_at_both_stages(self):
        hosts = [{"channel": name, "launcher_identity": name + "-exe", "installation_identity": name + "-dir",
                  "runs": [{"image_identity": name + "-image", "installation_identity": name + "-dir", "process_identity": name + "-pid/start"}]}
                 for name in ("stable", "preview")]
        self.assertEqual(channel_identity_errors(hosts), [])
        for field in ("launcher_identity", "installation_identity"):
            duplicate = copy.deepcopy(hosts)
            duplicate[1][field] = duplicate[0][field]
            self.assertTrue(channel_identity_errors(duplicate))
            self.assertTrue(summarize({"hosts": duplicate})["errors"])
        for field in ("image_identity", "installation_identity", "process_identity"):
            duplicate = copy.deepcopy(hosts)
            duplicate[1]["runs"][0][field] = duplicate[0]["runs"][0][field]
            self.assertTrue(channel_identity_errors(duplicate))


class WindowsSmokeOwnershipTests(unittest.TestCase):
    @staticmethod
    def root():
        from pathlib import Path
        return Path(__file__).resolve().parents[1]

    def test_scripts_share_owned_launcher_and_result_authority(self):
        for name in ("windows_tui_compat.ps1", "windows_smoke_conpty_path.ps1"):
            with self.subTest(script=name):
                text = (self.root() / "scripts" / name).read_text(encoding="utf-8-sig")
                self.assertIn("windows_smoke_helpers.ps1", text)
                self.assertRegex(text, r"\$exitCode\s*=\s*Complete-WindowsSmoke")
                self.assertRegex(text, r"(?m)^exit \$exitCode$")
                self.assertIn("Invoke-SmokeHerdr", text)
                self.assertNotRegex(text, r"(?i)Stop-Process|taskkill|Start-Process|global:LASTEXITCODE")
                self.assertNotIn("GetTempPath", text)
                self.assertNotRegex(text, r"&\s+\$exe\b")

    def test_conpty_probe_does_not_launch_msvc_background_helpers(self):
        text = (self.root() / "scripts/windows_smoke_conpty_path.ps1").read_text(encoding="utf-8-sig")
        helper = (self.root() / "scripts/windows_input/windows_smoke_helpers.ps1").read_text(encoding="utf-8-sig")
        self.assertIn("rust-lld.exe", helper)
        self.assertIn('linker=$($toolchain.Linker)', helper)
        self.assertIn("rust-toolchain.toml", helper)
        self.assertIn("-CrateType cdylib", text)
        self.assertIn("Join-Path $context.Root 'fake-conpty'", text)

    def test_job_ownership_has_no_pid_reopen_or_post_launch_assignment(self):
        helper = self.root() / "scripts/windows_input/windows_smoke_helpers.ps1"
        self.assertTrue(helper.is_file(), "missing suspended-launch ownership helper")
        text = helper.read_text(encoding="utf-8-sig")
        self.assertIn("CREATE_SUSPENDED", text)
        self.assertIn("KILL_ON_JOB_CLOSE", text)
        self.assertIn("QueryInformationJobObject", text)
        self.assertNotRegex(text, r"(?i)OpenProcess\(|GetProcessById|Stop-Process|taskkill")
        start = text[text.index("public SmokeProcess Start("):]
        self.assertLess(start.index("CreateProcessW("), start.index("AssignProcessToJobObject("))
        self.assertLess(start.index("AssignProcessToJobObject("), start.index("ResumeThread("))

    def run_ps(self, body, inherited=None, files=None):
        import json
        import os
        from pathlib import Path
        import shutil
        import subprocess
        import sys
        import tempfile
        if os.name != "nt":
            self.skipTest("Windows Job/PowerShell execution requires native Windows")
        shell = shutil.which(os.environ.get("HERDR_SMOKE_TEST_POWERSHELL", "powershell.exe"))
        self.assertIsNotNone(shell, "the selected PowerShell host is required on Windows")
        helper = self.root() / "scripts/windows_input/windows_smoke_helpers.ps1"
        self.assertTrue(helper.is_file(), "missing suspended-launch ownership helper")
        temporary = self.root() / "target/tmp"
        temporary.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="smoke-contract-", dir=temporary) as directory:
            for name, content in (files or {}).items():
                path = Path(directory) / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(content, encoding="utf-8")
            quote = lambda value: "'" + str(value).replace("'", "''") + "'"
            private_inherited = {key: str(Path(directory) / value) if value.startswith("DO_NOT_USE_") else value
                                 for key, value in (inherited or {}).items()}
            bootstrap = dict(os.environ)
            bootstrap["RUSTUP_HOME"] = os.environ.get("RUSTUP_HOME", str(Path(os.environ["USERPROFILE"]) / ".rustup"))
            for key in ("HOME", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "TEMP", "TMP", "TMPDIR",
                        "XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_RUNTIME_DIR"):
                path = Path(directory) / "host" / key
                path.mkdir(parents=True, exist_ok=True)
                bootstrap[key] = str(path)
            bootstrap["HOMEDRIVE"] = Path(bootstrap["USERPROFILE"]).drive
            bootstrap["HOMEPATH"] = bootstrap["USERPROFILE"][len(bootstrap["HOMEDRIVE"]):]
            bootstrap["PSModuleAnalysisCachePath"] = str(Path(directory) / 'host/ModuleAnalysisCache')
            injected = "".join("[Environment]::SetEnvironmentVariable(" + quote(key) + "," + quote(value) + ",'Process'); "
                               for key, value in private_inherited.items())
            script = ("$ErrorActionPreference='Stop'; . " + quote(helper) + "; "
                      "$testRoot=" + quote(directory) + "; "
                      "$python=" + quote(sys.executable) + "; " + injected + body)
            harness = Path(directory) / 'harness.ps1'
            harness.write_text(script, encoding='utf-8-sig')
            result = subprocess.run([shell, "-NoLogo", "-NoProfile", "-NonInteractive",
                                     "-ExecutionPolicy", "Bypass", "-File", str(harness)], cwd=self.root(),
                                    env=bootstrap, stdin=subprocess.DEVNULL, capture_output=True,
                                    encoding="utf-8", errors="replace", timeout=150)
            report_file = Path(directory) / "result.json"
            report = json.loads(report_file.read_text(encoding="utf-8-sig")) if report_file.exists() else None
            self.assertIsNotNone(report, result.stdout + result.stderr)
            return result, report

    def completion(self, setup="", runtime="", stop="", delete=""):
        return ("$ctx=New-WindowsSmokeContext -Name 'contract' -Root $testRoot; "
                "$result=[ordered]@{runtime='PASS'}; $runtimeError=$null; " + setup + runtime +
                "; $code=Complete-WindowsSmoke -Context $ctx -Result $result "
                "-RuntimeError $runtimeError -Stop { " + stop + " } -Delete { " + delete + " }; "
                "exit $code")

    def test_normal_cleanup_is_success_and_proves_job_empty(self):
        result, report = self.run_ps(self.completion())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report["cleanup"], "PASS")
        self.assertEqual(report["cleanup_mode"], "graceful")
        self.assertEqual(report["active_processes"], 0)

    def test_herdr_control_and_cleanup_share_explicit_private_session(self):
        result, report = self.run_ps("""
$ctx=New-WindowsSmokeContext -Name 'explicit-session' -Root $testRoot;
$ctx.Exe=$python; $ctx.ServerStarted=$true;
$script:commands=New-Object 'Collections.Generic.List[object]';
function Invoke-SmokeCommand {
    param($Context, $Command, [string[]]$Arguments, $TimeoutMilliseconds, [switch]$Interactive)
    $script:commands.Add($Arguments);
    return [pscustomobject]@{ExitCode=0; Output='{}'; Error=''};
}
$result=[ordered]@{runtime='PASS'; commands=$script:commands};
Invoke-SmokeHerdr -Context $ctx -Arguments @('config','check') | Out-Null;
$code=Complete-WindowsSmoke -Context $ctx -Result $result;
exit $code;
""", {"HERDR_SESSION": "DO_NOT_USE_INHERITED_SESSION"})
        self.assertEqual(result.returncode, 0, result.stderr)
        session = report["session"]
        self.assertRegex(session, r"^explicit-session-[0-9a-f]{32}$")
        self.assertEqual(report["commands"], [
            ["--session", session, "config", "check"],
            ["--session", session, "session", "stop", session, "--json"],
            ["--session", session, "session", "delete", session, "--json"],
        ])

    def test_session_prefix_is_ascii_and_bounded_with_full_guid(self):
        from pathlib import Path
        cases = [
            ("ci-conpty-invalid-windows-2022-37462565646-1", "ci-conpty-invalid-windows-2022-"),
            ("A" * 30, "A" * 30),
            ("A" * 31, "A" * 31),
            ("A" * 32, "A" * 31),
            ("Smoke_界éİK" * 4, ("Smoke_----" * 4)[:31]),
        ]
        for name, prefix in cases:
            with self.subTest(name=name):
                result, report = self.run_ps("""
$ctx=New-WindowsSmokeContext -Name 'NAME';
$ctx.Exe=$python; $ctx.ServerStarted=$true;
$script:commands=New-Object 'Collections.Generic.List[object]';
function Invoke-SmokeCommand {
    param($Context, $Command, [string[]]$Arguments, $TimeoutMilliseconds, [switch]$Interactive)
    $script:commands.Add($Arguments);
    return [pscustomobject]@{ExitCode=0; Output='{}'; Error=''};
}
$result=[ordered]@{runtime='PASS'; commands=$script:commands; environment_session=$env:HERDR_SESSION};
Invoke-SmokeHerdr -Context $ctx -Arguments @('--version') | Out-Null;
$code=Complete-WindowsSmoke -Context $ctx -Result $result;
Copy-Item -LiteralPath (Join-Path $ctx.Root 'result.json') -Destination (Join-Path $testRoot 'result.json');
if ($code -eq 0) { Remove-Item -LiteralPath $ctx.Root -Recurse -Force };
exit $code;
""".replace("'NAME'", "'" + name + "'"))
                self.assertEqual(result.returncode, 0, result.stderr)
                session = report["session"]
                self.assertEqual(session[:-33], prefix)
                self.assertRegex(session[-33:], r"^-[0-9a-f]{32}$")
                self.assertTrue(session.isascii())
                self.assertLessEqual(len(session.encode("utf-8")), 64)
                self.assertEqual(Path(report["root"]).name, session)
                self.assertEqual(report["environment_session"], session)
                self.assertEqual(report["commands"], [
                    ["--session", session, "--version"],
                    ["--session", session, "session", "stop", session, "--json"],
                    ["--session", session, "session", "delete", session, "--json"],
                ])
                self.assertEqual(report["cleanup"], "PASS")
                self.assertEqual(report["active_processes"], 0)

    def test_cleanup_failure_makes_script_nonzero(self):
        result, report = self.run_ps(self.completion(stop="throw 'cleanup sentinel'"))
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(report["cleanup"], "FAIL")
        self.assertIn("cleanup sentinel", " ".join(report["cleanup_errors"]))
        self.assertEqual(report["active_processes"], 0)

    def test_delete_failure_is_nonzero_without_claiming_forced_cleanup(self):
        result, report = self.run_ps(self.completion(delete="throw 'delete sentinel'"))
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(report["cleanup"], "FAIL")
        self.assertEqual(report["cleanup_mode"], "graceful")
        self.assertEqual(report["active_processes"], 0)
        self.assertIn("delete sentinel", " ".join(report["cleanup_errors"]))

    def test_incomplete_runtime_is_not_success(self):
        result, report = self.run_ps(self.completion(setup="$result.runtime='PENDING';"))
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(report["runtime"], "FAIL")
        self.assertEqual(report["cleanup"], "PASS")

    def test_cleanup_failure_does_not_replace_runtime_error_or_exit(self):
        result, report = self.run_ps(self.completion(
            runtime="try { $e=New-Object Exception('runtime sentinel'); $e.Data['ExitCode']=23; throw $e } catch { $runtimeError=$_ }",
            stop="throw 'cleanup sentinel'"))
        self.assertEqual(result.returncode, 23, result.stderr)
        self.assertIn("runtime sentinel", report["runtime_error"])
        self.assertIn("runtime sentinel", result.stderr)
        self.assertIn("cleanup sentinel", " ".join(report["cleanup_errors"]))

    def test_inherited_environment_is_private_and_restored(self):
        keys = ["HOME", "USERPROFILE", "HOMEDRIVE", "HOMEPATH", "APPDATA", "LOCALAPPDATA",
                "XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_RUNTIME_DIR",
                "HERDR_HOME", "HERDR_CONFIG_PATH", "CODEX_HOME", "KIMI_CODE_HOME", "TEMP", "TMP", "TMPDIR", "PSModuleAnalysisCachePath"]
        cleared = ["HERDR_SOCKET_PATH", "HERDR_CLIENT_SOCKET_PATH", "HERDR_WORKSPACE_ID",
                   "HERDR_TAB_ID", "HERDR_PANE_ID", "HERDR_STARTUP_CWD", "HERDR_BIN_PATH", "HERDR_ENV"]
        inherited = {key: "DO_NOT_USE_INHERITED_" + key for key in keys + cleared}
        # PowerShell and Add-Type require a valid initial temporary directory.
        for key in ("TEMP", "TMP"):
            inherited.pop(key)
        child = "import os,json; print(json.dumps({k:os.environ.get(k) for k in " + repr(keys + cleared) + "}))"
        body = ("$before=@{}; foreach($key in @(" + ",".join("'" + key + "'" for key in inherited) +
                ")) { $before[$key]=[Environment]::GetEnvironmentVariable($key) }; "
                "$ctx=New-WindowsSmokeContext -Name 'isolation' -Root $testRoot; "
                "$result=[ordered]@{runtime='PASS'; environment=@{}}; "
                "foreach($key in @(" + ",".join("'" + key + "'" for key in keys + cleared) +
                ")) { $result.environment[$key]=[Environment]::GetEnvironmentVariable($key) }; "
                "$call=Invoke-SmokeCommand -Context $ctx -Command $python -Arguments @('-c','" + child.replace("'", "''") + "'); "
                "$result.child_environment=$call.Output | ConvertFrom-Json; "
                "$code=Complete-WindowsSmoke -Context $ctx -Result $result -Stop {} -Delete {}; "
                "foreach($key in $before.Keys) { if ([Environment]::GetEnvironmentVariable($key) -ne $before[$key]) { throw ('not restored: '+$key) } }; exit $code")
        result, report = self.run_ps(body, inherited)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report["child_environment"], report["environment"])
        for key in keys:
            value = report["environment"][key]
            self.assertIsNotNone(value, key)
            self.assertNotIn("DO_NOT_USE", value, key)
            if key not in ("HOMEDRIVE", "HOMEPATH"):
                self.assertTrue(value.startswith(report["root"]), (key, value))
        for key in cleared:
            self.assertIsNone(report["environment"][key], key)

    def test_unverifiable_job_refuses_pid_fallback(self):
        body = self.completion(setup="""
$ctx.Job.Dispose();
$ctx.Job=New-Object PSObject;
$ctx.Job | Add-Member ScriptMethod WaitEmpty { param($milliseconds) throw 'ownership unavailable' };
$ctx.Job | Add-Member ScriptMethod Terminate { throw 'ownership unavailable' };
$ctx.Job | Add-Member ScriptMethod Dispose {};
$ctx.Job | Add-Member ScriptProperty ActiveProcesses { throw 'ownership unavailable' };
$ctx.Server=[pscustomobject]@{Id=$PID};
function Stop-Process { throw 'BARE_PID_KILL' };
function taskkill.exe { throw 'BARE_PID_KILL' };
""")
        result, report = self.run_ps(body)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(report["cleanup"], "FAIL")
        self.assertIsNone(report["active_processes"])
        self.assertNotIn("BARE_PID_KILL", str(report))

    def test_recycled_pid_field_is_not_a_cleanup_authority(self):
        result, report = self.run_ps(self.completion(setup="""
$ctx.Server=[pscustomobject]@{Id=$PID};
function Stop-Process { throw 'BARE_PID_KILL' };
function taskkill.exe { throw 'BARE_PID_KILL' };
"""))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report["active_processes"], 0)

    def test_owned_process_and_descendant_force_cleanup_is_not_a_pass(self):
        child = "import pathlib,subprocess,sys,time; subprocess.Popen([sys.executable,'-c','import time; time.sleep(120)']); pathlib.Path(sys.argv[1]).write_text('ready'); time.sleep(120)"
        result, report = self.run_ps(self.completion(setup=(
            "$ready=Join-Path $testRoot 'ready'; "
            "$child=Start-SmokeProcess -Context $ctx -Command $python -Arguments @('-c','" + child.replace("'", "''") + "',$ready); "
            "$deadline=[DateTime]::UtcNow.AddSeconds(15); "
            "while (-not (Test-Path $ready)) { if ([DateTime]::UtcNow -gt $deadline) { throw 'child not ready' }; Start-Sleep -Milliseconds 25 }; "
            "$result.active_before=$ctx.Job.ActiveProcesses; $ctx.CleanupTimeoutMilliseconds=100;")))
        self.assertNotEqual(result.returncode, 0)
        self.assertGreaterEqual(report["active_before"], 2)
        self.assertEqual(report["cleanup_mode"], "forced")
        self.assertEqual(report["active_processes"], 0)

    def test_owned_process_normal_exit_preserves_native_exit_and_output(self):
        result, report = self.run_ps(self.completion(setup="""
$call=Invoke-SmokeCommand -Context $ctx -Command $python -Arguments @('-c','print("owned output"); raise SystemExit(7)');
$result.native_exit=$call.ExitCode; $result.native_output=$call.Output;
"""))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report["native_exit"], 7)
        self.assertIn("owned output", report["native_output"])
        self.assertEqual(report["cleanup_mode"], "graceful")
        self.assertEqual(report["active_processes"], 0)

    def test_conpty_override_is_explicit_limited_and_restored(self):
        result, report = self.run_ps("""
$ctx=New-WindowsSmokeContext -Name 'mode' -Root $testRoot;
$result=[ordered]@{runtime='PASS'; inherited=[Environment]::GetEnvironmentVariable('HERDR_WINDOWS_CONPTY')};
Set-SmokeConptyMode -Context $ctx -Mode system;
$result.explicit=$env:HERDR_WINDOWS_CONPTY;
Set-SmokeConptyMode -Context $ctx -Mode auto;
$result.automatic=[Environment]::GetEnvironmentVariable('HERDR_WINDOWS_CONPTY');
try { Set-SmokeConptyMode -Context $ctx -Mode arbitrary; throw 'accepted invalid mode' } catch {
    $result.rejected=$_.FullyQualifiedErrorId -like '*ParameterArgumentValidationError*';
}
$code=Complete-WindowsSmoke -Context $ctx -Result $result -Stop {} -Delete {};
if ($env:HERDR_WINDOWS_CONPTY -ne 'inherited-mode') { throw 'override not restored' }; exit $code;
""", {"HERDR_WINDOWS_CONPTY": "inherited-mode"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIsNone(report["inherited"])
        self.assertEqual(report["explicit"], "system")
        self.assertIsNone(report["automatic"])
        self.assertTrue(report["rejected"])

    def test_entrypoints_expose_report_and_explicit_mode(self):
        for name in ("windows_tui_compat.ps1", "windows_smoke_conpty_path.ps1"):
            text = (self.root() / "scripts" / name).read_text(encoding="utf-8-sig")
            self.assertIn("[switch]$PassThru", text)
            self.assertIn("[ValidateSet('auto', 'system')]", text)
            self.assertIn("Set-SmokeConptyMode -Context $context -Mode $ConptyMode", text)
            self.assertIn("runtime_stage", text)

    def ci_step_script(self, name):
        lines = (self.root() / ".github/workflows/ci.yml").read_text(encoding="utf-8").splitlines()
        first = lines.index("      - name: " + name)
        start = next(i for i in range(first, len(lines)) if lines[i] == "        run: |") + 1
        body = []
        for line in lines[start:]:
            if line and not line.startswith("          "):
                break
            body.append(line[10:])
        return "\n".join(body)

    def run_fake_ci_step(self, name, fake_script, filename, failure_kind="bundle"):
        body = self.ci_step_script(name)
        wrapper = """
$env:SMOKE_TEST_CALLS=Join-Path $testRoot 'calls.txt';
$env:SMOKE_FAILURE_KIND='FAILURE_KIND';
$failure=$null; $code=0;
Push-Location $testRoot;
try { & {
CI_BODY
}; $code=$LASTEXITCODE } catch { $failure=$_.ToString(); $code=1 } finally { Pop-Location };
$result=[ordered]@{caller_exit=$code; failure=$failure; calls=@([IO.File]::ReadAllLines($env:SMOKE_TEST_CALLS))};
$result | ConvertTo-Json -Depth 8 | Set-Content -Encoding utf8 (Join-Path $testRoot 'result.json');
exit $code;
""".replace("CI_BODY", body).replace("'FAILURE_KIND'", "'" + failure_kind + "'")
        return self.run_ps(wrapper, files={
            "scripts/" + filename: fake_script,
            "target/debug/herdr.exe": "fixture; never executed",
            "target/x86_64-pc-windows-msvc/debug/herdr.exe": "fixture; never executed",
        })

    def test_ci_first_failure_cannot_be_hidden_by_second_success(self):
        fake = """
param($ExePath, $Shell)
Add-Content $env:SMOKE_TEST_CALLS $Shell;
if ($Shell -eq 'powershell') { exit 23 }; exit 0;
"""
        result, report = self.run_fake_ci_step("Replay PowerShell terminal compatibility", fake, "windows_tui_compat.ps1")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(report["calls"], ["powershell"])

    def test_ci_single_smoke_checks_its_exit(self):
        body = self.ci_step_script("Smoke ConPTY pane")
        self.assertIn("if ($LASTEXITCODE -ne 0)", body)

    def invalid_ci_case(self, failure_kind):
        fake = """
param($ExePath, $Session, $ConptyMode='auto', [switch]$PassThru)
Add-Content $env:SMOKE_TEST_CALLS $ConptyMode;
$code=17;
$report=[pscustomobject]@{runtime='FAIL'; runtime_stage='workspace_create'; runtime_error="Herdr's app-local ConPTY bundle is invalid: fixture"; server_stderr=''; exit_code=17; cleanup='PASS'; cleanup_mode='graceful'; active_processes=0};
if ($env:SMOKE_FAILURE_KIND -eq 'compile') { $report.runtime_stage='shell_launcher'; $report.runtime_error='compiler failed' };
if ($env:SMOKE_FAILURE_KIND -eq 'other') { $report.runtime_error='unrelated workspace failure' };
if ($ConptyMode -eq 'system') { $report.runtime='PASS'; $report.exit_code=0; $report.runtime_error=$null; $code=0 };
if ($PassThru) { $report }; exit $code;
"""
        return self.run_fake_ci_step("Verify invalid bundle is rejected and system override recovers", fake,
                                     "windows_smoke_conpty_path.ps1", failure_kind)

    def test_ci_invalid_bundle_requires_reason_and_explicit_system_recovery(self):
        result, report = self.invalid_ci_case("bundle")
        self.assertEqual(result.returncode, 0, result.stderr + str(report))
        self.assertEqual(report["calls"], ["auto", "system"])

    def test_ci_compiler_failure_is_not_a_bundle_rejection(self):
        result, report = self.invalid_ci_case("compile")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(report["calls"], ["auto"])

    def test_ci_unrelated_runtime_failure_is_not_a_bundle_rejection(self):
        result, report = self.invalid_ci_case("other")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(report["calls"], ["auto"])

    def test_no_profile_launcher_uses_repository_toolchain_and_x64_guard(self):
        text = (self.root() / "scripts/windows_input/windows_smoke_helpers.ps1").read_text(encoding="utf-8-sig")
        self.assertIn('rust-toolchain.toml', text)
        self.assertNotIn('1.96.1', text)
        self.assertIn('Assert-SmokeX64Image', text)
        self.assertIn('New-SmokeShellLauncher', text)
        self.assertIn('"-NoProfile"', text)
        self.assertIn('Stdio::inherit()', text)
        for name in ("windows_tui_compat.ps1", "windows_smoke_conpty_path.ps1"):
            script = (self.root() / "scripts" / name).read_text(encoding="utf-8-sig")
            self.assertIn('Set-SmokeConfig $context $shellPath', script)
            self.assertNotIn('1.96.1', script)

    def test_private_launcher_forwards_no_profile_and_preserves_exit(self):
        result, report = self.run_ps(self.completion(setup="""
$source=Join-Path $testRoot 'argv_probe.rs';
[IO.File]::WriteAllText($source, 'fn main() { let args: Vec<String> = std::env::args().skip(1).collect(); println!("{:?}", args); std::process::exit(7); }');
$probe=Join-Path $testRoot 'powershell.exe';
Invoke-SmokeRustc -Context $ctx -Source $source -Output $probe;
$launcher=New-SmokeShellLauncher -Context $ctx -ShellPath $probe;
$call=Invoke-SmokeCommand -Context $ctx -Command $launcher -Arguments @('-NoExit','-Command','a "quoted" value');
$result.arguments=[string[]]($call.Output | ConvertFrom-Json); $result.child_exit=$call.ExitCode;
"""))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report["child_exit"], 7)
        self.assertEqual(report["arguments"], ["-NoLogo", "-NoProfile", "-NoExit", "-Command", 'a "quoted" value'])
        self.assertEqual(report["active_processes"], 0)

    def test_actual_entries_return_report_and_native_failure_to_same_host(self):
        for name in ("windows_tui_compat.ps1", "windows_smoke_conpty_path.ps1"):
            with self.subTest(entry=name):
                entry = str(self.root() / "scripts" / name).replace("'", "''")
                result, report = self.run_ps("""
$entryReport=& 'ENTRY' -ExePath $python -PassThru;
$entryExit=$LASTEXITCODE;
$result=[ordered]@{entry_exit=$entryExit; report_exit=$entryReport.exit_code; runtime=$entryReport.runtime; error=$entryReport.runtime_error; cleanup=$entryReport.cleanup; active=$entryReport.active_processes};
if ($entryReport.cleanup -eq 'PASS' -and $entryReport.active_processes -eq 0) { Remove-Item -LiteralPath $entryReport.root -Recurse -Force };
$result | ConvertTo-Json | Set-Content -Encoding utf8 (Join-Path $testRoot 'result.json');
exit $entryExit;
""".replace("ENTRY", entry))
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertEqual(report["entry_exit"], 2)
                self.assertEqual(report["report_exit"], 2)
                self.assertEqual(report["runtime"], "FAIL")
                self.assertIn("Herdr failed (2)", report["error"])
                self.assertEqual(report["cleanup"], "PASS")
                self.assertEqual(report["active"], 0)

    def test_actual_entries_return_native_failure_from_file_host(self):
        for name in ("windows_tui_compat.ps1", "windows_smoke_conpty_path.ps1"):
            with self.subTest(entry=name):
                entry = str(self.root() / "scripts" / name).replace("'", "''")
                result, report = self.run_ps(self.completion(setup="""
$hostExe=[Diagnostics.Process]::GetCurrentProcess().MainModule.FileName;
$call=Invoke-SmokeCommand -Context $ctx -Command $hostExe -Arguments @('-NoLogo','-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-File','ENTRY','-ExePath',$python) -TimeoutMilliseconds 60000;
$match=[regex]::Match($call.Output,'replay report: ([^\r\n]+)');
if (-not $match.Success) { throw ('no explicit report path: '+$call.Error) };
$inner=[IO.File]::ReadAllText((Join-Path $match.Groups[1].Value 'result.json')) | ConvertFrom-Json;
$result.child_exit=$call.ExitCode; $result.report_exit=$inner.exit_code; $result.child_error=$inner.runtime_error;
if ($inner.cleanup -eq 'PASS' -and $inner.active_processes -eq 0) { Remove-Item -LiteralPath $inner.root -Recurse -Force };
""".replace("ENTRY", entry)))
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(report["child_exit"], 2)
                self.assertEqual(report["report_exit"], 2)
                self.assertIn("Herdr failed (2)", report["child_error"])
                self.assertEqual(report["active_processes"], 0)

    def test_x64_guard_rejects_arm64_and_dll_images(self):
        result, report = self.run_ps(self.completion(setup="""
$bytes=New-Object byte[] 128; $bytes[0]=0x4d; $bytes[1]=0x5a; $bytes[60]=64;
$bytes[64]=0x50; $bytes[65]=0x45; $bytes[68]=0x64; $bytes[69]=0xaa; $bytes[86]=2;
$image=Join-Path $testRoot 'wrong.exe'; [IO.File]::WriteAllBytes($image,$bytes);
try { Assert-SmokeX64Image $image; $result.arm64_rejected=$false } catch { $result.arm64_rejected=$_.ToString() -like '*only x64*' };
$bytes[68]=0x64; $bytes[69]=0x86; $bytes[87]=0x20; [IO.File]::WriteAllBytes($image,$bytes);
try { Assert-SmokeX64Image $image; $result.dll_rejected=$false } catch { $result.dll_rejected=$_.ToString() -like '*not an executable image*' };
"""))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(report["arm64_rejected"])
        self.assertTrue(report["dll_rejected"])

    def test_missing_toolchain_is_reported_without_installing(self):
        result, report = self.run_ps(self.completion(setup="""
$ctx.RustupHome=Join-Path $testRoot 'missing-toolchain';
try { Get-SmokeToolchain $ctx; $result.rejected=$false } catch { $result.rejected=$_.ToString() -like '*no automatic install*' };
$result.commands=$ctx.CommandNumber; $result.created=Test-Path $ctx.RustupHome;
"""))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(report["rejected"])
        self.assertEqual(report["commands"], 0)
        self.assertFalse(report["created"])

    def exited_server_case(self, original_error):
        return self.run_ps("""
$ctx=New-WindowsSmokeContext -Name 'exited-server' -Root $testRoot;
$ctx.Server=Start-SmokeProcess -Context $ctx -Command $python -Arguments @('-c','raise SystemExit(7)');
if (-not $ctx.Server.Wait(5000)) { throw 'fixture did not exit' }; $ctx.ServerStarted=$true;
function Invoke-SmokeHerdr {
    param($Context, [string[]]$Arguments, $TimeoutMilliseconds)
    if ($Arguments[1] -eq 'stop') { throw 'must not stop an already exited owned server' };
}
$result=[ordered]@{runtime='PASS'}; $runtimeError=$null;
ORIGINAL_ERROR
$code=Complete-WindowsSmoke -Context $ctx -Result $result -RuntimeError $runtimeError;
exit $code;
""".replace("ORIGINAL_ERROR", original_error))

    def test_already_exited_server_keeps_runtime_error_without_false_cleanup_failure(self):
        result, report = self.exited_server_case("try { $failure=New-Object Exception('original runtime'); $failure.Data['ExitCode']=23; throw $failure } catch { $runtimeError=$_ }")
        self.assertEqual(result.returncode, 23, result.stderr)
        self.assertIn('original runtime', report['runtime_error'])
        self.assertEqual(report['cleanup'], 'PASS')
        self.assertEqual(report['shutdown'], 'already_exited')
        self.assertEqual(report['server_exit_code'], 7)
        self.assertEqual(report['active_processes'], 0)

    def test_unexpected_owned_server_exit_cannot_be_green(self):
        result, report = self.exited_server_case("")
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertEqual(report['runtime'], 'FAIL')
        self.assertEqual(report['cleanup'], 'PASS')
        self.assertEqual(report['server_exit_code'], 7)

    def test_report_write_failure_keeps_passthru_exit_truthful(self):
        result, report = self.run_ps("""
$ctx=New-WindowsSmokeContext -Name 'locked-report' -Root $testRoot;
$result=[ordered]@{runtime='PASS'};
$path=Join-Path $testRoot 'result.json';
$held=[IO.File]::Open($path,[IO.FileMode]::Create,[IO.FileAccess]::ReadWrite,[IO.FileShare]::None);
try { $code=Complete-WindowsSmoke -Context $ctx -Result $result -Stop {} -Delete {} } finally { $held.Dispose() };
$result.returned_exit=$code;
$result | ConvertTo-Json | Set-Content -Encoding utf8 $path;
exit $code;
""")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(report['exit_code'], report['returned_exit'])


if __name__ == "__main__":
    unittest.main()
