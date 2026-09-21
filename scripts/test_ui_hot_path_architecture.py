from __future__ import annotations

import re
import unittest
from pathlib import Path


PROJECT_ROOT = Path(__file__).resolve().parent.parent
HOT_PATH_SOURCES = (
    PROJECT_ROOT / "src" / "ui.rs",
    *sorted((PROJECT_ROOT / "src" / "ui").rglob("*.rs")),
    PROJECT_ROOT / "src" / "server" / "render_stream.rs",
)
APP_SERVER_SOURCES = (
    *sorted((PROJECT_ROOT / "src" / "app").rglob("*.rs")),
    *sorted((PROJECT_ROOT / "src" / "server").rglob("*.rs")),
)
# APP-014：投影/快照构建路径（HSR-05/APP-002 的 cwd 与 procfs 成本面）同样
# 禁止进程树检查等重查询；`foreground_job(` 在这些文件必须为 0。
PROJECTION_SOURCES = (
    PROJECT_ROOT / "src" / "server" / "client_shell.rs",
    PROJECT_ROOT / "src" / "app" / "creation.rs",
    PROJECT_ROOT / "src" / "app" / "api" / "session.rs",
)
TEST_MODULE = re.compile(r"(?m)^#\[cfg\(test\)\]\s*\nmod\s+\w+\s*\{")
INPUT_STATE_CALL = re.compile(r"(?:\.|::)input_state\b")
KEYBOARD_STATE_ANSI_CALL = re.compile(
    r"(?:\.|::)(?:keyboard_state_ansi|kitty_keyboard_state_ansi)\b"
)
AGGREGATE_STATE_CALLS = (
    (INPUT_STATE_CALL, "aggregate terminal input state; add a narrow accessor"),
    (KEYBOARD_STATE_ANSI_CALL, "formatted keyboard state"),
)
FORBIDDEN_CALLS = (
    *AGGREGATE_STATE_CALLS,
    (
        re.compile(r"(?:\.|::)screen_text_snapshot\b"),
        "formatted terminal screen snapshot",
    ),
    (
        re.compile(r"\bforeground_job\s*\("),
        "process-tree inspection",
    ),
)


def blank_non_newlines(chars: list[str], start: int, end: int) -> None:
    for index in range(start, end):
        if chars[index] != "\n":
            chars[index] = " "


def mask_comments_and_literals(source: str) -> str:
    chars = list(source)
    index = 0
    while index < len(source):
        if source.startswith("//", index):
            end = source.find("\n", index + 2)
            end = len(source) if end == -1 else end
            blank_non_newlines(chars, index, end)
            index = end
            continue

        if source.startswith("/*", index):
            depth = 1
            end = index + 2
            while end < len(source) and depth > 0:
                if source.startswith("/*", end):
                    depth += 1
                    end += 2
                elif source.startswith("*/", end):
                    depth -= 1
                    end += 2
                else:
                    end += 1
            blank_non_newlines(chars, index, end)
            index = end
            continue

        if source[index] == "r":
            quote = index + 1
            while quote < len(source) and source[quote] == "#":
                quote += 1
            if quote < len(source) and source[quote] == '"':
                suffix = '"' + "#" * (quote - index - 1)
                end = source.find(suffix, quote + 1)
                end = len(source) if end == -1 else end + len(suffix)
                blank_non_newlines(chars, index, end)
                index = end
                continue

        if source[index] == '"':
            end = index + 1
            while end < len(source):
                if source[end] == "\\":
                    end += 2
                elif source[end] == '"':
                    end += 1
                    break
                else:
                    end += 1
            blank_non_newlines(chars, index, min(end, len(source)))
            index = end
            continue

        if source[index] == "'":
            end = index + 2
            if index + 1 < len(source) and source[index + 1] == "\\":
                end += 1
            if end < len(source) and source[end] == "'":
                end += 1
                blank_non_newlines(chars, index, end)
                index = end
                continue

        index += 1

    return "".join(chars)


def production_code(source: str) -> str:
    code = mask_comments_and_literals(source)
    chars = list(code)
    search_from = 0

    while test_module := TEST_MODULE.search(code, search_from):
        depth = 0
        end = test_module.end() - 1
        while end < len(code):
            if code[end] == "{":
                depth += 1
            elif code[end] == "}":
                depth -= 1
                if depth == 0:
                    end += 1
                    break
            end += 1
        blank_non_newlines(chars, test_module.start(), end)
        code = "".join(chars)
        search_from = end

    return code


def find_violations(paths, rules) -> list[str]:
    violations: list[str] = []
    for path in paths:
        code = production_code(path.read_text(encoding="utf-8"))
        for pattern, description in rules:
            for match in pattern.finditer(code):
                line = code.count("\n", 0, match.start()) + 1
                relative_path = path.relative_to(PROJECT_ROOT)
                violations.append(f"{relative_path}:{line}: {description}")
    return violations


def rust_function_body(code: str, name: str) -> str:
    match = re.search(rf"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?fn\s+{name}\s*\(", code)
    if match is None:
        return ""
    start = code.find("{", match.end())
    if start == -1:
        return ""
    depth = 0
    for index in range(start, len(code)):
        if code[index] == "{":
            depth += 1
        elif code[index] == "}":
            depth -= 1
            if depth == 0:
                return code[start : index + 1]
    return ""


class UiHotPathArchitectureTests(unittest.TestCase):
    def test_render_hot_paths_avoid_known_expensive_runtime_queries(self) -> None:
        violations = find_violations(HOT_PATH_SOURCES, FORBIDDEN_CALLS)

        self.assertEqual(
            violations,
            [],
            "Render/layout code must not perform pane-scaled expensive reads:\n"
            + "\n".join(violations),
        )

    def test_app_and_server_avoid_aggregate_terminal_state(self) -> None:
        self.assertTrue(APP_SERVER_SOURCES, "No app/server Rust sources were discovered")
        violations = find_violations(APP_SERVER_SOURCES, AGGREGATE_STATE_CALLS)

        self.assertEqual(
            violations,
            [],
            "App/server code must use narrow terminal-state accessors:\n"
            + "\n".join(violations),
        )

    def test_projection_paths_avoid_expensive_runtime_queries(self) -> None:
        violations = find_violations(PROJECTION_SOURCES, FORBIDDEN_CALLS)

        self.assertEqual(
            violations,
            [],
            "Projection/snapshot construction must not perform pane-scaled "
            "expensive reads (APP-014):\n" + "\n".join(violations),
        )

    def test_scanner_ignores_non_production_references(self) -> None:
        source = '''
// runtime.input_state()
const EXAMPLE: &str = "runtime.input_state()";
#[cfg(test)]
mod tests {
    fn aggregate_state_test() { runtime.input_state(); }
}
fn production_after_tests() {}
'''
        code = production_code(source)
        self.assertNotRegex(code, FORBIDDEN_CALLS[0][0])
        self.assertIn("fn production_after_tests()", code)
        self.assertEqual(code.count("\n"), source.count("\n"))

    def test_scanner_checks_production_after_test_modules(self) -> None:
        source = '''
#[cfg(test)]
mod tests {
    const BRACES: &str = "}}";
}
fn render() { TerminalRuntime::input_state; }
'''
        self.assertRegex(production_code(source), FORBIDDEN_CALLS[0][0])

    def test_scanner_catches_each_aggregate_state_call(self) -> None:
        cases = (
            ("fn render() { runtime.input_state(); }", INPUT_STATE_CALL),
            ("fn render() { runtime.keyboard_state_ansi(); }", KEYBOARD_STATE_ANSI_CALL),
            ("fn render() { runtime.kitty_keyboard_state_ansi(); }", KEYBOARD_STATE_ANSI_CALL),
        )
        for source, pattern in cases:
            with self.subTest(source=source):
                self.assertRegex(production_code(source), pattern)

    def test_scanner_catches_imported_process_query(self) -> None:
        source = "fn render() { foreground_job(pid); }"
        self.assertRegex(production_code(source), FORBIDDEN_CALLS[3][0])

    def test_plugin_event_hooks_check_cached_subscribers_before_registry_load(self) -> None:
        # APP-001：emit_event 对每个可 hook 事件触发。注册表磁盘加载
        # （plugin_registry::try_load，经 refresh_installed_plugins 包装）
        # 不得出现在判订阅者之前的前缀路径上。
        registry_load_calls = ("plugin_registry::try_load", "refresh_installed_plugins")

        api_rs = production_code(
            (PROJECT_ROOT / "src" / "app" / "api.rs").read_text(encoding="utf-8")
        )
        emit_event = rust_function_body(api_rs, "emit_event")
        self.assertIn("run_plugin_event_hooks", emit_event, "emit_event body not found")
        for call in registry_load_calls:
            self.assertNotIn(
                call, emit_event, f"emit_event must not reach the registry via {call}"
            )

        runtime_rs = production_code(
            (PROJECT_ROOT / "src" / "app" / "api" / "plugins" / "runtime.rs").read_text(
                encoding="utf-8"
            )
        )
        hooks = rust_function_body(runtime_rs, "run_plugin_event_hooks")
        subscriber_check = hooks.find("plugin_subscribes_to_event")
        self.assertNotEqual(
            subscriber_check,
            -1,
            "run_plugin_event_hooks must pre-check cached subscribers before loading",
        )
        load_index = hooks.find("refresh_installed_plugins")
        self.assertNotEqual(
            load_index,
            -1,
            "run_plugin_event_hooks must still refresh the registry "
            "once a cached subscriber exists",
        )
        self.assertGreater(
            load_index,
            subscriber_check,
            "refresh_installed_plugins must not run before the cached-subscriber check",
        )
        try_load_index = hooks.find("plugin_registry::try_load")
        if try_load_index != -1:
            self.assertGreater(
                try_load_index,
                subscriber_check,
                "plugin_registry::try_load must not appear on the prefix path "
                "before the cached-subscriber check",
            )


    def test_overlay_renderers_never_write_scroll_state(self) -> None:
        # STATE-04：滚动窗口与一次性 reveal 只在视图计算阶段（compose 之前）
        # 更新；渲染函数是纯函数。浮层逐个点名守门——新增浮层渲染入口时把
        # 函数名加进来（chrome 面 tabs/workbench/mobile 仍是遗留面，见交付
        # 记录的显式 PENDING）。
        renderers = {
            PROJECT_ROOT / "src" / "client" / "shell" / "machines_overlay.rs": (
                "render_machines_view",
            ),
            PROJECT_ROOT / "src" / "client" / "shell" / "overlays.rs": ("render_help_overlay",),
            PROJECT_ROOT / "src" / "client" / "shell" / "command_palette.rs": (
                "render_command_palette",
            ),
            PROJECT_ROOT / "src" / "client" / "shell" / "snippets_overlay.rs": (
                "render_run_pick_machines",
            ),
        }
        write = re.compile(r"\.(?:scroll|reveal)\s*=[^=]")
        for path, names in renderers.items():
            code = production_code(path.read_text(encoding="utf-8"))
            for name in names:
                body = rust_function_body(code, name)
                self.assertNotEqual(body, "", f"{path.name}: {name} body not found")
                match = write.search(body)
                if match is not None:
                    line = body.count("\n", 0, match.start()) + 1
                    self.fail(
                        f"{path.name}: {name} 渲染期回写滚动状态"
                        f"（body 内第 {line} 行）：{match.group(0)!r}"
                    )

if __name__ == "__main__":
    unittest.main()
