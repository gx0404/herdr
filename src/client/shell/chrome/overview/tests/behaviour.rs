use super::*;

#[test]
fn manual_marks_restore_per_endpoint_then_clear_on_new_turn_launch_and_boot() {
    let mut state = state();
    select(&mut state, "p0");
    let target = state.overview_target().expect("target");
    state.mark_agent_read(&target, false);
    let saved = state.saved_agent_marks();
    let config = ClientShellConfig {
        preferences: preferences::ClientChromePreferences {
            agent_marks: saved,
            ..Default::default()
        },
        ..ClientShellConfig::from_config(&Config::default())
    };
    let mut restored = ClientShellState::new(config);
    restored.set_snapshot(Box::new(snapshot()));
    assert_eq!(
        restored.snapshot.as_ref().expect("snapshot").agents[0].agent_status,
        AgentStatus::Done
    );
    for change in ["turn", "launch", "boot"] {
        let mut snapshot = snapshot();
        snapshot.revision += 1;
        match change {
            "turn" => snapshot.agents[0].state_change_seq += 1,
            "launch" => snapshot.agents[0].launch_seq += 1,
            _ => snapshot.boot_id = "boot-new".into(),
        }
        restored.set_snapshot(Box::new(snapshot));
        assert!(restored.endpoints[0].agent_presentation.marks().is_empty());
        assert_eq!(
            restored.snapshot.as_ref().expect("snapshot").agents[0].agent_status,
            AgentStatus::Idle
        );
    }
}

#[test]
fn r_u_capital_r_key_path_persists_without_sending_server_or_pane_input() {
    let _dirs = crate::config::test_dirs::isolate_dirs("overview-read-marks");
    let mut state = state();
    state.config.preferences_path =
        Some(crate::config::state_dir().join("client-shell/overview-test.json"));
    select(&mut state, "p0");
    for (key, status) in [
        ("u", AgentStatus::Done),
        ("r", AgentStatus::Idle),
        ("u", AgentStatus::Done),
        ("R", AgentStatus::Idle),
    ] {
        let outcome = state.handle_input_bytes(key.as_bytes());
        assert!(outcome.requests.is_empty() && outcome.actions.is_empty());
        assert_eq!(
            state.snapshot.as_ref().expect("snapshot").agents[0].agent_status,
            status
        );
    }
    state.persist_chrome_preferences(&mut ClientShellInput::default());
    let saved =
        preferences::load(state.config.preferences_path.as_ref().expect("path")).expect("stored");
    assert!(saved.agent_marks["local"]
        .iter()
        .any(|mark| mark.pane == "p0" && mark.read));
}

#[test]
fn groups_fold_more_search_all_tokens_and_enter_activates_selected_identity() {
    let mut state = state();
    let mut snapshot = snapshot();
    for index in 5..16 {
        let mut agent = snapshot.agents[0].clone();
        agent.pane_id = format!("p{index}");
        agent.name = Some(format!("idle{index}"));
        snapshot.agents.push(agent);
    }
    snapshot.revision += 1;
    state.set_snapshot(Box::new(snapshot));
    state.compose(160, 45).expect("groups");
    let n = state.overview().expect("overview");
    assert!(n.overview.rows.iter().any(|row| row.label.contains('…')));
    let index = n
        .overview
        .rows
        .iter()
        .position(|row| row.label.contains('…'))
        .expect("more");
    state.activate_overview(Action::Row(index), &mut ClientShellInput::default());
    state.activate_overview(Action::Jump, &mut ClientShellInput::default());
    state.compose(160, 45).expect("expanded");
    assert!(!state
        .overview()
        .expect("overview")
        .overview
        .rows
        .iter()
        .any(|row| row.label.contains('…')));
    if let Some(n) = state.overview_mut() {
        n.query = TextEditor::new("main 修复焦点 2", false);
        n.selected = None;
        n.overview.explicit = false;
    }
    state.compose(80, 30).expect("search");
    let target = state.overview_target().expect("search target");
    assert_eq!(target.pane, "p2");
    let outcome = state.handle_input_bytes(b"\r");
    assert!(outcome.actions.iter().any(|action|matches!(action,ClientShellAction::Endpoint{request,..}if matches!(&request.method,Method::PaneFocus(target)if target.pane_id=="p2"))));
    assert!(outcome.requests.is_empty());
}

#[test]
fn local_timeout_duplicate_send_and_late_old_epoch_keep_retry_draft() {
    let mut state = state();
    select(&mut state, "p0");
    state.activate_overview(Action::Reply, &mut ClientShellInput::default());
    state.insert_overview_text("draft\n保留");
    let mut outcome = ClientShellInput::default();
    state.send_overview_reply(&mut outcome);
    let ClientShellAction::EndpointRequest {
        boot_id, request, ..
    } = &outcome.actions[0]
    else {
        panic!("request");
    };
    let (boot, id) = (boot_id.clone(), request.id.clone());
    state.tick_overview(
        Instant::now() + Duration::from_secs(11),
        &mut ClientShellInput::default(),
    );
    assert_eq!(
        state.overview().expect("draft").overview.reply.as_str(),
        "draft\n保留"
    );
    let warning = state
        .overview()
        .expect("draft")
        .overview
        .error
        .as_deref()
        .expect("timeout warning");
    assert!(warning.contains("Check terminal") || warning.contains("请先检查终端"));
    let mut retry = ClientShellInput::default();
    state.send_overview_reply(&mut retry);
    assert_eq!(retry.actions.len(), 1);
    assert!(
        !state
            .handle_endpoint_result(
                &boot,
                &id,
                Err(ClientShellEndpointError {
                    code: None,
                    message: "late error".into()
                })
            )
            .0
    );
    assert!(state
        .overview()
        .expect("retry")
        .overview
        .in_flight
        .is_some());
}

#[test]
fn reply_unavailable_notice_follows_the_current_state_without_overwriting_errors() {
    let mut state = state();
    select(&mut state, "p1");
    state.activate_overview(Action::Preview, &mut ClientShellInput::default());
    state.compose(160, 45).expect("working preview");
    let view = &state.overview().expect("overview").overview;
    assert!(!view.can_reply);
    assert!(view.error.is_none());
    assert_eq!(
        reply_notice(view),
        Some(tr(
            "Reply unavailable. Jump to the terminal to respond.",
            "不可在此回复，请跳转到终端。",
        ))
    );

    let mut projected = snapshot();
    projected.revision += 1;
    projected.agents[1].agent_status = AgentStatus::Idle;
    projected.agents[1].state_change_seq += 1;
    state.set_snapshot(Box::new(projected));
    state.compose(160, 45).expect("ready preview");
    let view = &state.overview().expect("overview").overview;
    assert!(view.can_reply);
    assert!(reply_notice(view).is_none());

    state.overview_mut().expect("overview").overview.error = Some("send failed".into());
    state.set_endpoint_methods(Some(vec!["pane.focus".into()]));
    state.compose(160, 45).expect("unsupported preview");
    let view = &state.overview().expect("overview").overview;
    assert!(!view.can_reply);
    assert_eq!(reply_notice(view), Some("send failed"));
}

#[test]
fn offline_target_and_unknown_prompt_capability_never_send() {
    let mut state = state();
    select(&mut state, "p0");
    state.set_endpoint_status(&ClientEndpointId::Local, ClientEndpointStatus::Reconnecting);
    let mut before_paint = ClientShellInput::default();
    state.activate_overview(Action::Jump, &mut before_paint);
    assert!(before_paint.actions.is_empty() && before_paint.requests.is_empty());
    state.compose(160, 45).expect("offline");
    let mut outcome = ClientShellInput::default();
    for action in [Action::Jump, Action::Reply, Action::Send, Action::Unread] {
        state.activate_overview(action, &mut outcome);
    }
    assert!(outcome.actions.is_empty() && outcome.requests.is_empty());
}

#[test]
fn prompt_response_from_old_boot_and_replacement_cannot_mutate_current_draft() {
    for change in ["boot", "launch"] {
        let mut state = state();
        select(&mut state, "p0");
        state.activate_overview(Action::Reply, &mut ClientShellInput::default());
        state.insert_overview_text("old draft");
        let mut outcome = ClientShellInput::default();
        state.send_overview_reply(&mut outcome);
        let ClientShellAction::EndpointRequest {
            boot_id, request, ..
        } = &outcome.actions[0]
        else {
            panic!("request");
        };
        let (boot, id) = (boot_id.clone(), request.id.clone());
        let mut snapshot = snapshot();
        snapshot.revision += 1;
        if change == "boot" {
            snapshot.boot_id = "new".into();
        } else {
            snapshot.agents[0].launch_seq += 1;
        }
        state.set_snapshot(Box::new(snapshot));
        state.compose(160, 45).expect("changed");
        assert!(
            !state
                .handle_endpoint_result(
                    &boot,
                    &id,
                    Err(ClientShellEndpointError {
                        code: None,
                        message: "old".into()
                    })
                )
                .0
        );
        assert!(state.overview().is_none_or(|n| !n.overview.editing));
    }
}

#[test]
fn narrow_cjk_preview_and_editor_resize_are_safe_and_no_keys_leak() {
    let mut state = state();
    select(&mut state, "p0");
    state.handle_input_bytes(b" ");
    state.compose(60, 24).expect("preview");
    state.handle_input_bytes(b"e");
    let outcome = state.handle_input_bytes("第一行\n第二行".as_bytes());
    assert!(outcome.requests.is_empty());
    state.insert_overview_text("\n第三行\n第四行\n第五行\n第六行\n第七行");
    for (cols, rows) in [(80, 30), (60, 24), (30, 15), (12, 6), (160, 45)] {
        let frame = state.compose(cols, rows).expect("safe");
        assert_eq!(frame.cells.len(), usize::from(cols) * usize::from(rows));
    }
    let n = state.overview().expect("overview");
    assert!(n.overview.reply.contains('\n'));
    assert!(n.overview.editor_cursor.is_some());
}

#[test]
fn read_all_does_not_stop_at_the_manual_mark_cap() {
    let mut state = state();
    let mut projected = snapshot();
    projected.agents.clear();
    let template = snapshot().agents[0].clone();
    for index in 0..270 {
        let mut agent = template.clone();
        agent.pane_id = format!("cap-{index}");
        agent.agent_status = AgentStatus::Working;
        agent.state_change_seq = 2;
        projected.agents.push(agent);
    }
    projected.revision += 1;
    state.set_snapshot(Box::new(projected.clone()));
    for agent in &mut projected.agents {
        agent.agent_status = AgentStatus::Idle;
        agent.state_change_seq = 3;
    }
    projected.revision += 1;
    state.set_snapshot(Box::new(projected));
    let targets = state
        .snapshot
        .as_deref()
        .expect("snapshot")
        .agents
        .iter()
        .map(|agent| {
            Target::for_agent(
                ClientEndpointId::Local,
                state.snapshot.as_deref().expect("snapshot"),
                agent,
            )
        })
        .collect::<Vec<_>>();
    for target in targets.iter().take(256) {
        assert!(state.mark_agent_read(target, false));
    }
    state.mark_all_agents_read();
    assert!(state
        .snapshot
        .as_ref()
        .expect("snapshot")
        .agents
        .iter()
        .all(|agent| agent.agent_status == AgentStatus::Idle));
    assert!(
        state
            .saved_agent_marks()
            .values()
            .map(Vec::len)
            .sum::<usize>()
            <= 256
    );
}

#[test]
fn status_scrollbar_track_is_real_without_opening_or_changing_selection() {
    let mut state = state();
    state.compose(80, 12).expect("small list");
    let track = state.hits.navigator_scrollbar;
    assert!(!track.is_empty());
    let selected = state.overview().expect("overview").selected.clone();
    let mut outcome = ClientShellInput::default();
    state.overview_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: track.x,
            row: track.bottom() - 1,
            modifiers: KeyModifiers::NONE,
        },
        &mut outcome,
    );
    assert!(outcome.actions.is_empty() && outcome.requests.is_empty());
    assert!(state.overview().expect("overview").scroll > 0);
    assert_eq!(state.overview().expect("overview").selected, selected);
}

#[test]
fn failed_preview_preserves_both_received_error_and_old_question() {
    let mut state = state();
    let mut projected = snapshot();
    projected.revision += 1;
    projected.agents[3].insight.error = Some("failure reason must stay visible".into());
    projected.agents[3].insight.question = Some("previous question is also received".into());
    state.set_snapshot(Box::new(projected));
    select(&mut state, "p3");
    state.activate_overview(Action::Preview, &mut ClientShellInput::default());
    state.compose(160, 45).expect("failure preview");
    let text = state
        .overview()
        .expect("overview")
        .overview
        .lines
        .join("\n");
    assert!(
        text.contains("failure reason must stay visible")
            && text.contains("previous question is also received")
    );
}

#[test]
fn location_rows_keep_navigation_and_received_insight_cue() {
    let mut state = state();
    state.activate_overview(Action::Grouping, &mut ClientShellInput::default());
    let frame = state.compose(160, 45).expect("location");
    let rows = frame
        .cells
        .iter()
        .map(|cell| cell.symbol.as_str())
        .collect::<String>();
    assert!(rows.contains("Allow edit?") && rows.contains("main"));
    assert!(state
        .hits
        .navigator_rows
        .iter()
        .any(|(_, target)| matches!(target, ClientNavigatorTarget::Workspace { .. })));
}

#[test]
fn successful_prompt_receipt_clears_only_current_draft_and_keeps_lifecycle() {
    let mut state = state();
    select(&mut state, "p0");
    state.activate_overview(Action::Reply, &mut ClientShellInput::default());
    state.insert_overview_text("confirmed draft");
    let mut outcome = ClientShellInput::default();
    state.send_overview_reply(&mut outcome);
    let ClientShellAction::EndpointRequest {
        boot_id, request, ..
    } = &outcome.actions[0]
    else {
        panic!("request");
    };
    let agent=serde_json::from_value(serde_json::json!({"terminal_id":"t0","workspace_id":"ws_1","tab_id":"tab_1","pane_id":"p0","focused":false,"agent_status":"idle","revision":1})).expect("agent result");
    assert!(
        state
            .handle_endpoint_result(
                boot_id,
                &request.id,
                Ok(ResponseResult::AgentPrompted { agent })
            )
            .0
    );
    let n = state.overview().expect("overview");
    assert!(n.overview.reply.is_empty() && !n.overview.editing && n.overview.in_flight.is_none());
    assert_eq!(
        state.snapshot.as_ref().expect("snapshot").agents[0].agent_status,
        AgentStatus::Idle
    );
}

#[test]
fn read_marks_parse_lenient_and_total_bound() {
    let marks=(0..400).map(|index|serde_json::json!({"boot":"boot-1","pane":format!("p{index}"),"launch":1,"sequence":1,"read":false})).collect::<Vec<_>>();
    let raw = serde_json::json!({"agent_marks":{"local":marks,"ssh:bad":[{"bad":true}]},"sidebar_width":31});
    let preferences: preferences::ClientChromePreferences =
        serde_json::from_value(raw).expect("lenient");
    assert_eq!(preferences.agent_marks["local"].len(), 256);
    assert_eq!(preferences.sidebar_width, Some(31));
}

#[test]
fn saved_remote_unread_survives_flush_before_first_snapshot_and_revalidates_identity() {
    let _dirs = crate::config::test_dirs::isolate_dirs("overview-unimported-marks");
    for transition in ["matching", "boot", "launch", "turn"] {
        let profile =
            SavedSshEndpoint::new("Remote", "dev@example.com", "default").expect("profile");
        let endpoint_id = ClientEndpointId::Ssh(profile.id.clone());
        let key = endpoint_id.storage_key();
        let remote = snapshot();
        let agent = &remote.agents[0];
        let mark = crate::client::shell::endpoint_agent_state::ReadMark {
            boot: remote.boot_id.clone(),
            pane: agent.pane_id.clone(),
            launch: agent.launch_seq,
            sequence: agent.state_change_seq,
            read: false,
        };
        let mut config = ClientShellConfig::from_config(&Config::default());
        config
            .preferences
            .agent_marks
            .insert(key.clone(), vec![mark.clone()]);
        config.preferences_path = Some(
            crate::config::state_dir().join(format!("client-shell/{transition}-unimported.json")),
        );
        let mut state = ClientShellState::new(config);
        state.set_endpoint_catalog(&[profile]);
        state.set_snapshot(Box::new(snapshot()));
        assert!(state.endpoints[1].snapshot.is_none());
        assert!(state.endpoints[1].agent_presentation.marks().is_empty());
        state.sidebar_width_manual = true;
        state.sidebar_width = 31;
        state.persist_chrome_preferences(&mut ClientShellInput::default());
        assert_eq!(
            state.config.preferences.agent_marks.get(&key),
            Some(&vec![mark.clone()])
        );
        let persisted = preferences::load(state.config.preferences_path.as_ref().expect("path"))
            .expect("persisted preferences");
        assert_eq!(persisted.agent_marks.get(&key), Some(&vec![mark]));
        assert_eq!(persisted.sidebar_width, Some(31));

        let mut remote = remote;
        match transition {
            "boot" => remote.boot_id = "boot-new".into(),
            "launch" => remote.agents[0].launch_seq += 1,
            "turn" => remote.agents[0].state_change_seq += 1,
            _ => {}
        }
        state.set_endpoint_status(&endpoint_id, ClientEndpointStatus::Online);
        state.set_endpoint_snapshot(&endpoint_id, Box::new(remote));
        let endpoint = &state.endpoints[1];
        assert_eq!(
            endpoint.snapshot.as_ref().expect("remote snapshot").agents[0].agent_status,
            if transition == "matching" {
                AgentStatus::Done
            } else {
                AgentStatus::Idle
            },
            "{transition}"
        );
        assert_eq!(
            endpoint.agent_presentation.marks().len(),
            usize::from(transition == "matching")
        );
        state.persist_chrome_preferences(&mut ClientShellInput::default());
        assert_eq!(
            state.config.preferences.agent_marks.contains_key(&key),
            transition == "matching"
        );
    }
}

#[test]
fn unimported_remote_marks_reserve_global_capacity_and_retirement_releases_it() {
    for retirement in ["disabled", "removed"] {
        let profile =
            SavedSshEndpoint::new("Remote", "dev@example.com", "default").expect("profile");
        let endpoint_id = ClientEndpointId::Ssh(profile.id.clone());
        let key = endpoint_id.storage_key();
        let marks = (0..256)
            .map(
                |index| crate::client::shell::endpoint_agent_state::ReadMark {
                    boot: "boot-1".into(),
                    pane: format!("remote-{index}"),
                    launch: 1,
                    sequence: 1,
                    read: false,
                },
            )
            .collect::<Vec<_>>();
        let mut config = ClientShellConfig::from_config(&Config::default());
        config
            .preferences
            .agent_marks
            .insert(key.clone(), marks.clone());
        let mut state = ClientShellState::new(config);
        state.set_endpoint_catalog(&[profile]);
        state.set_snapshot(Box::new(snapshot()));
        let local = state.snapshot.as_deref().expect("local snapshot");
        let target = Target::for_agent(ClientEndpointId::Local, local, &local.agents[0]);
        assert_eq!(state.saved_agent_marks().get(&key), Some(&marks));
        assert!(
            !state.mark_agent_read(&target, false),
            "{retirement}: reserved capacity"
        );
        assert_eq!(
            state
                .saved_agent_marks()
                .values()
                .map(Vec::len)
                .sum::<usize>(),
            256
        );
        assert!(state.mark_agent_read(&target, true));
        assert_eq!(state.saved_agent_marks().get(&key), Some(&marks));
        if retirement == "disabled" {
            state.retire_endpoint(&endpoint_id);
        } else {
            state.set_endpoint_catalog(&[]);
        }
        assert!(!state.saved_agent_marks().contains_key(&key));
        assert!(state.mark_agent_read(&target, false));
        assert_eq!(
            state
                .saved_agent_marks()
                .values()
                .map(Vec::len)
                .sum::<usize>(),
            1
        );
    }
}

#[test]
fn read_mark_actions_refuse_needs_input_and_failed_without_changing_lifecycle() {
    let mut state = state();
    for pane in ["p2", "p3"] {
        select(&mut state, pane);
        let target = state.overview_target().expect("target");
        let before = state.overview_agent(&target).expect("agent").clone();
        assert!(!state.mark_agent_read(&target, false));
        assert!(!state.mark_agent_read(&target, true));
        for action in [Action::Read, Action::Unread] {
            let mut outcome = ClientShellInput::default();
            state.activate_overview(action, &mut outcome);
            assert!(outcome.requests.is_empty() && outcome.actions.is_empty());
            assert_eq!(
                state.overview_agent(&target).expect("agent").agent_status,
                before.agent_status
            );
            assert_eq!(
                state.overview_agent(&target).expect("agent").insight,
                before.insight
            );
        }
    }
    assert!(state.saved_agent_marks().is_empty());
}

#[test]
fn overview_and_global_unread_share_capacity_gate_but_read_and_existing_marks_still_work() {
    use crate::client::shell::action_table::{global_action_state, ActionId};
    let mut state = state();
    let mut projected = snapshot();
    projected.focused_pane_id = Some("p0".into());
    let template = projected.agents[0].clone();
    for index in 0..256 {
        let mut agent = template.clone();
        agent.pane_id = format!("capacity-{index}");
        projected.agents.push(agent);
    }
    projected.revision += 1;
    state.set_snapshot(Box::new(projected.clone()));
    for agent in projected.agents.iter().skip(5) {
        let target = Target::for_agent(ClientEndpointId::Local, &projected, agent);
        assert!(state.mark_agent_read(&target, false));
    }
    select(&mut state, "p0");
    assert!(!global_action_state(ActionId::MarkUnread, &state.global_action_context()).enabled);
    assert!(state
        .hits
        .overview_actions
        .iter()
        .any(|(_, action)| *action == Action::Read));
    assert!(state
        .hits
        .overview_actions
        .iter()
        .any(|(_, action)| *action == Action::ReadAll));
    assert!(!state
        .hits
        .overview_actions
        .iter()
        .any(|(_, action)| *action == Action::Unread));
    let before = state.saved_agent_marks();
    let outcome = state.handle_input_bytes(b"u");
    assert!(outcome.actions.is_empty() && outcome.requests.is_empty());
    assert!(state.endpoint_error.is_some());
    assert_eq!(state.saved_agent_marks(), before);
    assert!(state.handle_input_bytes(b"r").actions.is_empty());
    assert_eq!(state.saved_agent_marks(), before);

    projected.focused_pane_id = Some("capacity-0".into());
    projected.revision += 1;
    state.set_snapshot(Box::new(projected));
    select(&mut state, "capacity-0");
    assert!(global_action_state(ActionId::MarkUnread, &state.global_action_context()).enabled);
    assert!(state
        .hits
        .overview_actions
        .iter()
        .any(|(_, action)| *action == Action::Unread));
    state.handle_input_bytes(b"u");
    assert_eq!(
        state
            .saved_agent_marks()
            .values()
            .map(Vec::len)
            .sum::<usize>(),
        256
    );
    state.handle_input_bytes(b"R");
    let projected = state.snapshot.as_ref().expect("snapshot");
    assert_eq!(projected.agents[0].agent_status, AgentStatus::Idle);
    assert_eq!(projected.agents[2].agent_status, AgentStatus::Blocked);
    assert!(projected.agents[3].insight.attention.is_some());
    assert!(projected
        .agents
        .iter()
        .skip(5)
        .all(|agent| agent.agent_status == AgentStatus::Idle));
}
