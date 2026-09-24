use super::ClientEndpointId;

pub(crate) struct DecodedAgentViewProjection {
    pub(crate) boot_id: String,
    pub(crate) revision: u64,
    pub(crate) view: Result<Option<crate::api::schema::AgentViewSetParams>, ()>,
}

pub(crate) enum EndpointControlMessage {
    HealthPong,
    AgentViewProjection(DecodedAgentViewProjection),
    AgentCompletions(crate::protocol::endpoint::EndpointAgentCompletions),
    Snapshot(Box<crate::protocol::ClientShellSnapshot>),
    /// 后台观测订阅推送的 `endpoint.observation.v1` 事件（账号用量 / 系统指标）。
    Observation(Box<crate::protocol::endpoint::EndpointObservationEvent>),
    /// 前台焦点 pane 的 cwd 上送（`endpoint.terminal-cwd.v1`），客户端写 OSC 7。
    TerminalCwd(Option<String>),
    Ignored,
}

pub(crate) fn decode_endpoint_control(
    kind: &str,
    data: &str,
) -> Result<EndpointControlMessage, String> {
    if kind == crate::protocol::endpoint::HEALTH_PONG_KIND {
        return Ok(EndpointControlMessage::HealthPong);
    }
    if kind == crate::protocol::endpoint::AGENT_COMPLETIONS_KIND {
        return Ok(serde_json::from_str(data)
            .map(EndpointControlMessage::AgentCompletions)
            .unwrap_or(EndpointControlMessage::Ignored));
    }
    if kind == crate::protocol::endpoint::AGENT_VIEW_PROJECTION_KIND {
        let Ok(projection): Result<crate::protocol::endpoint::EndpointAgentViewProjection, _> =
            serde_json::from_str(data)
        else {
            return Ok(EndpointControlMessage::Ignored);
        };
        let view = match projection.view {
            Some(value) => serde_json::from_value(value)
                .map(Some)
                .map_err(|_| ())
                .and_then(|mut view| {
                    view.as_mut()
                        .map(crate::app::agent_view::validate_agent_view)
                        .transpose()
                        .map(|_| view)
                        .map_err(|_| ())
                }),
            None => Ok(None),
        };
        return Ok(EndpointControlMessage::AgentViewProjection(
            DecodedAgentViewProjection {
                boot_id: projection.boot_id,
                revision: projection.revision,
                view,
            },
        ));
    }
    if kind == crate::protocol::endpoint::OBSERVATION_EVENT_KIND {
        // 可选控制帧：新 server 推来本版本不认识的事件种类时整帧解码失败，按未知
        // 可选控制忽略，不能因此断开端点。
        return Ok(
            serde_json::from_str(data).map_or(EndpointControlMessage::Ignored, |event| {
                EndpointControlMessage::Observation(Box::new(event))
            }),
        );
    }
    if kind == crate::protocol::endpoint::TERMINAL_CWD_KIND {
        // 与观测事件同口径的可选控制帧：坏载荷按忽略处理。
        return Ok(
            serde_json::from_str::<crate::protocol::endpoint::EndpointTerminalCwd>(data).map_or(
                EndpointControlMessage::Ignored,
                |frame| {
                    EndpointControlMessage::TerminalCwd(
                        frame.uri.filter(|uri: &String| !uri.is_empty()),
                    )
                },
            ),
        );
    }
    if kind == crate::protocol::endpoint::ENDPOINT_SNAPSHOT_KIND {
        let snapshot = serde_json::from_str(data)
            .map_err(|error| format!("invalid endpoint snapshot: {error}"))?;
        return Ok(EndpointControlMessage::Snapshot(Box::new(snapshot)));
    }
    if kind.starts_with("shell.snapshot.") {
        return Err(format!(
            "unsupported mandatory endpoint snapshot codec {kind:?}"
        ));
    }
    Ok(EndpointControlMessage::Ignored)
}

pub(crate) fn protocol_failure_is_fatal(endpoint_id: &ClientEndpointId) -> bool {
    endpoint_id.is_local()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::endpoint::ProfileId;

    #[test]
    fn unknown_optional_controls_are_ignored() {
        assert!(matches!(
            decode_endpoint_control("future.optional", "not json").unwrap(),
            EndpointControlMessage::Ignored
        ));
    }

    #[test]
    fn terminal_cwd_decodes_uri_and_tolerates_bad_payloads() {
        let crate::protocol::ServerMessage::EndpointControl { kind, data } =
            crate::protocol::endpoint::terminal_cwd_message(Some("file://host/tmp%20x".to_owned()))
                .unwrap()
        else {
            panic!("terminal cwd control");
        };
        let EndpointControlMessage::TerminalCwd(uri) =
            decode_endpoint_control(&kind, &data).unwrap()
        else {
            panic!("decoded terminal cwd");
        };
        assert_eq!(uri.as_deref(), Some("file://host/tmp%20x"));

        for data in ["not json", r#"{"uri":""}"#, r#"{"uri":null}"#] {
            let decoded =
                decode_endpoint_control(crate::protocol::endpoint::TERMINAL_CWD_KIND, data)
                    .unwrap();
            match decoded {
                EndpointControlMessage::TerminalCwd(None) => {}
                EndpointControlMessage::Ignored => {
                    assert_eq!(data, "not json");
                }
                _ => panic!("unexpected decode for {data}"),
            }
        }
    }

    #[test]
    fn completion_guard_optional_control_round_trips_without_changing_snapshot_codec() {
        let projection = crate::protocol::endpoint::EndpointAgentCompletions {
            boot_id: "boot".into(),
            revision: 3,
            completions: [("pane".into(), 7)].into_iter().collect(),
        };
        let crate::protocol::ServerMessage::EndpointControl { kind, data } =
            crate::protocol::endpoint::agent_completions_message(&projection).unwrap()
        else {
            panic!("expected optional control");
        };
        let EndpointControlMessage::AgentCompletions(decoded) =
            decode_endpoint_control(&kind, &data).unwrap()
        else {
            panic!("expected completion projection");
        };
        assert_eq!(decoded, projection);
        assert!(matches!(
            decode_endpoint_control(&kind, "invalid").unwrap(),
            EndpointControlMessage::Ignored
        ));
    }

    #[test]
    fn agent_view_projection_decodes_and_validates_the_view() {
        let view = crate::api::schema::AgentViewSetParams {
            source: "example.views".into(),
            label: Some("focus".into()),
            filter: None,
            sort: Vec::new(),
        };
        let crate::protocol::ServerMessage::EndpointControl { kind, data } =
            crate::protocol::endpoint::agent_view_projection_message("boot", 4, Some(&view))
                .unwrap()
        else {
            panic!("projection control");
        };
        let EndpointControlMessage::AgentViewProjection(decoded) =
            decode_endpoint_control(&kind, &data).unwrap()
        else {
            panic!("decoded projection");
        };
        assert_eq!(decoded.boot_id, "boot");
        assert_eq!(decoded.revision, 4);
        assert_eq!(decoded.view, Ok(Some(view)));
    }

    #[test]
    fn unsupported_agent_view_payload_falls_back_without_rejecting_endpoint() {
        let projection = crate::protocol::endpoint::EndpointAgentViewProjection {
            boot_id: "boot".into(),
            revision: 5,
            view: Some(serde_json::json!({
                "source": "example.views",
                "filter": {"op": "future_filter"}
            })),
        };
        let decoded = decode_endpoint_control(
            crate::protocol::endpoint::AGENT_VIEW_PROJECTION_KIND,
            &serde_json::to_string(&projection).unwrap(),
        )
        .unwrap();
        let EndpointControlMessage::AgentViewProjection(decoded) = decoded else {
            panic!("decoded projection");
        };
        assert_eq!(decoded.view, Err(()));
    }

    #[test]
    fn malformed_agent_view_envelope_is_ignored() {
        assert!(matches!(
            decode_endpoint_control(
                crate::protocol::endpoint::AGENT_VIEW_PROJECTION_KIND,
                "not json"
            )
            .unwrap(),
            EndpointControlMessage::Ignored
        ));
    }

    #[test]
    fn observation_events_decode_into_typed_frames_and_unknown_kinds_are_ignored() {
        use crate::api::schema::{
            AccountUsageRefreshingEvent, ObservationEventEnvelope, UsageRefreshState,
        };
        let frame = crate::protocol::endpoint::EndpointObservationEvent {
            boot_id: "boot".into(),
            event: ObservationEventEnvelope::AccountUsageRefreshing(AccountUsageRefreshingEvent {
                refresh: vec![UsageRefreshState {
                    account_id: "claude:default".into(),
                    in_flight: true,
                    ..Default::default()
                }],
            }),
        };
        let decoded = decode_endpoint_control(
            crate::protocol::endpoint::OBSERVATION_EVENT_KIND,
            &serde_json::to_string(&frame).unwrap(),
        )
        .unwrap();
        let EndpointControlMessage::Observation(decoded) = decoded else {
            panic!("decoded observation event");
        };
        assert_eq!(*decoded, frame);
        // 未来的事件种类与坏载荷都按可选控制忽略，不拒绝端点。
        for data in [
            r#"{"boot_id":"boot","event":"account.usage.future","data":{}}"#,
            "not json",
        ] {
            assert!(matches!(
                decode_endpoint_control(crate::protocol::endpoint::OBSERVATION_EVENT_KIND, data)
                    .unwrap(),
                EndpointControlMessage::Ignored
            ));
        }
    }

    #[test]
    fn unknown_snapshot_codecs_are_rejected() {
        assert_eq!(
            decode_endpoint_control("shell.snapshot.v2", "{}")
                .err()
                .as_deref(),
            Some("unsupported mandatory endpoint snapshot codec \"shell.snapshot.v2\"")
        );
    }

    #[test]
    fn only_local_protocol_failures_end_the_client() {
        let remote =
            ClientEndpointId::Ssh(ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap());
        assert!(protocol_failure_is_fatal(&ClientEndpointId::Local));
        assert!(!protocol_failure_is_fatal(&remote));
    }
}
