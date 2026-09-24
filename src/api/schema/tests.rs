use std::collections::HashMap;

use super::*;

fn protocol_schema_entry<T: schemars::JsonSchema>(name: &str) -> serde_json::Value {
    let mut schema = serde_json::to_value(schemars::schema_for!(T)).unwrap();
    rewrite_schema_refs(&mut schema, name);
    schema
}

fn rewrite_schema_refs(value: &mut serde_json::Value, schema_name: &str) {
    match value {
        serde_json::Value::Object(object) => {
            if let Some(serde_json::Value::String(reference)) = object.get_mut("$ref") {
                if let Some(path) = reference.strip_prefix("#/") {
                    *reference = format!("#/schemas/{schema_name}/{path}");
                }
            }
            for child in object.values_mut() {
                rewrite_schema_refs(child, schema_name);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                rewrite_schema_refs(item, schema_name);
            }
        }
        _ => {}
    }
}

fn protocol_schema_document() -> serde_json::Value {
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Herdr API",
        "schema_version": 1,
        "protocol": crate::protocol::PROTOCOL_VERSION,
        "schemas": {
            "request": protocol_schema_entry::<Request>("request"),
            "success_response": protocol_schema_entry::<SuccessResponse>("success_response"),
            "error_response": protocol_schema_entry::<ErrorResponse>("error_response"),
            "event": protocol_schema_entry::<EventEnvelope>("event"),
            "stream_event": protocol_schema_entry::<StreamEventEnvelope>("stream_event"),
            "event_stream_notice": protocol_schema_entry::<EventStreamNotice>("event_stream_notice"),
            "subscription_event": protocol_schema_entry::<SubscriptionEventEnvelope>("subscription_event"),
            "observation_event": protocol_schema_entry::<ObservationEventEnvelope>("observation_event"),
        },
    })
}

#[test]
fn request_uses_dot_method_names() {
    let request = Request {
        id: "req_1".into(),
        method: Method::WorkspaceCreate(WorkspaceCreateParams {
            source_workspace_id: None,
            cwd: Some("/tmp".into()),
            focus: true,
            label: Some("api".into()),
            env: Default::default(),
        }),
    };

    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["method"], "workspace.create");
}

#[test]
fn workspace_close_group_intent_defaults_false_and_round_trips() {
    let request: Request = serde_json::from_value(serde_json::json!({
        "id": "close",
        "method": "workspace.close",
        "params": { "workspace_id": "w1" }
    }))
    .unwrap();
    assert!(matches!(
        request.method,
        Method::WorkspaceClose(WorkspaceCloseParams {
            close_group: false,
            ..
        })
    ));

    let explicit = Request {
        id: "close-group".into(),
        method: Method::WorkspaceClose(WorkspaceCloseParams {
            workspace_id: "w1".into(),
            close_group: true,
        }),
    };
    let json = serde_json::to_value(&explicit).unwrap();
    assert_eq!(json["params"]["close_group"], true);
    assert_eq!(serde_json::from_value::<Request>(json).unwrap(), explicit);
}

#[test]
fn agent_start_and_prompt_requests_round_trip() {
    let start = Request {
        id: "start".into(),
        method: Method::AgentStart(AgentStartParams {
            name: "reviewer".into(),
            kind: "pi".into(),
            pane_id: "w1:p2".into(),
            args: vec!["--no-session".into()],
            timeout_ms: Some(30_000),
        }),
    };
    let start_json = serde_json::to_value(&start).unwrap();
    assert_eq!(start_json["method"], "agent.start");
    assert_eq!(start_json["params"]["pane_id"], "w1:p2");
    assert_eq!(
        serde_json::from_value::<Request>(start_json).unwrap(),
        start
    );

    let prompt = Request {
        id: "prompt".into(),
        method: Method::AgentPrompt(AgentPromptParams {
            target: "reviewer".into(),
            text: "review this".into(),
            wait: None,
        }),
    };
    let prompt_json = serde_json::to_value(&prompt).unwrap();
    assert_eq!(prompt_json["method"], "agent.prompt");
    assert_eq!(
        serde_json::from_value::<Request>(prompt_json).unwrap(),
        prompt
    );

    let prompt_and_wait = Request {
        id: "prompt-and-wait".into(),
        method: Method::AgentPrompt(AgentPromptParams {
            target: "reviewer".into(),
            text: "review this".into(),
            wait: Some(AgentPromptWaitOptions {
                until: vec![AgentStatus::Idle, AgentStatus::Done],
                timeout_ms: Some(120_000),
                submission_deadline: None,
            }),
        }),
    };
    let prompt_and_wait_json = serde_json::to_value(&prompt_and_wait).unwrap();
    assert_eq!(
        prompt_and_wait_json["params"]["wait"]["until"],
        serde_json::json!(["idle", "done"])
    );
    assert_eq!(
        prompt_and_wait_json["params"]["wait"]["timeout_ms"],
        120_000
    );
    assert_eq!(
        serde_json::from_value::<Request>(prompt_and_wait_json).unwrap(),
        prompt_and_wait
    );
}

#[test]
fn bundled_protocol_schema_refs_resolve_inside_bundle() {
    fn assert_no_standalone_refs(value: &serde_json::Value) {
        match value {
            serde_json::Value::Object(object) => {
                if let Some(serde_json::Value::String(reference)) = object.get("$ref") {
                    assert!(
                        !reference.starts_with("#/$defs/"),
                        "schema bundle contains standalone ref {reference}"
                    );
                }
                for child in object.values() {
                    assert_no_standalone_refs(child);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    assert_no_standalone_refs(item);
                }
            }
            _ => {}
        }
    }

    assert_no_standalone_refs(&protocol_schema_document());
}

#[test]
fn generated_protocol_schema_artifact_is_current() {
    let actual = format!(
        "{}\n",
        serde_json::to_string_pretty(&protocol_schema_document()).unwrap()
    );
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("docs/next/api/herdr-api.schema.json");

    if std::env::var_os("HERDR_UPDATE_API_SCHEMA").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &actual).unwrap();
        return;
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "failed to read {}; run `HERDR_UPDATE_API_SCHEMA=1 just test-one generated_protocol_schema_artifact_is_current`: {err}",
            path.display()
        )
    });
    assert_eq!(
        expected, actual,
        "generated API schema artifact is stale; run `HERDR_UPDATE_API_SCHEMA=1 just test-one generated_protocol_schema_artifact_is_current`"
    );
}

#[test]
fn request_round_trips_for_server_stop() {
    let request = Request {
        id: "req_stop".into(),
        method: Method::ServerStop(EmptyParams::default()),
    };

    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["method"], "server.stop");
    let restored: Request = serde_json::from_value(json).unwrap();
    assert_eq!(restored, request);
}

#[test]
fn request_round_trips_for_server_reload_config() {
    let request = Request {
        id: "req_reload".into(),
        method: Method::ServerReloadConfig(EmptyParams::default()),
    };

    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["method"], "server.reload_config");
    let restored: Request = serde_json::from_value(json).unwrap();
    assert_eq!(restored, request);
}

#[test]
fn request_round_trips_for_server_reload_agent_manifests() {
    let request = Request {
        id: "req_reload_agent_manifests".into(),
        method: Method::ServerReloadAgentManifests(EmptyParams::default()),
    };

    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["method"], "server.reload_agent_manifests");
    let restored: Request = serde_json::from_value(json).unwrap();
    assert_eq!(restored, request);
}

#[test]
fn request_round_trips_for_server_agent_manifests() {
    let request = Request {
        id: "req_agent_manifests".into(),
        method: Method::ServerAgentManifests(EmptyParams::default()),
    };

    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["method"], "server.agent_manifests");
    let restored: Request = serde_json::from_value(json).unwrap();
    assert_eq!(restored, request);
}

#[test]
fn request_round_trips_for_agent_explain() {
    let request = Request {
        id: "req_agent_explain".into(),
        method: Method::AgentExplain(AgentTarget {
            target: "agent-1".into(),
        }),
    };

    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["method"], "agent.explain");
    let restored: Request = serde_json::from_value(json).unwrap();
    assert_eq!(restored, request);
}

#[test]
fn integration_list_request_and_response_round_trip() {
    let request = Request {
        id: "req_integrations".into(),
        method: Method::IntegrationList(EmptyParams::default()),
    };
    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["method"], "integration.list");
    assert_eq!(serde_json::from_value::<Request>(json).unwrap(), request);

    let response = SuccessResponse {
        id: "req_integrations".into(),
        result: ResponseResult::IntegrationList {
            integrations: vec![IntegrationInfo {
                target: IntegrationTarget::Codex,
                label: "codex".into(),
                command: "codex".into(),
                available: true,
                state: IntegrationState::Outdated,
            }],
        },
    };
    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(json["result"]["type"], "integration_list");
    assert_eq!(json["result"]["integrations"][0]["state"], "outdated");
    assert_eq!(
        serde_json::from_value::<SuccessResponse>(json).unwrap(),
        response
    );
}

#[test]
fn command_invoke_request_round_trips_without_command_text() {
    let request = Request {
        id: "req_command".into(),
        method: Method::CommandInvoke(CommandInvokeParams {
            command_id: "cmd_0123456789abcdef0123456789abcdef".into(),
            workspace_id: Some("w1".into()),
            tab_id: Some("w1:t1".into()),
            pane_id: Some("w1:p1".into()),
            selection: None,
        }),
    };
    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["method"], "command.invoke");
    assert_eq!(serde_json::from_value::<Request>(json).unwrap(), request);
}

#[test]
fn notification_show_request_parses() {
    let json = r#"{"id":"req_1","method":"notification.show","params":{"title":"build failed","body":"api workspace","position":"top-left","sound":"request"}}"#;
    let request: Request = serde_json::from_str(json).unwrap();
    let Method::NotificationShow(params) = request.method else {
        panic!("wrong method parsed");
    };
    assert_eq!(params.title, "build failed");
    assert_eq!(params.body.as_deref(), Some("api workspace"));
    assert_eq!(
        params.position,
        Some(crate::config::ToastHerdrPosition::TopLeft)
    );
    assert_eq!(params.sound, NotificationShowSound::Request);
}

#[test]
fn notification_show_sound_defaults_to_none() {
    let json = r#"{"id":"req_1","method":"notification.show","params":{"title":"build failed"}}"#;
    let request: Request = serde_json::from_str(json).unwrap();
    let Method::NotificationShow(params) = request.method else {
        panic!("wrong method parsed");
    };

    assert_eq!(params.sound, NotificationShowSound::None);
}

#[test]
fn client_window_title_requests_round_trip() {
    let set = Request {
        id: "req_title_set".into(),
        method: Method::ClientWindowTitleSet(ClientWindowTitleSetParams {
            title: "herdr api".into(),
        }),
    };
    let json = serde_json::to_value(&set).unwrap();
    assert_eq!(json["method"], "client.window_title.set");
    assert_eq!(json["params"]["title"], "herdr api");
    let restored: Request = serde_json::from_value(json).unwrap();
    assert_eq!(restored, set);

    let clear = Request {
        id: "req_title_clear".into(),
        method: Method::ClientWindowTitleClear(EmptyParams::default()),
    };
    let json = serde_json::to_value(&clear).unwrap();
    assert_eq!(json["method"], "client.window_title.clear");
    let restored: Request = serde_json::from_value(json).unwrap();
    assert_eq!(restored, clear);
}

#[test]
fn agent_view_requests_round_trip() {
    let set_json = serde_json::json!({
        "id": "view-set",
        "method": "agent.view.set",
        "params": {
            "source": "example.views",
            "label": "current + attention",
            "filter": {
                "op": "any",
                "filters": [
                    {
                        "op": "eq",
                        "field": "workspace_id",
                        "value": {"context": "current_workspace_id"}
                    },
                    {
                        "op": "in",
                        "field": "status",
                        "values": ["blocked", "done"]
                    }
                ]
            },
            "sort": [
                {"field": "attention", "order": "desc"},
                {"field": "state_change_seq", "order": "desc"}
            ]
        }
    });
    let request: Request = serde_json::from_value(set_json.clone()).unwrap();
    assert!(matches!(request.method, Method::AgentViewSet(_)));
    assert_eq!(serde_json::to_value(request).unwrap(), set_json);

    let clear_json = serde_json::json!({
        "id": "view-clear",
        "method": "agent.view.clear",
        "params": {"source": "example.views"}
    });
    let request: Request = serde_json::from_value(clear_json.clone()).unwrap();
    assert!(matches!(request.method, Method::AgentViewClear(_)));
    assert_eq!(serde_json::to_value(request).unwrap(), clear_json);
}

#[test]
fn unknown_method_is_rejected() {
    let json = r#"{"id":"req_1","method":"nope","params":{}}"#;
    let err = serde_json::from_str::<Request>(json)
        .unwrap_err()
        .to_string();
    assert!(err.contains("unknown variant"));
}

#[test]
fn missing_required_params_are_rejected() {
    let json = r#"{"id":"req_1","method":"pane.send_text","params":{"pane_id":"p_1"}}"#;
    let err = serde_json::from_str::<Request>(json)
        .unwrap_err()
        .to_string();
    assert!(err.contains("text"));
}

#[test]
fn pane_send_input_defaults_to_empty_text_and_keys() {
    let json = r#"
    {
        "id": "req_1",
        "method": "pane.send_input",
        "params": {
            "pane_id": "p_1"
        }
    }
    "#;

    let request: Request = serde_json::from_str(json).unwrap();
    let Method::PaneSendInput(params) = request.method else {
        panic!("wrong method parsed");
    };
    assert_eq!(params.pane_id, "p_1");
    assert!(params.text.is_empty());
    assert!(params.keys.is_empty());
}

#[test]
fn pane_wait_for_output_defaults_strip_ansi_to_true() {
    let json = r#"
    {
        "id": "req_1",
        "method": "pane.wait_for_output",
        "params": {
            "pane_id": "p_1",
            "source": "recent",
            "match": { "type": "substring", "value": "ready" }
        }
    }
    "#;

    let request: Request = serde_json::from_str(json).unwrap();
    let Method::PaneWaitForOutput(params) = request.method else {
        panic!("wrong method parsed");
    };
    assert!(params.strip_ansi);
}

#[test]
fn pane_read_defaults_to_text_format() {
    let json = r#"
    {
        "id": "req_1",
        "method": "pane.read",
        "params": {
            "pane_id": "p_1",
            "source": "visible"
        }
    }
    "#;

    let request: Request = serde_json::from_str(json).unwrap();
    let serialized = serde_json::to_value(&request).unwrap();
    assert!(serialized["params"].get("intent").is_none());
    let Method::PaneRead(params) = request.method else {
        panic!("wrong method parsed");
    };
    assert_eq!(params.format, ReadFormat::Text);
    assert_eq!(params.intent, ReadIntent::Interactive);
}

#[test]
fn pane_current_request_round_trips() {
    let request = Request {
        id: "req_current".into(),
        method: Method::PaneCurrent(PaneCurrentParams {
            caller_pane_id: Some("w1-1".into()),
        }),
    };

    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["method"], "pane.current");
    assert_eq!(json["params"]["caller_pane_id"], "w1-1");
    let restored: Request = serde_json::from_value(json).unwrap();
    assert_eq!(restored, request);
}

#[test]
fn pane_process_info_request_round_trips() {
    let request = Request {
        id: "req_process_info".into(),
        method: Method::PaneProcessInfo(PaneProcessInfoParams {
            pane_id: Some("w1-1".into()),
        }),
    };

    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["method"], "pane.process_info");
    assert_eq!(json["params"]["pane_id"], "w1-1");
    let restored: Request = serde_json::from_value(json).unwrap();
    assert_eq!(restored, request);
}

#[test]
fn event_envelope_round_trips() {
    let events = [
        EventEnvelope {
            event: EventKind::PaneOutputChanged,
            data: EventData::PaneOutputChanged {
                pane_id: "p_1".into(),
                workspace_id: "w_1".into(),
                revision: 42,
            },
        },
        EventEnvelope {
            event: EventKind::WorkspaceMoved,
            data: EventData::WorkspaceMoved {
                workspace_id: "w_1".into(),
                insert_index: 2,
                workspaces: vec![],
            },
        },
        EventEnvelope {
            event: EventKind::WorkspaceReordered,
            data: EventData::WorkspaceReordered {
                workspace_ids: vec!["w_1".into(), "w_2".into()],
                before_workspace_id: Some("w_3".into()),
                workspaces: vec![],
            },
        },
        EventEnvelope {
            event: EventKind::TabMoved,
            data: EventData::TabMoved {
                tab_id: "w_1:1".into(),
                workspace_id: "w_1".into(),
                insert_index: 1,
                tabs: vec![],
            },
        },
        EventEnvelope {
            event: EventKind::LayoutUpdated,
            data: EventData::LayoutUpdated {
                layout: PaneLayoutSnapshot {
                    workspace_id: "w_1".into(),
                    tab_id: "w_1:1".into(),
                    zoomed: false,
                    area: PaneLayoutRect {
                        x: 0,
                        y: 0,
                        width: 100,
                        height: 24,
                    },
                    focused_pane_id: "w_1-1".into(),
                    panes: vec![PaneLayoutPane {
                        pane_id: "w_1-1".into(),
                        focused: true,
                        rect: PaneLayoutRect {
                            x: 0,
                            y: 0,
                            width: 100,
                            height: 24,
                        },
                    }],
                    splits: vec![],
                },
            },
        },
    ];

    for event in events {
        let json = serde_json::to_string(&event).unwrap();
        let restored: EventEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, event);
    }
}

#[test]
fn subscribe_request_parses_parameterized_subscriptions() {
    let json = r#"
    {
        "id": "sub_1",
        "method": "events.subscribe",
        "params": {
            "subscriptions": [
                {
                    "type": "pane.output_matched",
                    "pane_id": "p_1_1",
                    "source": "recent",
                    "lines": 200,
                    "match": { "type": "substring", "value": "auth: received" }
                },
                {
                    "type": "pane.agent_status_changed",
                    "pane_id": "p_1_1",
                    "agent_status": "done"
                },
                {
                    "type": "pane.scroll_changed",
                    "pane_id": "p_1_1"
                }
            ]
        }
    }
    "#;

    let request: Request = serde_json::from_str(json).unwrap();
    let Method::EventsSubscribe(params) = request.method else {
        panic!("wrong method parsed");
    };
    assert_eq!(params.subscriptions.len(), 3);
    assert!(matches!(
        &params.subscriptions[0],
        Subscription::PaneOutputMatched {
            pane_id,
            source: ReadSource::Recent,
            lines: Some(200),
            r#match: OutputMatch::Substring { value },
            strip_ansi: true,
        } if pane_id == "p_1_1" && value == "auth: received"
    ));
    assert!(matches!(
        &params.subscriptions[1],
        Subscription::PaneAgentStatusChanged {
            pane_id,
            agent_status: Some(AgentStatus::Done),
        } if pane_id == "p_1_1"
    ));
    assert!(matches!(
        &params.subscriptions[2],
        Subscription::PaneScrollChanged { pane_id } if pane_id == "p_1_1"
    ));
}

/// HSR-03/APP-006：`notices` 是显式可选面，且是**纯追加**的请求字段。
/// 省略即关闭（既有客户端行为不变），关闭时线上形状与旧请求逐字节一致。
#[test]
fn subscribe_request_notices_flag_defaults_off_and_only_appends() {
    let legacy = r#"{"id":"sub_1","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.focused"}]}}"#;
    let request: Request = serde_json::from_str(legacy).expect("旧请求必须仍能解析");
    let Method::EventsSubscribe(params) = &request.method else {
        panic!("wrong method");
    };
    assert!(!params.notices, "省略 notices 即关闭通知帧");
    assert_eq!(
        serde_json::to_string(&request).unwrap(),
        legacy,
        "关闭时不得出现在线上"
    );

    let opted_in = r#"{"id":"sub_1","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.focused"}],"notices":true}}"#;
    let request: Request = serde_json::from_str(opted_in).expect("显式开启必须能解析");
    let Method::EventsSubscribe(params) = &request.method else {
        panic!("wrong method");
    };
    assert!(params.notices);
    assert_eq!(serde_json::to_string(&request).unwrap(), opted_in);
}

/// HSR-03/APP-006：订阅流帧只在 `EventEnvelope` 基础上**纯追加**可选
/// `sequence`。带序号的帧必须能被旧形状（`EventEnvelope`）宽松解码，不带序号时
/// 线上形状与旧帧逐字节一致。
#[test]
fn stream_event_envelope_only_appends_an_optional_sequence() {
    let envelope = EventEnvelope {
        event: EventKind::WorkspaceFocused,
        data: EventData::WorkspaceFocused {
            workspace_id: "w_1".into(),
        },
    };

    let without_sequence = StreamEventEnvelope {
        event: envelope.event,
        data: envelope.data.clone(),
        sequence: None,
    };
    assert_eq!(
        serde_json::to_string(&without_sequence).unwrap(),
        serde_json::to_string(&envelope).unwrap(),
        "没有序号时线上形状必须与旧事件帧完全一致"
    );

    let sequenced = StreamEventEnvelope::new(42, envelope.clone());
    let json = serde_json::to_value(&sequenced).unwrap();
    assert_eq!(json["sequence"], 42);
    let legacy: EventEnvelope =
        serde_json::from_value(json.clone()).expect("旧客户端必须忽略未知字段");
    assert_eq!(legacy, envelope);
    let restored: StreamEventEnvelope = serde_json::from_value(json).unwrap();
    assert_eq!(restored, sequenced);

    // 旧 server 不发 `sequence`，新客户端解码后得到 `None`。
    let legacy_frame = serde_json::to_value(&envelope).unwrap();
    let decoded: StreamEventEnvelope = serde_json::from_value(legacy_frame).unwrap();
    assert_eq!(decoded.sequence, None);
}

/// 断层通知帧与事件帧同形（`event` + `data`），是纯新增帧：旧客户端按事件名
/// 分派时忽略它，新客户端据此重拉全量快照。
#[test]
fn event_stream_notice_round_trips_with_the_event_envelope_shape() {
    let notice = EventStreamNotice::EventsLost(EventGap { from: 1, to: 88 });
    let json = serde_json::to_value(notice).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"event": "events.lost", "data": {"from": 1, "to": 88}})
    );
    let restored: EventStreamNotice = serde_json::from_value(json).unwrap();
    assert_eq!(restored, notice);
}

#[test]
fn subscription_event_envelope_round_trips() {
    let event = SubscriptionEventEnvelope {
        event: SubscriptionEventKind::PaneOutputMatched,
        data: SubscriptionEventData::PaneOutputMatched(PaneOutputMatchedEvent {
            pane_id: "p_1_1".into(),
            matched_line: "auth: received".into(),
            read: PaneReadResult {
                pane_id: "p_1_1".into(),
                workspace_id: "w_1".into(),
                tab_id: "t_1_1".into(),
                source: ReadSource::Recent,
                format: ReadFormat::Text,
                text: "auth: received\n".into(),
                revision: 0,
                truncated: false,
            },
        }),
        sequence: None,
    };

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("\"event\":\"pane.output_matched\""));
    assert!(
        !json.contains("sequence"),
        "快照类订阅没有序号时不得出现在线上（纯追加可选字段）"
    );
    let restored: SubscriptionEventEnvelope = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, event);
}

#[test]
fn scroll_changed_subscription_event_round_trips() {
    let event = SubscriptionEventEnvelope {
        event: SubscriptionEventKind::ScrollChanged,
        data: SubscriptionEventData::ScrollChanged(PaneScrollChangedEvent {
            pane_id: "p_1_1".into(),
            workspace_id: "w_1".into(),
            scroll: PaneScrollInfo {
                offset_from_bottom: 12,
                max_offset_from_bottom: 240,
                viewport_rows: 30,
            },
        }),
        sequence: None,
    };

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("\"event\":\"pane.scroll_changed\""));
    let restored: SubscriptionEventEnvelope = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, event);
}

#[test]
fn agent_status_request_values_remain_strict() {
    assert!(serde_json::from_str::<AgentStatus>(r#""working""#).is_ok());
    assert!(serde_json::from_str::<AgentStatus>(r#""future_status""#).is_err());
}

#[test]
fn success_response_round_trips() {
    let response = SuccessResponse {
        id: "req_1".into(),
        result: ResponseResult::Pong {
            version: "0.1.2".into(),
            protocol: 6,
            capabilities: Some(ServerCapabilities {
                live_handoff: true,
                detached_server_daemon: true,
                endpoint_protocol_generation: Some(1),
                surface_interest: true,
                health_check: true,
                ssh_agent_registration: false,
            }),
        },
    };

    let json = serde_json::to_string(&response).unwrap();
    let restored: SuccessResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, response);
}

#[test]
fn account_usage_response_round_trips_with_optional_refresh_state() {
    // 旧 server 不带 refresh：字段缺省为 None，快照本体不受影响。
    let legacy = serde_json::json!({
        "id": "req_1",
        "result": {
            "type": "account_usage",
            "accounts": [{
                "account_id": "claude:default",
                "account_label": "Claude Code",
                "agent": "claude",
                "provider": "claude",
                "auth_mode": "cli",
                "plan": null,
                "status": "ready",
                "source": "官方 CLI 回调",
                "source_url": "https://example.test",
                "observed_at_ms": 5,
                "metrics": [],
                "message": null
            }]
        }
    });
    let restored: SuccessResponse = serde_json::from_value(legacy).unwrap();
    let ResponseResult::AccountUsage { accounts, refresh } = restored.result else {
        panic!("not an account_usage result");
    };
    assert_eq!(accounts.len(), 1);
    assert_eq!(refresh, None);

    // None 时序列化省略字段；Some 时并行结构随快照往返，可选字段各自缺省。
    let bare = SuccessResponse {
        id: "req_2".into(),
        result: ResponseResult::AccountUsage {
            accounts: Vec::new(),
            refresh: None,
        },
    };
    let json = serde_json::to_value(&bare).unwrap();
    assert!(json["result"].get("refresh").is_none());

    let response = SuccessResponse {
        id: "req_3".into(),
        result: ResponseResult::AccountUsage {
            accounts: vec![AccountUsageSnapshot {
                account_id: "claude:default".into(),
                ..Default::default()
            }],
            refresh: Some(vec![UsageRefreshState {
                account_id: "claude:default".into(),
                in_flight: true,
                queued: false,
                requested_at_ms: Some(10),
                attempted_at_ms: Some(9),
                next_allowed_at_ms: Some(40_009),
                retry_after_ms: None,
                binding_inferred: true,
                trust_required: false,
                callback_only: true,
                pending_binding: Some(UsagePendingBinding {
                    pane_id: "wT:p9".into(),
                    agent: "claude".into(),
                    candidates: vec!["claude:work".into(), "claude:home".into()],
                    rejected_at_ms: 7,
                }),
                callback_enabled: Some(true),
            }]),
        },
    };
    let json = serde_json::to_string(&response).unwrap();
    assert!(!json.contains("retry_after_ms"), "None 字段应省略: {json}");
    let restored: SuccessResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, response);

    let minimal: UsageRefreshState =
        serde_json::from_value(serde_json::json!({"account_id": "x"})).unwrap();
    assert_eq!(
        minimal,
        UsageRefreshState {
            account_id: "x".into(),
            ..Default::default()
        }
    );
    // 旧 server 不带 callback_only：缺省 false 表示「显式刷新可用」，不会误禁用按钮。
    assert!(!minimal.callback_only);
    // 旧 server 不带账号级回调态：缺省 None = 未知，客户端仍提供「启用」入口。
    assert_eq!(minimal.callback_enabled, None);
}

#[test]
fn session_snapshot_request_and_response_round_trip() {
    let request = Request {
        id: "req_snapshot".into(),
        method: Method::SessionSnapshot(EmptyParams::default()),
    };
    let json = serde_json::to_string(&request).unwrap();
    assert!(json.contains("\"method\":\"session.snapshot\""));
    let restored: Request = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, request);

    let response = SuccessResponse {
        id: "req_snapshot".into(),
        result: ResponseResult::SessionSnapshot {
            snapshot: Box::new(SessionSnapshot {
                version: "0.1.2".into(),
                protocol: 16,
                focused_workspace_id: None,
                focused_tab_id: None,
                focused_pane_id: None,
                workspaces: Vec::new(),
                tabs: Vec::new(),
                panes: Vec::new(),
                layouts: Vec::new(),
                agents: Vec::new(),
            }),
        },
    };
    let json = serde_json::to_string(&response).unwrap();
    assert!(json.contains("\"type\":\"session_snapshot\""));
    let restored: SuccessResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, response);
}

#[test]
fn worktree_request_and_response_round_trip() {
    let request = Request {
        id: "req_worktree".into(),
        method: Method::WorktreeCreate(WorktreeCreateParams {
            workspace_id: Some("1".into()),
            branch: Some("worktree/api".into()),
            base: Some("HEAD".into()),
            focus: true,
            trust_repository: true,
            ..WorktreeCreateParams::default()
        }),
    };
    let json = serde_json::to_string(&request).unwrap();
    assert!(json.contains("\"trust_repository\":true"));
    let restored: Request = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, request);

    let response = SuccessResponse {
        id: "req_worktree".into(),
        result: ResponseResult::WorktreeCreated {
            workspace: WorkspaceInfo {
                workspace_id: "w_1".into(),
                number: 2,
                label: "herdr".into(),
                focused: true,
                pane_count: 1,
                tab_count: 1,
                active_tab_id: "w_1:1".into(),
                agent_status: AgentStatus::Unknown,
                tokens: HashMap::new(),
                worktree: Some(WorkspaceWorktreeInfo {
                    repo_key: "/repo/herdr/.git".into(),
                    repo_name: "herdr".into(),
                    repo_root: "/repo/herdr".into(),
                    checkout_path: "/worktrees/herdr/worktree-api".into(),
                    is_linked_worktree: true,
                }),
            },
            tab: TabInfo {
                tab_id: "w_1:1".into(),
                workspace_id: "w_1".into(),
                number: 1,
                label: "herdr".into(),
                focused: true,
                pane_count: 1,
                agent_status: AgentStatus::Unknown,
            },
            root_pane: PaneInfo {
                pane_id: "w_1-1".into(),
                terminal_id: "term_1".into(),
                workspace_id: "w_1".into(),
                tab_id: "w_1:1".into(),
                focused: true,
                cwd: Some("/worktrees/herdr/worktree-api".into()),
                foreground_cwd: None,
                restore_error: None,
                label: None,
                agent: None,
                title: None,
                terminal_title: None,
                terminal_title_stripped: None,
                display_agent: None,
                agent_status: AgentStatus::Unknown,
                state_labels: HashMap::new(),
                tokens: HashMap::new(),
                agent_session: None,
                scroll: None,
                revision: 0,
            },
            worktree: WorktreeInfo {
                path: "/worktrees/herdr/worktree-api".into(),
                branch: Some("worktree/api".into()),
                is_bare: false,
                is_detached: false,
                is_prunable: false,
                is_linked_worktree: true,
                open_workspace_id: Some("w_1".into()),
                label: "herdr".into(),
            },
        },
    };
    let json = serde_json::to_string(&response).unwrap();
    assert!(json.contains("\"type\":\"worktree_created\""));
    assert!(json.contains("\"worktree\""));
    let restored: SuccessResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, response);
}

#[test]
fn worktree_lifecycle_events_round_trip() {
    let subscription = Request {
        id: "sub_worktrees".into(),
        method: Method::EventsSubscribe(EventsSubscribeParams {
            subscriptions: vec![
                Subscription::WorktreeCreated {},
                Subscription::WorktreeOpened {},
                Subscription::WorktreeRemoved {},
            ],
            notices: false,
        }),
    };
    let json = serde_json::to_string(&subscription).unwrap();
    assert!(json.contains("\"type\":\"worktree.created\""));
    assert!(json.contains("\"type\":\"worktree.opened\""));
    assert!(json.contains("\"type\":\"worktree.removed\""));
    let restored: Request = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, subscription);

    let workspace = WorkspaceInfo {
        workspace_id: "w_2".into(),
        number: 2,
        label: "herdr".into(),
        focused: true,
        pane_count: 1,
        tab_count: 1,
        active_tab_id: "w_2:1".into(),
        agent_status: AgentStatus::Unknown,
        tokens: HashMap::new(),
        worktree: Some(WorkspaceWorktreeInfo {
            repo_key: "/repo/herdr/.git".into(),
            repo_name: "herdr".into(),
            repo_root: "/repo/herdr".into(),
            checkout_path: "/worktrees/herdr/worktree-api".into(),
            is_linked_worktree: true,
        }),
    };
    let worktree = WorktreeInfo {
        path: "/worktrees/herdr/worktree-api".into(),
        branch: Some("worktree/api".into()),
        is_bare: false,
        is_detached: false,
        is_prunable: false,
        is_linked_worktree: true,
        open_workspace_id: Some("w_2".into()),
        label: "herdr".into(),
    };

    for event in [
        EventEnvelope {
            event: EventKind::WorktreeCreated,
            data: EventData::WorktreeCreated {
                workspace: workspace.clone(),
                worktree: worktree.clone(),
            },
        },
        EventEnvelope {
            event: EventKind::WorktreeOpened,
            data: EventData::WorktreeOpened {
                workspace: workspace.clone(),
                worktree: worktree.clone(),
                already_open: false,
            },
        },
        EventEnvelope {
            event: EventKind::WorktreeRemoved,
            data: EventData::WorktreeRemoved {
                workspace_id: "w_2".into(),
                workspace: Some(workspace.clone()),
                worktree: WorktreeInfo {
                    open_workspace_id: None,
                    ..worktree.clone()
                },
                forced: false,
            },
        },
        EventEnvelope {
            event: EventKind::WorkspaceClosed,
            data: EventData::WorkspaceClosed {
                workspace_id: "w_2".into(),
                workspace: Some(workspace.clone()),
            },
        },
    ] {
        let json = serde_json::to_string(&event).unwrap();
        let restored: EventEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, event);
    }
}

#[test]
fn plugin_link_list_unlink_round_trip() {
    let link = Request {
        id: "plugin_link".into(),
        method: Method::PluginLink(PluginLinkParams {
            path: "/plugins/worktree-bootstrap".into(),
            enabled: true,
            source: None,
        }),
    };
    let json = serde_json::to_string(&link).unwrap();
    assert!(json.contains("\"method\":\"plugin.link\""));
    let restored: Request = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, link);

    let list = Request {
        id: "plugin_list".into(),
        method: Method::PluginList(PluginListParams {
            plugin_id: Some("example.worktree-bootstrap".into()),
        }),
    };
    let json = serde_json::to_string(&list).unwrap();
    assert!(json.contains("\"method\":\"plugin.list\""));
    let restored: Request = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, list);

    let unlink = Request {
        id: "plugin_unlink".into(),
        method: Method::PluginUnlink(PluginUnlinkParams {
            plugin_id: "example.worktree-bootstrap".into(),
        }),
    };
    let json = serde_json::to_string(&unlink).unwrap();
    assert!(json.contains("\"method\":\"plugin.unlink\""));
    let restored: Request = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, unlink);

    let plugin = InstalledPluginInfo {
        plugin_id: "example.worktree-bootstrap".into(),
        name: "Worktree Bootstrap".into(),
        version: "0.1.0".into(),
        min_herdr_version: crate::build_info::BASE_VERSION.into(),
        description: Some("Prepare new worktrees".into()),
        manifest_path: "/plugins/worktree-bootstrap/herdr-plugin.toml".into(),
        plugin_root: "/plugins/worktree-bootstrap".into(),
        enabled: true,
        platforms: None,
        build: vec![PluginManifestBuild {
            platforms: None,
            command: vec!["bun".into(), "install".into()],
        }],
        startup: vec![],
        actions: vec![PluginManifestAction {
            id: "bootstrap".into(),
            title: "Bootstrap worktree".into(),
            description: None,
            contexts: vec![PluginActionContext::Workspace],
            platforms: None,
            command: vec!["bun".into(), "run".into(), "bootstrap.ts".into()],
        }],
        events: vec![PluginManifestEventHook {
            on: "worktree.created".into(),
            platforms: None,
            command: vec!["bun".into(), "run".into(), "bootstrap.ts".into()],
        }],
        panes: vec![PluginManifestPane {
            id: "board".into(),
            title: "Board".into(),
            description: None,
            platforms: None,
            placement: PluginPanePlacement::Overlay,
            width: None,
            height: None,
            command: vec!["bun".into(), "run".into(), "board.ts".into()],
        }],
        link_handlers: vec![PluginManifestLinkHandler {
            id: "github-pr".into(),
            title: "Open GitHub PR".into(),
            pattern: "^https://github.com/[^/]+/[^/]+/(issues|pull)/[0-9]+$".into(),
            action: "bootstrap".into(),
            platforms: None,
        }],
        source: Default::default(),
        warnings: vec![],
    };

    for response in [
        SuccessResponse {
            id: "plugin_link".into(),
            result: ResponseResult::PluginLinked {
                plugin: plugin.clone(),
            },
        },
        SuccessResponse {
            id: "plugin_list".into(),
            result: ResponseResult::PluginList {
                plugins: vec![plugin.clone()],
            },
        },
        SuccessResponse {
            id: "plugin_unlink".into(),
            result: ResponseResult::PluginUnlinked {
                plugin_id: plugin.plugin_id.clone(),
                removed: true,
            },
        },
    ] {
        let json = serde_json::to_string(&response).unwrap();
        let restored: SuccessResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, response);
    }
}

#[test]
fn layout_export_apply_round_trip() {
    let root = LayoutNode::Split {
        direction: SplitDirection::Right,
        ratio: 0.6,
        first: Box::new(LayoutNode::Pane {
            pane: LayoutPane {
                label: Some("editor".into()),
                cwd: Some("/repo".into()),
                ..Default::default()
            },
        }),
        second: Box::new(LayoutNode::Pane {
            pane: LayoutPane {
                label: Some("tests".into()),
                command: Some(vec!["sh".into(), "-c".into(), "just test".into()]),
                env: HashMap::from([("HERDR_ROLE".into(), "tests".into())]),
                ..Default::default()
            },
        }),
    };

    let export = Request {
        id: "layout_export".into(),
        method: Method::LayoutExport(LayoutExportParams {
            tab_id: Some("w1:1".into()),
            pane_id: None,
        }),
    };
    let json = serde_json::to_string(&export).unwrap();
    assert!(json.contains("\"method\":\"layout.export\""));
    let restored: Request = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, export);

    let apply = Request {
        id: "layout_apply".into(),
        method: Method::LayoutApply(LayoutApplyParams {
            workspace_id: Some("w1".into()),
            tab_id: None,
            tab_label: Some("dev".into()),
            focus: true,
            root: root.clone(),
        }),
    };
    let json = serde_json::to_string(&apply).unwrap();
    assert!(json.contains("\"method\":\"layout.apply\""));
    let restored: Request = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, apply);

    let response = SuccessResponse {
        id: "layout_export".into(),
        result: ResponseResult::LayoutExport {
            layout: LayoutDescription {
                workspace_id: "w1".into(),
                tab_id: "w1:1".into(),
                zoomed: false,
                focused_pane_id: "w1-1".into(),
                root,
            },
        },
    };
    let json = serde_json::to_string(&response).unwrap();
    let restored: SuccessResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, response);

    let response = SuccessResponse {
        id: "layout_ratio".into(),
        result: ResponseResult::LayoutSplitRatioSet {
            layout: LayoutDescription {
                workspace_id: "w1".into(),
                tab_id: "w1:1".into(),
                zoomed: false,
                focused_pane_id: "w1-1".into(),
                root: LayoutNode::Pane {
                    pane: LayoutPane {
                        pane_id: Some("w1-1".into()),
                        ..Default::default()
                    },
                },
            },
        },
    };
    let json = serde_json::to_string(&response).unwrap();
    assert!(json.contains("\"type\":\"layout_split_ratio_set\""));
    let restored: SuccessResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, response);
}

#[test]
fn authority_mutation_requests_round_trip() {
    let workspace_move = Request {
        id: "move_ws".into(),
        method: Method::WorkspaceMove(WorkspaceMoveParams {
            workspace_id: "w1".into(),
            insert_index: 2,
        }),
    };
    let json = serde_json::to_value(&workspace_move).unwrap();
    assert_eq!(json["method"], "workspace.move");
    let restored: Request = serde_json::from_value(json).unwrap();
    assert_eq!(restored, workspace_move);

    let workspace_move_block = Request {
        id: "move_ws_block".into(),
        method: Method::WorkspaceMoveBlock(WorkspaceMoveBlockParams {
            workspace_ids: vec!["w1".into(), "w2".into()],
            before_workspace_id: Some("w3".into()),
        }),
    };
    let json = serde_json::to_value(&workspace_move_block).unwrap();
    assert_eq!(json["method"], "workspace.move_block");
    let restored: Request = serde_json::from_value(json).unwrap();
    assert_eq!(restored, workspace_move_block);

    let tab_move = Request {
        id: "move_tab".into(),
        method: Method::TabMove(TabMoveParams {
            tab_id: "w1:1".into(),
            insert_index: 1,
        }),
    };
    let json = serde_json::to_value(&tab_move).unwrap();
    assert_eq!(json["method"], "tab.move");
    let restored: Request = serde_json::from_value(json).unwrap();
    assert_eq!(restored, tab_move);

    let pane_focus = Request {
        id: "focus_pane".into(),
        method: Method::PaneFocus(PaneTarget {
            pane_id: "w1:1".into(),
        }),
    };
    let json = serde_json::to_value(&pane_focus).unwrap();
    assert_eq!(json["method"], "pane.focus");
    let restored: Request = serde_json::from_value(json).unwrap();
    assert_eq!(restored, pane_focus);

    let split_ratio = Request {
        id: "set_ratio".into(),
        method: Method::LayoutSetSplitRatio(LayoutSetSplitRatioParams {
            tab_id: Some("w1:1".into()),
            pane_id: None,
            path: vec![false, true],
            ratio: 0.6,
        }),
    };
    let json = serde_json::to_value(&split_ratio).unwrap();
    assert_eq!(json["method"], "layout.set_split_ratio");
    let restored: Request = serde_json::from_value(json).unwrap();
    assert_eq!(restored, split_ratio);

    let subscription = Request {
        id: "sub_moves".into(),
        method: Method::EventsSubscribe(EventsSubscribeParams {
            subscriptions: vec![
                Subscription::WorkspaceMoved {},
                Subscription::WorkspaceReordered {},
                Subscription::TabMoved {},
                Subscription::LayoutUpdated {},
            ],
            notices: false,
        }),
    };
    let json = serde_json::to_string(&subscription).unwrap();
    assert!(json.contains("\"type\":\"workspace.moved\""));
    assert!(json.contains("\"type\":\"workspace.reordered\""));
    assert!(json.contains("\"type\":\"tab.moved\""));
    assert!(json.contains("\"type\":\"layout.updated\""));
    let restored: Request = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, subscription);
}

#[test]
fn create_response_round_trips_with_root_pane() {
    let response = SuccessResponse {
        id: "req_2".into(),
        result: ResponseResult::TabCreated {
            tab: TabInfo {
                tab_id: "w_1:2".into(),
                workspace_id: "w_1".into(),
                number: 2,
                label: "review".into(),
                focused: false,
                pane_count: 1,
                agent_status: AgentStatus::Unknown,
            },
            root_pane: PaneInfo {
                pane_id: "w_1-3".into(),
                terminal_id: "term_example".into(),
                workspace_id: "w_1".into(),
                tab_id: "w_1:2".into(),
                focused: false,
                cwd: Some("/tmp/review".into()),
                foreground_cwd: None,
                restore_error: None,
                label: None,
                agent: None,
                title: None,
                terminal_title: None,
                terminal_title_stripped: None,
                display_agent: None,
                agent_status: AgentStatus::Unknown,
                state_labels: HashMap::new(),
                tokens: HashMap::new(),
                agent_session: None,
                scroll: None,
                revision: 0,
            },
        },
    };

    let json = serde_json::to_string(&response).unwrap();
    assert!(json.contains("\"type\":\"tab_created\""));
    assert!(json.contains("\"root_pane\""));
    let restored: SuccessResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, response);
}

#[test]
fn error_response_round_trips() {
    let response = ErrorResponse {
        id: "req_1".into(),
        error: ErrorBody {
            code: "pane_not_found".into(),
            message: "pane p_1 not found".into(),
        },
    };

    let json = serde_json::to_string(&response).unwrap();
    let restored: ErrorResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, response);
}

#[test]
fn event_wait_parses_typed_match() {
    let json = r#"
    {
        "id": "req_9",
        "method": "events.wait",
        "params": {
            "match_event": {
                "event": "pane_agent_status_changed",
                "pane_id": "p_1",
                "agent_status": "done"
            },
            "timeout_ms": 30000
        }
    }
    "#;

    let request: Request = serde_json::from_str(json).unwrap();
    let Method::EventsWait(params) = request.method else {
        panic!("wrong method parsed");
    };
    assert_eq!(
        params.match_event,
        EventMatch::PaneAgentStatusChanged {
            pane_id: "p_1".into(),
            agent_status: AgentStatus::Done,
        }
    );
}

#[test]
fn pane_link_activate_round_trips() {
    let request = Request {
        id: "req_pane_link".into(),
        method: Method::PaneLinkActivate(PaneLinkActivateParams {
            pane_id: "w1:p1".into(),
            viewport_row: 3,
            col: 7,
            content_revision: Some(42),
            offset_from_bottom: Some(5),
        }),
    };
    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["method"], "pane.link.activate");
    let restored: Request = serde_json::from_value(json).unwrap();
    assert_eq!(restored, request);

    let response = ResponseResult::PaneLinkActivated {
        url: Some("https://example.test".into()),
        handled: false,
    };
    let json = serde_json::to_string(&response).unwrap();
    let restored: ResponseResult = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, response);
}

#[test]
fn plugin_action_list_and_invoke_round_trips() {
    let list = Request {
        id: "req_plugin_action_list".into(),
        method: Method::PluginActionList(PluginActionListParams {
            plugin_id: Some("example.issue-flow".into()),
        }),
    };
    let json = serde_json::to_value(&list).unwrap();
    assert_eq!(json["method"], "plugin.action.list");
    let restored: Request = serde_json::from_value(json).unwrap();
    assert_eq!(restored, list);

    let invoke = Request {
        id: "req_plugin_action_invoke".into(),
        method: Method::PluginActionInvoke(PluginActionInvokeParams {
            plugin_id: Some("example.issue-flow".into()),
            action_id: "assign-issue".into(),
            context: None,
        }),
    };
    let json = serde_json::to_value(&invoke).unwrap();
    assert_eq!(json["method"], "plugin.action.invoke");
    let restored: Request = serde_json::from_value(json).unwrap();
    assert_eq!(restored, invoke);

    let action_info = PluginActionInfo {
        plugin_id: "example.issue-flow".into(),
        action_id: "assign-issue".into(),
        title: "Assign Issue".into(),
        description: Some("Open the issue assignment UI".into()),
        contexts: vec![PluginActionContext::Workspace, PluginActionContext::Pane],
        command: vec!["assign".into(), "--issue".into()],
        platforms: Some(vec![PluginPlatform::Linux, PluginPlatform::Macos]),
    };
    assert_eq!(
        action_info.qualified_id(),
        "example.issue-flow.assign-issue"
    );
    let json = serde_json::to_string(&action_info).unwrap();
    let restored: PluginActionInfo = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, action_info);
}

#[test]
fn plugin_pane_open_request_round_trips() {
    let request = Request {
        id: "req_plugin_pane".into(),
        method: Method::PluginPaneOpen(PluginPaneOpenParams {
            plugin_id: "example.board".into(),
            entrypoint: "board".into(),
            placement: Some(PluginPanePlacement::Popup),
            width: Some(crate::popup_size::PopupSize::Cells(90)),
            height: Some(crate::popup_size::PopupSize::Percent(80)),
            workspace_id: None,
            target_pane_id: None,
            direction: None,
            cwd: Some("/tmp".into()),
            focus: true,
            env: [("HERDR_ROLE".to_string(), "board".to_string())].into(),
        }),
    };

    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["method"], "plugin.pane.open");
    assert_eq!(json["params"]["placement"], "popup");
    assert_eq!(json["params"]["width"], 90);
    assert_eq!(json["params"]["height"], "80%");
    assert_eq!(json["params"]["env"]["HERDR_ROLE"], "board");
    let restored: Request = serde_json::from_value(json).unwrap();
    assert_eq!(restored, request);
}

#[test]
fn popup_close_request_round_trips() {
    let request = Request {
        id: "popup-close".into(),
        method: Method::PopupClose(EmptyParams::default()),
    };

    let json = serde_json::to_value(request).unwrap();

    assert_eq!(json["method"], "popup.close");
    assert_eq!(json["params"], serde_json::json!({}));
}

#[test]
fn pane_link_resolve_round_trips() {
    let request: Request = serde_json::from_value(serde_json::json!({
        "id": "hover", "method": "pane.link.resolve",
        "params": {"pane_id": "pane-1", "viewport_row": 2, "col": 3}
    }))
    .unwrap();
    assert!(matches!(request.method, Method::PaneLinkResolve(_)));
    let result = ResponseResult::PaneLinkResolved {
        regions: vec![PaneLinkRegion {
            row: 2,
            start_col: 3,
            end_col: 9,
        }],
    };
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["type"], "pane_link_resolved");
    assert_eq!(json["regions"][0]["end_col"], 9);
    assert_eq!(
        serde_json::from_value::<ResponseResult>(json).unwrap(),
        result
    );
}

// ---- B-12：观测事件类型化 ----

#[test]
fn observation_event_envelope_keeps_the_legacy_wire_shape() {
    let snapshot = AccountUsageSnapshot {
        account_id: "claude:default".into(),
        agent: "claude".into(),
        status: ObservationStatus::Ready,
        ..Default::default()
    };
    let updated = ObservationEventEnvelope::AccountUsageUpdated(AccountUsageUpdatedEvent {
        accounts: vec![snapshot],
        refresh: None,
    });
    let json = serde_json::to_value(&updated).unwrap();
    // 顶层只有 event/data 两个键，与类型化之前的裸 JSON 完全一致；data 不带 serde 标签。
    assert_eq!(json["event"], "account.usage.updated");
    assert_eq!(json.as_object().map(|object| object.len()), Some(2));
    assert_eq!(json["data"]["accounts"][0]["account_id"], "claude:default");
    assert!(json["data"].get("type").is_none());
    assert!(
        json["data"].get("refresh").is_none(),
        "缺省的 refresh 不占键"
    );
    assert_eq!(
        serde_json::from_value::<ObservationEventEnvelope>(json).unwrap(),
        updated
    );

    let metrics = ObservationEventEnvelope::SystemMetricsUpdated(SystemMetricsUpdatedEvent {
        snapshot: Box::new(SystemMetricsSnapshot {
            sequence: 7,
            ..Default::default()
        }),
    });
    let json = serde_json::to_value(&metrics).unwrap();
    assert_eq!(json["event"], "system.metrics.updated");
    assert_eq!(json["data"]["snapshot"]["sequence"], 7);
    assert_eq!(
        serde_json::from_value::<ObservationEventEnvelope>(json).unwrap(),
        metrics
    );

    let refreshing =
        ObservationEventEnvelope::AccountUsageRefreshing(AccountUsageRefreshingEvent {
            refresh: vec![UsageRefreshState {
                account_id: "claude:default".into(),
                in_flight: true,
                queued: true,
                ..Default::default()
            }],
        });
    let json = serde_json::to_value(&refreshing).unwrap();
    assert_eq!(json["event"], "account.usage.refreshing");
    assert_eq!(json["data"]["refresh"][0]["in_flight"], true);
    assert!(json["data"].get("accounts").is_none(), "探测开始不带快照");
    assert_eq!(
        serde_json::from_value::<ObservationEventEnvelope>(json).unwrap(),
        refreshing
    );
}

#[test]
fn observation_event_kinds_match_envelope_tags() {
    let cases = [
        (
            ObservationEventKind::AccountUsageUpdated,
            ObservationEventEnvelope::AccountUsageUpdated(AccountUsageUpdatedEvent::default()),
        ),
        (
            ObservationEventKind::AccountUsageRefreshing,
            ObservationEventEnvelope::AccountUsageRefreshing(AccountUsageRefreshingEvent::default()),
        ),
        (
            ObservationEventKind::SystemMetricsUpdated,
            ObservationEventEnvelope::SystemMetricsUpdated(SystemMetricsUpdatedEvent::default()),
        ),
    ];
    for (kind, envelope) in cases {
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["event"], kind.dot_name(), "{kind:?} 的 event 标签");
        assert_eq!(
            serde_json::to_value(kind).unwrap(),
            kind.dot_name(),
            "{kind:?} 单独序列化与信封标签一致"
        );
        assert_eq!(envelope.kind(), kind);
        assert_eq!(
            serde_json::from_value::<ObservationEventKind>(json["event"].clone()).unwrap(),
            kind
        );
    }
    // 未知种类回落为 Unknown，客户端据此忽略新 server 的事件。
    assert_eq!(
        serde_json::from_str::<ObservationEventKind>(r#""account.usage.future""#).unwrap(),
        ObservationEventKind::Unknown
    );
    assert_eq!(ObservationEventKind::Unknown.dot_name(), "unknown");
}

#[test]
fn protocol_schema_entry_set_is_a_contract() {
    let document = protocol_schema_document();
    let schemas = document["schemas"].as_object().unwrap();
    // 按**集合**比对，与序列化顺序无关：`serde_json` 的 `preserve_order`
    // 特性可能被任何一个传递依赖打开（Cargo feature 取并集），届时按数组比对
    // 会以「顺序不对」而不是「条目不对」的形式变红。
    let mut keys = schemas.keys().map(String::as_str).collect::<Vec<_>>();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "error_response",
            "event",
            "event_stream_notice",
            "observation_event",
            "request",
            "stream_event",
            "subscription_event",
            "success_response",
        ],
        "schema 条目集合是契约的一部分，新增只能追加"
    );
}

#[test]
fn protocol_schema_registers_observation_events() {
    let document = protocol_schema_document();
    let schemas = document["schemas"].as_object().unwrap();
    let entry = &schemas["observation_event"];
    let tags = entry["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .map(|branch| branch["properties"]["event"]["const"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        tags,
        [
            "account.usage.updated",
            "account.usage.refreshing",
            "system.metrics.updated",
        ]
    );
    // 每个分支都要求 event 与 data 同时存在，data 引用类型化负载。
    for branch in entry["oneOf"].as_array().unwrap() {
        assert_eq!(
            branch["required"],
            serde_json::json!(["event", "data"]),
            "{}",
            branch["properties"]["event"]["const"]
        );
        assert!(branch["properties"]["data"]["$ref"]
            .as_str()
            .is_some_and(|reference| reference.starts_with("#/schemas/observation_event/")));
    }
    let defs = entry["$defs"].as_object().unwrap();
    assert!(defs.contains_key("AccountUsageSnapshot"));
    assert!(defs.contains_key("UsageRefreshState"));
    assert!(defs.contains_key("SystemMetricsSnapshot"));
    // Unknown 回落只在解码侧存在：种类枚举单独生成 schema 时也不列出它。
    let kind = serde_json::to_value(schemars::schema_for!(ObservationEventKind)).unwrap();
    let names = kind["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .map(|variant| variant["const"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, tags);
}
