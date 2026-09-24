use super::*;

// Codex is only a registry key here; behavior tests supply synthetic rules.
fn remote_manifest(version: &str, state: &str, contains: &str) -> String {
    format!(
        r#"
id = "codex"
version = "{version}"
min_engine_version = 1
updated_at = "2026-06-10T12:00:00Z"

[[rules]]
id = "test"
state = "{state}"
contains = ["{contains}"]
"#
    )
}

fn local_manifest(state: &str, contains: &str) -> String {
    format!(
        r#"
id = "codex"

[[rules]]
id = "test"
state = "{state}"
contains = ["{contains}"]
"#
    )
}

fn rules_manifest(rules: &str) -> String {
    format!(
        r#"
id = "codex"

{rules}
"#
    )
}

fn with_manifest_dirs<T>(name: &str, f: impl FnOnce() -> T) -> T {
    let _guard = crate::config::test_config_env_lock().lock().unwrap();
    let old_config = std::env::var_os("XDG_CONFIG_HOME");
    let old_state = std::env::var_os("XDG_STATE_HOME");
    let base = std::env::temp_dir().join(format!(
        "herdr-manifest-loader-{name}-{}",
        std::process::id()
    ));
    let config_dir = base.join("config");
    let state_dir = base.join("state");
    let _ = std::fs::remove_dir_all(&base);
    std::env::set_var("XDG_CONFIG_HOME", &config_dir);
    std::env::set_var("XDG_STATE_HOME", &state_dir);
    reload_manifests();
    let result = f();
    match old_config {
        Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }
    match old_state {
        Some(value) => std::env::set_var("XDG_STATE_HOME", value),
        None => std::env::remove_var("XDG_STATE_HOME"),
    }
    reload_manifests();
    let _ = std::fs::remove_dir_all(&base);
    result
}

fn write_remote_codex(content: &str) {
    let path = crate::detect::manifest_update::remote_manifest_path(Agent::Codex);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
    reload_manifests();
}

fn write_remote_codex_without_reload(content: &str) {
    let path = crate::detect::manifest_update::remote_manifest_path(Agent::Codex);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn write_local_codex(content: &str) {
    let path = override_path(Agent::Codex).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
    reload_manifests();
}

#[test]
fn codex_no_match_is_unknown_without_changing_other_agents() {
    with_manifest_dirs("no-match", || {
        write_local_codex(&local_manifest("working", "active-marker"));
        let explain = explain(Agent::Codex, "unmatched-marker");

        assert_eq!(explain.state, AgentState::Unknown);
        assert!(!explain.visible_idle);
        assert_eq!(
            explain.fallback_reason.as_deref(),
            Some("codex_state_ambiguous")
        );
        let other = fallback_explain(Some(Agent::Pi), None, false);
        assert_eq!(other.state, AgentState::Idle);
        assert_eq!(
            other.fallback_reason.as_deref(),
            Some(DEFAULT_KNOWN_AGENT_IDLE_FALLBACK)
        );
    });
}

#[test]
fn rule_semantics_apply_gates_priority_and_line_regex() {
    with_manifest_dirs("rule-semantics", || {
        write_local_codex(&rules_manifest(
            r#"
[[rules]]
id = "low_contains"
state = "idle"
priority = 1
contains = ["match"]

[[rules]]
id = "high_nested_gates"
state = "working"
priority = 10
contains = ["match"]
all = [
  { any = [{ regex = ["w[io]n"] }, { contains = ["fallback"] }] },
]
not = [
  { contains = ["blocked"] },
]

[[rules]]
id = "line_regex"
state = "blocked"
priority = 20
line_regex = ["^exact line$"]
"#,
        ));

        let high = explain(Agent::Codex, "match win");
        assert_eq!(high.state, AgentState::Working);
        assert_eq!(
            high.matched_rule.as_ref().map(|rule| rule.id.as_str()),
            Some("high_nested_gates")
        );

        let not_gate = explain(Agent::Codex, "match win blocked");
        assert_eq!(not_gate.state, AgentState::Idle);
        assert_eq!(
            not_gate.matched_rule.as_ref().map(|rule| rule.id.as_str()),
            Some("low_contains")
        );

        let line = explain(Agent::Codex, "before\nexact line\nafter");
        assert_eq!(line.state, AgentState::Blocked);
        assert_eq!(
            line.matched_rule.as_ref().map(|rule| rule.id.as_str()),
            Some("line_regex")
        );
    });
}

#[test]
fn remote_manifest_loads_between_local_override_and_bundled() {
    with_manifest_dirs("remote-source", || {
        write_remote_codex(&remote_manifest("9999.01.01.1", "blocked", "remote-ready"));

        let explain = explain(Agent::Codex, "remote-ready");

        assert_eq!(explain.state, AgentState::Blocked);
        assert!(matches!(
            explain.source,
            Some(ManifestSource::Remote { .. })
        ));
        assert_eq!(explain.manifest_version.as_deref(), Some("9999.01.01.1"));
        assert_eq!(
            explain.cached_remote_version.as_deref(),
            Some("9999.01.01.1")
        );
    });
}

#[test]
fn fallback_explain_preserves_active_manifest_version() {
    with_manifest_dirs("fallback-version", || {
        write_remote_codex(&remote_manifest("9999.01.01.1", "blocked", "remote-ready"));

        let explain = explain(Agent::Codex, "ordinary prompt text");

        assert_eq!(explain.state, AgentState::Unknown);
        assert_eq!(
            explain.fallback_reason.as_deref(),
            Some("codex_state_ambiguous")
        );
        assert_eq!(explain.manifest_version.as_deref(), Some("9999.01.01.1"));
        assert!(matches!(
            explain.source,
            Some(ManifestSource::Remote { .. })
        ));
    });
}

#[test]
fn older_cached_remote_manifest_does_not_shadow_newer_bundled_manifest() {
    with_manifest_dirs("older-remote-bundled-fallback", || {
        write_remote_codex(&remote_manifest("2026.06.10.0", "blocked", "remote-ready"));

        let explain = explain(Agent::Codex, "remote-ready");

        assert!(matches!(explain.source, Some(ManifestSource::Bundled)));
        assert_eq!(
            explain.cached_remote_version.as_deref(),
            Some("2026.06.10.0")
        );
        assert!(explain
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("older than bundled")));
    });
}

#[test]
fn local_override_shadows_cached_remote_manifest() {
    with_manifest_dirs("local-shadows-remote", || {
        write_remote_codex(&remote_manifest("9999.01.01.1", "blocked", "remote-ready"));
        write_local_codex(&local_manifest("idle", "local-ready"));

        let explain = explain(Agent::Codex, "local-ready");

        assert_eq!(explain.state, AgentState::Idle);
        assert!(matches!(explain.source, Some(ManifestSource::Override(_))));
        assert!(explain.local_override_shadowing_remote);
        assert_eq!(
            explain.cached_remote_version.as_deref(),
            Some("9999.01.01.1")
        );
    });
}

#[test]
fn invalid_local_override_falls_back_to_cached_remote_manifest() {
    with_manifest_dirs("invalid-local-remote-fallback", || {
        write_remote_codex(&remote_manifest("9999.01.01.1", "blocked", "remote-ready"));
        write_local_codex("id = ");

        let explain = explain(Agent::Codex, "remote-ready");

        assert_eq!(explain.state, AgentState::Blocked);
        assert!(matches!(
            explain.source,
            Some(ManifestSource::Remote { .. })
        ));
        assert!(explain.warning.is_some());
    });
}

#[test]
fn detection_uses_cached_manifest_until_explicit_reload() {
    with_manifest_dirs("cache-boundary", || {
        write_remote_codex(&remote_manifest("9999.01.01.1", "blocked", "cached-ready"));

        let cached = explain(Agent::Codex, "cached-ready");
        assert_eq!(cached.state, AgentState::Blocked);
        assert!(matches!(cached.source, Some(ManifestSource::Remote { .. })));
        assert_eq!(
            cached.matched_rule.as_ref().map(|rule| rule.id.as_str()),
            Some("test")
        );

        write_remote_codex_without_reload(&remote_manifest("9999.01.01.2", "working", "new-ready"));

        let unchanged = explain(Agent::Codex, "new-ready");
        assert_eq!(unchanged.state, AgentState::Unknown);
        assert_eq!(
            unchanged.fallback_reason.as_deref(),
            Some("codex_state_ambiguous")
        );
        assert_eq!(
            unchanged.cached_remote_version.as_deref(),
            Some("9999.01.01.1")
        );

        reload_manifests();

        let reloaded = explain(Agent::Codex, "new-ready");
        assert_eq!(reloaded.state, AgentState::Working);
        assert_eq!(
            reloaded.cached_remote_version.as_deref(),
            Some("9999.01.01.2")
        );
        assert_eq!(
            reloaded.matched_rule.as_ref().map(|rule| rule.id.as_str()),
            Some("test")
        );
    });
}

#[test]
fn compiled_rules_are_shared_until_manifest_reload() {
    with_manifest_dirs("shared-compiled-rules", || {
        write_remote_codex(&format!(
            "{}\nregex = ['^cached-[a-z]+$']\n",
            remote_manifest("9999.01.01.1", "blocked", "cached-ready")
        ));
        let first = load_manifest(Agent::Codex).unwrap();
        let second = load_manifest(Agent::Codex).unwrap();
        assert!(!first.compiled_rules.is_empty());
        assert_eq!(
            first.compiled_rules.as_ptr(),
            second.compiled_rules.as_ptr(),
            "cached loads must retain the same compiled rules and regex search caches"
        );

        write_remote_codex_without_reload(&format!(
            "{}\nregex = ['^new-[a-z]+$']\n",
            remote_manifest("9999.01.01.2", "working", "new-ready")
        ));
        let unchanged = load_manifest(Agent::Codex).unwrap();
        assert_eq!(
            first.compiled_rules.as_ptr(),
            unchanged.compiled_rules.as_ptr()
        );

        reload_manifests_for_agents(&[Agent::Codex]);
        let reloaded = load_manifest(Agent::Codex).unwrap();
        let shared_reload = load_manifest(Agent::Codex).unwrap();
        assert_ne!(
            first.compiled_rules.as_ptr(),
            reloaded.compiled_rules.as_ptr()
        );
        assert_eq!(
            reloaded.compiled_rules.as_ptr(),
            shared_reload.compiled_rules.as_ptr()
        );
        assert!(compiled_rule_matches(
            &first.compiled_rules[0],
            "cached-ready"
        ));
        assert!(!compiled_rule_matches(
            &first.compiled_rules[0],
            "new-ready"
        ));
        assert_eq!(
            explain(Agent::Codex, "new-ready").state,
            AgentState::Working
        );

        std::thread::scope(|scope| {
            for _ in 0..4 {
                let reloaded = &reloaded;
                scope.spawn(move || {
                    let loaded = load_manifest(Agent::Codex).unwrap();
                    assert_eq!(
                        loaded.compiled_rules.as_ptr(),
                        reloaded.compiled_rules.as_ptr()
                    );
                    for _ in 0..8 {
                        assert_eq!(detect(Agent::Codex, "new-ready").state, AgentState::Working);
                    }
                });
            }
        });
    });
}

#[test]
fn osc_regions_use_separate_inputs_and_share_rule_priority() {
    with_manifest_dirs("osc-regions", || {
        write_local_codex(&rules_manifest(
            r#"
[[rules]]
id = "screen"
state = "idle"
priority = 10
region = "whole_recent"
visible_idle = true
contains = ["screen-marker"]

[[rules]]
id = "title"
state = "working"
priority = 20
region = "osc_title"
visible_working = true
regex = ['^title-marker$']

[[rules]]
id = "progress"
state = "blocked"
priority = 30
region = "osc_progress"
visible_blocker = true
regex = ['^progress-marker$']
"#,
        ));
        for (screen, title, progress, state, rule) in [
            ("screen-marker", "", "", AgentState::Idle, "screen"),
            (
                "screen-marker",
                "title-marker",
                "",
                AgentState::Working,
                "title",
            ),
            (
                "screen-marker",
                "title-marker",
                "progress-marker",
                AgentState::Blocked,
                "progress",
            ),
            (
                "screen-marker title-marker progress-marker",
                "",
                "",
                AgentState::Idle,
                "screen",
            ),
        ] {
            let input = DetectionInput {
                screen,
                osc_title: title,
                osc_progress: progress,
            };
            let result = explain_with_input(Agent::Codex, input);
            assert_eq!(result.state, state);
            assert_eq!(
                result
                    .matched_rule
                    .as_ref()
                    .map(|matched| matched.id.as_str()),
                Some(rule)
            );
            let detection =
                crate::detect::detect_agent_with_osc(Some(Agent::Codex), screen, title, progress);
            assert_eq!(detection.state, state);
            assert_eq!(detection.visible_idle, state == AgentState::Idle);
            assert_eq!(detection.visible_working, state == AgentState::Working);
            assert_eq!(detection.visible_blocker, state == AgentState::Blocked);
        }
        let swapped = explain_with_input(
            Agent::Codex,
            DetectionInput {
                screen: "",
                osc_title: "progress-marker",
                osc_progress: "title-marker",
            },
        );
        assert!(swapped.matched_rule.is_none());
    });
}

#[test]
fn skip_rule_suppresses_state_update_without_visible_state_evidence() {
    with_manifest_dirs("skip-rule", || {
        write_local_codex(&rules_manifest(
            r#"
[[rules]]
id = "activity"
state = "working"
priority = 10
visible_working = true
contains = ["activity-marker"]

[[rules]]
id = "overlay"
state = "unknown"
priority = 20
skip_state_update = true
contains = ["overlay-marker"]
"#,
        ));
        let screen = "activity-marker overlay-marker";
        let result = explain(Agent::Codex, screen);
        assert_eq!(result.state, AgentState::Unknown);
        assert!(result.skip_state_update);
        assert_eq!(
            result.skipped_update_reason.as_deref(),
            Some("matched_rule:overlay")
        );
        assert!(!result.visible_idle);
        assert!(!result.visible_working);
        assert!(!result.visible_blocker);
        assert!(detect(Agent::Codex, screen).skip_state_update);
    });
}

#[test]
fn screen_regions_extract_structure_without_classifying_agent_state() {
    for (screen, spec, expected) in [
        ("old\n\nnew\n", "bottom_lines(2)", "\nnew\n"),
        (
            "before\n› input\nafter\n",
            "after_last_prompt_marker",
            "after\n",
        ),
        (
            "before\n› input\nafter\n",
            "before_current_prompt_marker",
            "before\n",
        ),
        (
            "before\n› input\nafter\n",
            "whole_recent_without_current_prompt_marker",
            "",
        ),
        (
            "no marker\n",
            "whole_recent_without_current_prompt_marker",
            "no marker\n",
        ),
        (
            "• old\n■ latest\n› input\n",
            "current_prompt_block_marker",
            "■ latest",
        ),
        (
            "• old\n■ latest\n› input\n",
            "after_current_prompt_block_marker",
            "■ latest\n› input\n",
        ),
        ("› old\n• new\n", "current_prompt_block_marker", ""),
        (
            "above\n\n───\nbody\n───\nfooter\n",
            "above_prompt_box",
            "above\n\n",
        ),
        (
            "above\n\n───\nbody\n───\nfooter\n",
            "last_non_empty_above_prompt_box",
            "above",
        ),
        (
            "above\n───\nbody\n───\nfooter\n",
            "prompt_box_body",
            "body\n",
        ),
        (
            "above\n───\nbody\n───\nfooter\n",
            "after_last_horizontal_rule",
            "footer\n",
        ),
    ] {
        assert_eq!(
            region(
                DetectionInput {
                    screen,
                    osc_title: "",
                    osc_progress: ""
                },
                spec
            ),
            expected,
            "region={spec}"
        );
    }
}

#[test]
fn all_bundled_manifests_parse_and_validate() {
    for agent in Agent::SCREEN_MANIFEST_AGENTS {
        assert!(
            bundled_manifest(agent).is_some(),
            "missing bundled manifest for {}",
            agent_label(agent)
        );
    }
}

#[test]
fn manifest_validation_rejects_unknown_fields_empty_rules_invalid_regions_and_regexes() {
    assert!(parse_manifest(
        r#"
id = "codex"

[[rules]]
id = "typo"
state = "working"
contain = ["Working"]
"#
    )
    .is_err());
    assert!(parse_manifest(
        r#"
id = "codex"

[[rules]]
id = "empty"
state = "working"
"#
    )
    .is_err());
    assert!(parse_manifest(
        r#"
id = "codex"

[[rules]]
id = "bad_region"
state = "working"
region = "after_last_promt_marker"
contains = ["Working"]
"#
    )
    .is_err());
    assert!(parse_manifest(
        r#"
id = "codex"

[[rules]]
id = "bad_regex"
state = "working"
regex = ["["]
"#
    )
    .is_err());
    assert!(parse_manifest(
        r#"
id = "codex"

[[rules]]
id = "bad_nested_regex"
state = "working"
any = [{ line_regex = ["["] }]
"#
    )
    .is_err());
}

#[test]
fn manifest_validation_keeps_skip_rules_neutral() {
    assert!(parse_manifest(
        r#"
id = "codex"

[[rules]]
id = "bad_skip_state"
state = "idle"
skip_state_update = true
contains = ["menu"]
"#
    )
    .is_err());
    assert!(parse_manifest(
        r#"
id = "codex"

[[rules]]
id = "bad_skip_visible"
state = "unknown"
skip_state_update = true
visible_blocker = true
contains = ["menu"]
"#
    )
    .is_err());
}

#[test]
fn manifest_validation_rejects_excessive_rule_count() {
    let mut manifest = String::from(
        r#"
id = "codex"
"#,
    );
    for index in 0..129 {
        manifest.push_str(&format!(
            r#"
[[rules]]
id = "rule_{index}"
state = "idle"
contains = ["ready"]
"#
        ));
    }
    assert!(parse_manifest(&manifest).is_err());
}

#[test]
fn manifest_validation_rejects_excessive_gate_depth() {
    let manifest = r#"
id = "codex"

[[rules]]
id = "deep"
state = "idle"
contains = ["ready"]
all = [
  { contains = ["1"], all = [
    { contains = ["2"], all = [
      { contains = ["3"], all = [
        { contains = ["4"], all = [
          { contains = ["5"], all = [
            { contains = ["6"], all = [
              { contains = ["7"], all = [
                { contains = ["8"], all = [
                  { contains = ["9"] },
                ] },
              ] },
            ] },
          ] },
        ] },
      ] },
    ] },
  ] },
]
"#;
    assert!(parse_manifest(manifest).is_err());
}

#[test]
fn manifest_validation_rejects_excessive_matchers() {
    let matchers = (0..33)
        .map(|index| format!(r#""m{index}""#))
        .collect::<Vec<_>>()
        .join(", ");
    let manifest = format!(
        r#"
id = "codex"

[[rules]]
id = "many"
state = "idle"
contains = [{matchers}]
"#
    );
    assert!(parse_manifest(&manifest).is_err());
}

#[test]
fn bottom_non_empty_lines_uses_bottom_occurrence_for_repeated_text() {
    let content = "marker\nold\n\nmiddle\nmarker\nnew\n";
    assert_eq!(
        region(
            DetectionInput {
                screen: content,
                osc_title: "",
                osc_progress: ""
            },
            "bottom_non_empty_lines(2)"
        ),
        "marker\nnew\n"
    );
}

#[test]
fn top_non_empty_lines_uses_top_occurrence_for_repeated_text() {
    let content = "\nmarker\nold\n\nmiddle\nmarker\nnew\n";
    assert_eq!(
        region(
            DetectionInput {
                screen: content,
                osc_title: "",
                osc_progress: ""
            },
            "top_non_empty_lines(2)"
        ),
        "\nmarker\nold\n"
    );
}

#[test]
fn top_non_empty_lines_requires_a_canonical_positive_bounded_count() {
    let name = "top_non_empty_lines";
    assert!(validate_region_name(&format!("{name}(1)")).is_ok());
    assert!(validate_region_name(&format!("{name}({})", u16::MAX)).is_ok());
    for count in ["0", "01", "+1", "65536", "999999999999999999999999"] {
        assert!(
            validate_region_name(&format!("{name}({count})")).is_err(),
            "{name} accepted invalid count {count}"
        );
    }
}

#[test]
fn top_non_empty_lines_requires_engine_three_when_declared() {
    let manifest = r#"
id = "codex"
version = "1"
min_engine_version = 2

[[rules]]
id = "background"
state = "working"
region = " top_non_empty_lines(1) "
contains = ["active"]
"#;
    assert!(parse_manifest(manifest).is_err());
}

// ---------------------------------------------------------------------------
// fork 自有规则的屏幕证据测试：只覆盖 fork 叠加层（src/detect/manifests/fork/）的
// 规则。上游捆绑规则的屏幕分类不在这里测（上游 7201907b 起改为真机冒烟验证）。
// ---------------------------------------------------------------------------

fn osc_explain(
    agent: Agent,
    screen: &str,
    osc_title: &str,
    osc_progress: &str,
) -> DetectionExplain {
    explain_with_input(
        agent,
        DetectionInput {
            screen,
            osc_title,
            osc_progress,
        },
    )
}

/// 冒烟 M5：codex 首启发现新增 / 变更的钩子时，先在清空的屏幕顶端弹「Hooks need
/// review」，选完才进会话。弹窗期间 herdr 曾按 OSC 标题报 idle，`agent start`
/// 直接返回成功。`evidence` 是真机截屏 `evidence/realcli-729a9b0f/
/// codex-02-hooks-review-prompt.txt`（codex 0.156.1），`narrow` 是 codex 自带的
/// 40 列快照（`startup_hooks_review_prompt.snap`），选项行在窄屏下会折行。
#[test]
fn codex_startup_hooks_review_is_blocked() {
    let evidence = "\n  Hooks need review\n  3 hooks are new or changed.\n  \
        Hooks can run outside the sandbox after you trust them.\n\n\n\
        › 1. Review hooks\n  2. Trust all and continue\n  \
        3. Continue without trusting (hooks won't run)\n\n  \
        enter confirm · esc skip\n";
    let narrow = "\n  Hooks need review\n  2 hooks are new or changed.\n  \
        Hooks can run outside the sandbox\n  after you trust them.\n\n\n  \
        1. Review hooks\n› 2. Trust all and continue\n  \
        3. Continue without trusting (hooks\n     won't run)\n\n  \
        enter confirm · esc skip\n";
    // codex --no-alt-screen 时弹窗同样撑满整屏，原先的 shell 行被推进 scrollback；检测
    // 窗口从最后一行内容往上取满一屏，会把这几行一起带上（ghostty 回放实验）。
    let inline = format!("earlier output\n╰─❯ codex -m gpt-6-luna\n{evidence}");

    for screen in [evidence, narrow, inline.as_str()] {
        let result = osc_explain(Agent::Codex, screen, "project", "");
        assert_eq!(result.state, AgentState::Blocked, "{screen}");
        assert_eq!(
            result.matched_rule.as_ref().map(|rule| rule.id.as_str()),
            Some("startup_hooks_review")
        );
        assert!(result.visible_blocker);
    }

    // 选完进入会话后弹窗已清掉；对话里提到这几个字也不算弹窗。上游 9c96f7dd 起
    // codex 不再从终端输出推断 idle（无规则命中时为 unknown），这里只钉「不报
    // blocked、不命中 fork 规则」。
    for screen in [
        "› Ask Codex to do anything\n\n  gpt-6-luna low · /work\n",
        "› Hooks need review\n\n• Codex asks first; pick \"2. Trust all and continue\".\n\n\
         › Ask Codex to do anything\n\n  gpt-6-luna low · /work\n",
    ] {
        let result = osc_explain(Agent::Codex, screen, "project", "");
        assert_ne!(result.state, AgentState::Blocked, "{screen}");
        assert_ne!(matched_rule_id(&result), Some("startup_hooks_review"));
        assert!(!result.visible_blocker);
    }
}

/// 按行加前缀（空行保持为空），模拟 agent 在回答或工具输出里原样引用一段屏幕。
fn indent_lines(text: &str, prefix: &str) -> String {
    text.lines()
        .map(|line| {
            if line.is_empty() {
                String::new()
            } else {
                format!("{prefix}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 审查 N11：codex 在回答或工具输出里以两格缩进原样引用整段弹窗时，空闲的 codex
/// 不能报 blocked。引用下面总有输入框与状态行；引用滚到屏幕顶端、标题正好是第一个
/// 非空行时也一样。
#[test]
fn codex_startup_hooks_review_ignores_quoted_prompt() {
    let quoted = indent_lines(CODEX_HOOKS_REVIEW_SCREEN, "  ");
    let composer = "\n› Ask Codex to do anything\n\n  gpt-6-luna low · /work\n";
    let in_reply = format!(
        "› What does the hooks prompt look like?\n\n\
         • Codex shows this before the session starts:\n{quoted}\n{composer}"
    );
    let scrolled_to_top = format!("{}\n{composer}", quoted.trim_start_matches('\n'));

    for screen in [in_reply, scrolled_to_top] {
        let result = osc_explain(Agent::Codex, &screen, "project", "");
        assert_ne!(result.state, AgentState::Blocked, "{screen}");
        assert_ne!(matched_rule_id(&result), Some("startup_hooks_review"));
        assert!(!result.visible_blocker);
    }
}

/// 冒烟 M5：kimi 2.x 在新目录首启时先问「Trust this folder?」，此时会话还没建、
/// 钩子一条都没报，只能看屏幕；herdr 曾按兜底报 idle。`evidence` 取自真机截屏
/// `evidence/realcli-729a9b0f/kimi-02-trust-prompt.txt`（Kimi Code 2.0.2，含上方的
/// shell 命令行与分隔线），指针可以停在任一选项上。
#[test]
fn kimi_trust_folder_prompt_is_blocked() {
    let rule = "─".repeat(60);
    let evidence = format!(
        "❯ kimi -m glm-4.5-air\n {rule}\n  Trust this folder?\n  \
         ↑↓ navigate · Enter select · Esc exit\n\n  /var/tmp/project\n\n  \
         Project-level MCP servers are disabled until you explicitly choose Trust. \
         Trust starts the listed project MCP targets and remembers this folder.\n\n   \
         ❯ Trust this folder\n     Enable project MCP servers. Remembered for this folder.\n\n     \
         Don't trust\n     Exit Kimi Code. Asked again next launch.\n\n {rule}\n"
    );
    let pointer_on_distrust = evidence
        .replace("   ❯ Trust this folder\n", "     Trust this folder\n")
        .replace("     Don't trust\n", "   ❯ Don't trust\n");

    for screen in [evidence.as_str(), pointer_on_distrust.as_str()] {
        let result = explain(Agent::Kimi, screen);
        assert_eq!(result.state, AgentState::Blocked, "{screen}");
        assert_eq!(
            result.matched_rule.as_ref().map(|rule| rule.id.as_str()),
            Some("trust_folder_prompt")
        );
        assert!(result.visible_blocker);
    }

    // 信任之后进入输入框；把问题原样打进输入框也不构成弹窗。
    let prompt = "   No session yet — one will be created on your first message.\n\n \
        ╭────╮\n │ > Trust this folder?\n ╰────╯\n Zhipu GLM · GLM-4.5-Air thinking\n";
    let result = explain(Agent::Kimi, prompt);
    assert_eq!(result.state, AgentState::Idle);
    assert!(!result.visible_blocker);
}

/// 审查 N11 同类问题：kimi 在回答或工具输出里原样引用整个信任弹窗（连同上下分隔线）
/// 时，下面还有输入框与状态行，不算弹窗。
#[test]
fn kimi_trust_folder_prompt_ignores_quoted_prompt() {
    let quoted = indent_lines(KIMI_TRUST_FOLDER_SCREEN, "   ");
    let screen = format!(
        " ✨ What does the trust prompt look like?\n\n • Kimi Code asks this on first launch:\n\
         {quoted}\n\n ╭────╮\n │ >  │\n ╰────╯\n Zhipu GLM · GLM-4.5-Air thinking  …/work\n"
    );
    let result = explain(Agent::Kimi, &screen);
    assert_eq!(result.state, AgentState::Idle, "{screen}");
    assert_ne!(matched_rule_id(&result), Some("trust_folder_prompt"));
    assert!(!result.visible_blocker);
}

// ---------------------------------------------------------------------------
// fork 规则叠加层：fork 自有的检测规则不进上游发布的 manifest，加载时叠加到选定的
// 捆绑或远端 manifest 上。捆绑版本不再为 fork 规则抬高，两个方向都不互相遮蔽：
// 远端较新时上游新规则照常生效、fork 规则仍在；远端较旧时回落捆绑、fork 规则仍在。
// ---------------------------------------------------------------------------

/// 真机截屏 codex-02 的弹窗正文（codex 0.156.1）。
const CODEX_HOOKS_REVIEW_SCREEN: &str = "\n  Hooks need review\n  3 hooks are new or changed.\n  \
    Hooks can run outside the sandbox after you trust them.\n\n\n\
    › 1. Review hooks\n  2. Trust all and continue\n  \
    3. Continue without trusting (hooks won't run)\n\n  \
    enter confirm · esc skip\n";

/// 真机截屏 kimi-02 的弹窗（Kimi Code 2.0.2，去掉上方的 shell 行；上下两条分隔线在
/// 真机上占满整行，这里截短）。
const KIMI_TRUST_FOLDER_SCREEN: &str = " ────────────────────────────────────────\n  \
    Trust this folder?\n  ↑↓ navigate · Enter select · Esc exit\n\n  /var/tmp/project\n\n   \
    ❯ Trust this folder\n     Enable project MCP servers. Remembered for this folder.\n\n     \
    Don't trust\n     Exit Kimi Code. Asked again next launch.\n\n \
    ────────────────────────────────────────\n";

/// 各 fork 规则与能触发它的真机弹窗。
const FORK_RULE_SCREENS: [(Agent, &str, &str); 2] = [
    (
        Agent::Codex,
        "startup_hooks_review",
        CODEX_HOOKS_REVIEW_SCREEN,
    ),
    (Agent::Kimi, "trust_folder_prompt", KIMI_TRUST_FOLDER_SCREEN),
];

/// 上游在 fork 同步之后才发布的新规则（herdr.dev 上的较新版本才有）。
const UPSTREAM_ONLY_RULE: &str = r#"
[[rules]]
id = "upstream_only_working"
state = "working"
priority = 800
region = "whole_recent"
visible_working = true
contains = ["upstream-only-marker"]
"#;

fn write_remote_manifest(agent: Agent, content: &str) {
    let path = crate::detect::manifest_update::remote_manifest_path(agent);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
    reload_manifests();
}

fn bundled_version(agent: Agent) -> String {
    bundled_manifest(agent)
        .and_then(|manifest| manifest.version)
        .map(|version| version.to_string())
        .unwrap()
}

/// 模拟上游随后发布到 herdr.dev 的版本：捆绑原文换成 `version`，再追加 `extra_rules`。
fn upstream_remote_manifest(agent: Agent, version: &str, extra_rules: &str) -> String {
    let (_, content) = BUNDLED_MANIFESTS
        .iter()
        .find(|(id, _)| *id == agent_label(agent))
        .unwrap();
    let current = format!("version = \"{}\"", bundled_version(agent));
    assert!(content.contains(&current), "{content}");
    format!(
        "{}\n{extra_rules}",
        content.replacen(&current, &format!("version = \"{version}\""), 1)
    )
}

fn explain_for_screen(agent: Agent, screen: &str) -> DetectionExplain {
    osc_explain(agent, screen, "project", "")
}

fn matched_rule_id(result: &DetectionExplain) -> Option<&str> {
    result.matched_rule.as_ref().map(|rule| rule.id.as_str())
}

#[test]
fn fork_rules_stay_active_when_a_newer_remote_manifest_wins() {
    for (agent, rule_id, screen) in FORK_RULE_SCREENS {
        with_manifest_dirs(&format!("fork-rules-newer-remote-{rule_id}"), || {
            // 比捆绑新一档的上游版本：远端生效，它独有的规则不能被捆绑遮住。
            let version = format!("{}.1", bundled_version(agent));
            let remote = upstream_remote_manifest(agent, &version, UPSTREAM_ONLY_RULE);
            assert!(!remote.contains(rule_id), "{remote}");
            write_remote_manifest(agent, &remote);

            let upstream = explain_for_screen(agent, "upstream-only-marker\n");
            assert_eq!(upstream.state, AgentState::Working, "{agent:?}");
            assert_eq!(matched_rule_id(&upstream), Some("upstream_only_working"));
            assert!(matches!(
                upstream.source,
                Some(ManifestSource::Remote { .. })
            ));
            assert_eq!(upstream.manifest_version.as_deref(), Some(version.as_str()));

            // 远端里没有 fork 规则，叠加层仍让启动弹窗报 blocked。
            let blocked = explain_for_screen(agent, screen);
            assert_eq!(blocked.state, AgentState::Blocked, "{agent:?}");
            assert_eq!(matched_rule_id(&blocked), Some(rule_id));
            assert!(blocked.visible_blocker);
            assert!(matches!(
                blocked.source,
                Some(ManifestSource::Remote { .. })
            ));
        });
    }
}

#[test]
fn fork_rules_stay_active_when_an_older_remote_manifest_falls_back_to_bundled() {
    for (agent, rule_id, screen) in FORK_RULE_SCREENS {
        with_manifest_dirs(&format!("fork-rules-older-remote-{rule_id}"), || {
            write_remote_manifest(
                agent,
                &upstream_remote_manifest(agent, "2000.01.01.1", UPSTREAM_ONLY_RULE),
            );

            let blocked = explain_for_screen(agent, screen);
            assert_eq!(blocked.state, AgentState::Blocked, "{agent:?}");
            assert_eq!(matched_rule_id(&blocked), Some(rule_id));
            assert!(matches!(blocked.source, Some(ManifestSource::Bundled)));
            assert!(blocked
                .warning
                .as_deref()
                .is_some_and(|warning| warning.contains("older than bundled")));

            // 较旧远端独有的规则不生效：生效的是捆绑。
            let fallback = explain_for_screen(agent, "upstream-only-marker\n");
            assert_ne!(matched_rule_id(&fallback), Some("upstream_only_working"));
        });
    }
}

#[test]
fn remote_rule_with_a_fork_rule_id_takes_precedence_over_the_fork_rule() {
    with_manifest_dirs("fork-rule-id-owned-by-remote", || {
        // 上游日后收编同名规则时以上游为准，fork 规则让位且不重复出现。
        let version = format!("{}.1", bundled_version(Agent::Codex));
        let remote_rule = r#"
[[rules]]
id = "startup_hooks_review"
state = "working"
priority = 950
region = "whole_recent"
contains = ["Hooks need review"]
"#;
        write_remote_manifest(
            Agent::Codex,
            &upstream_remote_manifest(Agent::Codex, &version, remote_rule),
        );

        let result = explain_for_screen(Agent::Codex, CODEX_HOOKS_REVIEW_SCREEN);
        assert_eq!(result.state, AgentState::Working);
        assert_eq!(matched_rule_id(&result), Some("startup_hooks_review"));
        assert_eq!(
            result
                .evaluated_rules
                .iter()
                .filter(|rule| rule.id == "startup_hooks_review")
                .count(),
            1
        );
    });
}

#[test]
fn local_override_replaces_the_manifest_without_fork_rules() {
    with_manifest_dirs("fork-rules-not-in-override", || {
        // 本地覆盖是用户写的整份 manifest：只跑文件里的规则。
        write_local_codex(&local_manifest("idle", "local-ready"));

        let result = explain_for_screen(Agent::Codex, CODEX_HOOKS_REVIEW_SCREEN);
        assert!(matches!(result.source, Some(ManifestSource::Override(_))));
        // 上游 9c96f7dd：codex 无规则命中时回落 unknown，不再默认 idle。
        assert_eq!(result.state, AgentState::Unknown);
        assert_eq!(
            result.fallback_reason.as_deref(),
            Some("codex_state_ambiguous")
        );
        assert!(result
            .evaluated_rules
            .iter()
            .all(|rule| rule.id != "startup_hooks_review"));
    });
}

/// fork 规则 id 与捆绑 manifest 撞名时叠加层整条让位，规则就成了死代码；同步上游
/// 后若撞名，删掉 fork 那条（或改名）。
#[test]
fn fork_rule_overlays_parse_compile_and_do_not_shadow_bundled_rule_ids() {
    assert!(!FORK_RULE_OVERLAYS.is_empty());
    for (id, content) in FORK_RULE_OVERLAYS {
        let overlay = parse_manifest(content)
            .unwrap_or_else(|err| panic!("fork {id} rule overlay is invalid: {err}"));
        assert_eq!(overlay.id.as_str(), *id);
        let agent = parse_agent_label(id).unwrap();
        assert!(Agent::SCREEN_MANIFEST_AGENTS.contains(&agent), "{id}");
        let bundled = bundled_manifest(agent).unwrap();
        for rule in &overlay.rules {
            assert!(
                bundled.rules.iter().all(|existing| existing.id != rule.id),
                "fork rule {} is shadowed by the bundled {id} manifest",
                rule.id
            );
        }
        let merged = with_fork_rules(agent, bundled.clone());
        assert_eq!(
            merged.rules.len(),
            bundled.rules.len() + overlay.rules.len()
        );
        loaded_manifest(merged, ManifestSource::Bundled, None, None, false)
            .unwrap_or_else(|err| panic!("fork {id} rule overlay could not be compiled: {err}"));
    }
}

#[test]
fn manifest_cache_indexes_align_with_agent_all() {
    // get/set 的定长数组索引依赖「判别式 == ALL 下标」这一不变量。
    for agent in Agent::ALL {
        assert_eq!(Agent::ALL[agent as usize], agent);
    }
}

#[test]
fn detect_and_explain_agree_on_detection_fields() {
    // collect_evidence=false 的检测热路径必须与 explain（=true）给出逐字段
    // 相同的判定；证据收集只是观察面，不得影响匹配结果。
    let screens = [
        "",
        "plain shell output\n$ ",
        "● Esc to cancel · 1m 2s\n",
        "Do you want to proceed? (y/n)\n",
        "⠋ Working…\n",
        "✻ Thinking…\n  allow once? [y/n]\n",
    ];
    for agent in Agent::SCREEN_MANIFEST_AGENTS {
        for screen in screens {
            for (osc_title, osc_progress) in [("", ""), ("⠙ proj", "50")] {
                let input = DetectionInput {
                    screen,
                    osc_title,
                    osc_progress,
                };
                let detection = detect_with_osc(agent, input);
                let explain = explain_with_input(agent, input);
                assert_eq!(
                    detection.state, explain.state,
                    "state mismatch for {agent:?} screen={screen:?}"
                );
                assert_eq!(detection.skip_state_update, explain.skip_state_update);
                assert_eq!(detection.visible_idle, explain.visible_idle);
                assert_eq!(detection.visible_blocker, explain.visible_blocker);
                assert_eq!(detection.visible_working, explain.visible_working);
                assert!(
                    !explain.evaluated_rules.is_empty()
                        || explain.fallback_reason.is_some()
                        || explain.matched_rule.is_none(),
                    "explain path must collect rule evidence"
                );
            }
        }
    }
}

/// INFRA-01：bundled manifest 由 `include_str!` 编进二进制，解析或编译失败会在
/// 运行时 panic（`bundled_manifest` / `bundled_loaded_manifest`）。这里在测试期
/// 把每一份都走一遍，把「编译期不变量」变成守门而不是运行时炸。
#[test]
fn every_bundled_manifest_parses_and_compiles() {
    for (id, content) in BUNDLED_MANIFESTS {
        let manifest = parse_manifest(content)
            .unwrap_or_else(|err| panic!("bundled {id} manifest is invalid: {err}"));
        assert_eq!(
            manifest.id.as_str(),
            *id,
            "bundled manifest id must match its table key"
        );
        loaded_manifest(manifest, ManifestSource::Bundled, None, None, false)
            .unwrap_or_else(|err| panic!("bundled {id} manifest could not be compiled: {err}"));
    }
}
