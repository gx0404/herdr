//! Stable endpoint compatibility contract for client-owned shells.
//!
//! The endpoint generation is intentionally independent from the private
//! binary protocol used by same-install CLI, direct-terminal, and handoff
//! paths. Generation 1 is the compatibility floor for Local, SSH, and Cloud
//! shell endpoints and must remain available indefinitely unless retired for a
//! security reason. New JSON fields must be optional or have serde defaults;
//! new enum values need an `Unknown` fallback. Unknown named controls are
//! optional and ignored unless negotiated as part of the core.

use serde::{Deserialize, Serialize};

use super::{ClientShellSnapshot, ClientSurfaceSize, ServerMessage};

pub const ENDPOINT_PROTOCOL_GENERATION: u32 = 1;
pub const ENDPOINT_HELLO_KIND: &str = "endpoint.hello.v1";
pub const ENDPOINT_WELCOME_KIND: &str = "endpoint.welcome.v1";
pub const SNAPSHOT_CODEC_V1: &str = "shell.snapshot.v1";
pub const ENDPOINT_SNAPSHOT_KIND: &str = SNAPSHOT_CODEC_V1;
pub const SURFACE_CODEC_V1: &str = "shell.surface.v1";
pub const INPUT_CODEC_V1: &str = "shell.input.semantic.v1";
pub const BLOB_CODEC_V1: &str = "shell.blob.v1";
pub const SURFACE_INTEREST_CAPABILITY: &str = "surface_interest";
pub const PRESENTATION_EFFECTS_FENCE_CAPABILITY: &str = "presentation_effects_fence";
pub const PRESENTATION_EFFECTS_SYNC_KIND: &str = "endpoint.presentation.sync.v1";
pub const PRESENTATION_EFFECTS_READY_KIND: &str = "endpoint.presentation.ready.v1";
pub const HEALTH_CHECK_CAPABILITY: &str = "health_check";
pub const HEALTH_PING_KIND: &str = "endpoint.health.ping.v1";
pub const HEALTH_PONG_KIND: &str = "endpoint.health.pong.v1";
pub const AGENT_VIEW_PROJECTION_CAPABILITY: &str = "agent_view_projection";
pub const AGENT_VIEW_PROJECTION_KIND: &str = "endpoint.agent-view.v1";
/// 后台观测事件（账号用量 / 系统指标订阅推送）的可选控制帧；客户端不认识时忽略。
pub const OBSERVATION_EVENT_KIND: &str = "endpoint.observation.v1";
/// 前台焦点 pane 的 cwd 上送（WEZ-INT-02）：server 只在焦点或 cwd 变化时推送，
/// 客户端据此向宿主终端写 OSC 7。可选控制帧，旧客户端忽略。
pub const TERMINAL_CWD_KIND: &str = "endpoint.terminal-cwd.v1";

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointClientHello {
    pub generation: u32,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
    pub surface_size: ClientSurfaceSize,
    pub pixel_mouse: bool,
    pub direct_graphics: bool,
    pub endpoint_keybindings: bool,
    pub mouse_capture: bool,
    #[serde(default = "default_true")]
    pub surface_active: bool,
    /// Accept the optional cell-retaining surface encoding on this connection.
    #[serde(default)]
    pub surface_reuse: bool,
    #[serde(default)]
    pub snapshot_codecs: Vec<String>,
    #[serde(default)]
    pub surface_codecs: Vec<String>,
    #[serde(default)]
    pub input_codecs: Vec<String>,
    #[serde(default)]
    pub blob_codecs: Vec<String>,
    /// 前台 client 宿主环境里的 SSH agent socket。server 是 detached 进程，自身继承的
    /// `SSH_AUTH_SOCK` 会随宿主终端重启失效；pane spawn 前校验不通过时用这个上报值兜底
    /// （使用端仍会校验属主与 socket 类型）。纯追加可选字段：老 client 不发、老 server 忽略。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_auth_sock: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointHandshakeError {
    pub code: String,
    pub message: String,
}

/// Optional companion to a V1 snapshot. The view payload is intentionally opaque to this
/// stable envelope so clients can ignore view language additions they do not understand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointAgentViewProjection {
    pub boot_id: String,
    pub revision: u64,
    #[serde(default)]
    pub view: Option<serde_json::Value>,
}

/// `endpoint.observation.v1` 控制帧的载荷：观测事件信封（`event` + `data`）外再带
/// `boot_id`，客户端据此丢弃早于当前 server 启动的推送。事件本身是合并丢帧语义。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EndpointObservationEvent {
    pub boot_id: String,
    #[serde(flatten)]
    pub event: crate::api::schema::ObservationEventEnvelope,
}

/// `endpoint.terminal-cwd.v1` 控制帧的载荷：前台焦点 pane 的 cwd 的 `file://` URI
/// （server 侧完成 hostname 与 percent 编码）；`None` 表示当前没有可上送的 cwd。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointTerminalCwd {
    pub uri: Option<String>,
}

pub fn terminal_cwd_message(uri: Option<String>) -> serde_json::Result<ServerMessage> {
    Ok(ServerMessage::EndpointControl {
        kind: TERMINAL_CWD_KIND.into(),
        data: serde_json::to_string(&EndpointTerminalCwd { uri })?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointServerWelcome {
    pub generation: u32,
    pub server_version: String,
    pub snapshot_codec: String,
    pub surface_codec: String,
    pub input_codec: String,
    pub blob_codec: String,
    #[serde(default)]
    pub methods: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<EndpointHandshakeError>,
}

pub fn snapshot_message(snapshot: &ClientShellSnapshot) -> serde_json::Result<ServerMessage> {
    Ok(ServerMessage::EndpointControl {
        kind: ENDPOINT_SNAPSHOT_KIND.into(),
        data: serde_json::to_string(snapshot)?,
    })
}

pub fn agent_view_projection_message(
    boot_id: &str,
    revision: u64,
    view: Option<&crate::api::schema::AgentViewSetParams>,
) -> serde_json::Result<ServerMessage> {
    let projection = EndpointAgentViewProjection {
        boot_id: boot_id.to_owned(),
        revision,
        view: view.map(serde_json::to_value).transpose()?,
    };
    Ok(ServerMessage::EndpointControl {
        kind: AGENT_VIEW_PROJECTION_KIND.into(),
        data: serde_json::to_string(&projection)?,
    })
}

impl EndpointClientHello {
    pub fn supports_required_codecs(&self) -> bool {
        self.snapshot_codecs
            .iter()
            .any(|codec| codec == SNAPSHOT_CODEC_V1)
            && self
                .surface_codecs
                .iter()
                .any(|codec| codec == SURFACE_CODEC_V1)
            && self
                .input_codecs
                .iter()
                .any(|codec| codec == INPUT_CODEC_V1)
            && self.blob_codecs.iter().any(|codec| codec == BLOB_CODEC_V1)
    }
}

impl EndpointServerWelcome {
    pub fn compatible(methods: Vec<String>) -> Self {
        Self {
            generation: ENDPOINT_PROTOCOL_GENERATION,
            server_version: crate::build_info::version().to_owned(),
            snapshot_codec: SNAPSHOT_CODEC_V1.into(),
            surface_codec: SURFACE_CODEC_V1.into(),
            input_codec: INPUT_CODEC_V1.into(),
            blob_codec: BLOB_CODEC_V1.into(),
            methods,
            capabilities: vec![
                super::views::CAPABILITY.into(),
                super::surface_reuse::CAPABILITY.into(),
                SURFACE_INTEREST_CAPABILITY.into(),
                PRESENTATION_EFFECTS_FENCE_CAPABILITY.into(),
                HEALTH_CHECK_CAPABILITY.into(),
                AGENT_VIEW_PROJECTION_CAPABILITY.into(),
            ],
            error: None,
        }
    }

    pub fn incompatible(code: &str, message: impl Into<String>) -> Self {
        Self {
            generation: ENDPOINT_PROTOCOL_GENERATION,
            server_version: crate::build_info::version().to_owned(),
            snapshot_codec: SNAPSHOT_CODEC_V1.into(),
            surface_codec: SURFACE_CODEC_V1.into(),
            input_codec: INPUT_CODEC_V1.into(),
            blob_codec: BLOB_CODEC_V1.into(),
            methods: Vec::new(),
            capabilities: Vec::new(),
            error: Some(EndpointHandshakeError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observation_event_frames_flatten_the_envelope_next_to_boot_id() {
        use crate::api::schema::{
            AccountUsageRefreshingEvent, ObservationEventEnvelope, UsageRefreshState,
        };
        let frame = EndpointObservationEvent {
            boot_id: "boot".into(),
            event: ObservationEventEnvelope::AccountUsageRefreshing(AccountUsageRefreshingEvent {
                refresh: vec![UsageRefreshState {
                    account_id: "claude:default".into(),
                    in_flight: true,
                    ..Default::default()
                }],
            }),
        };
        let text = serde_json::to_string(&frame).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        // 与 generation 1 之前的裸 JSON 同形：boot_id / event / data 三个顶层键。
        assert_eq!(value["boot_id"], "boot");
        assert_eq!(value["event"], "account.usage.refreshing");
        assert_eq!(value["data"]["refresh"][0]["account_id"], "claude:default");
        assert_eq!(value.as_object().map(|object| object.len()), Some(3));
        let restored: EndpointObservationEvent = serde_json::from_str(&text).unwrap();
        assert_eq!(restored, frame);
        // 未知事件种类整帧解码失败，客户端按可选控制帧忽略。
        assert!(serde_json::from_str::<EndpointObservationEvent>(
            r#"{"boot_id":"boot","event":"account.usage.future","data":{}}"#
        )
        .is_err());
    }

    fn hello() -> EndpointClientHello {
        EndpointClientHello {
            generation: ENDPOINT_PROTOCOL_GENERATION,
            cell_width_px: 8,
            cell_height_px: 16,
            surface_size: ClientSurfaceSize { cols: 80, rows: 24 },
            pixel_mouse: true,
            direct_graphics: false,
            endpoint_keybindings: false,
            mouse_capture: true,
            surface_active: true,
            surface_reuse: false,
            snapshot_codecs: vec![SNAPSHOT_CODEC_V1.into()],
            surface_codecs: vec![SURFACE_CODEC_V1.into()],
            input_codecs: vec![INPUT_CODEC_V1.into()],
            blob_codecs: vec![BLOB_CODEC_V1.into()],
            ssh_auth_sock: None,
        }
    }

    fn snapshot() -> ClientShellSnapshot {
        ClientShellSnapshot {
            boot_id: "boot".into(),
            revision: 1,
            config_diagnostic: None,
            product_announcement: None,
            update_available: None,
            update_install_command: "herdr update".into(),
            server_keybindings_toml: None,
            latest_release_notes_available: false,
            integration_updates_available: false,
            worktree_directory: String::new(),
            release_notes: None,
            focused_workspace_id: None,
            focused_tab_id: None,
            focused_pane_id: None,
            tab_bar_right: Vec::new(),
            tab_bar_right_separator: String::new(),
            agent_view_label: None,
            agent_order: Vec::new(),
            workspaces: Vec::new(),
            tabs: Vec::new(),
            panes: Vec::new(),
            agents: Vec::new(),
            commands: Vec::new(),
            external_agents: Vec::new(),
        }
    }

    #[test]
    fn hello_ignores_future_named_fields() {
        let mut value = serde_json::to_value(hello()).unwrap();
        value["future_feature"] = serde_json::json!({"enabled": true});
        let decoded: EndpointClientHello = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, hello());
    }

    #[test]
    fn hello_ssh_auth_sock_is_an_optional_append_only_field() {
        // 旧 client 的 hello 没有该键，解码必须是 None（纯追加可选字段）。
        let decoded: EndpointClientHello = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/endpoint-hello-v1.json"
        )))
        .unwrap();
        assert_eq!(decoded.ssh_auth_sock, None);

        let mut hello = hello();
        assert_eq!(hello.ssh_auth_sock, None);
        // None 不序列化出键，保持线上 hello 载荷逐字节不变。
        let text = serde_json::to_string(&hello).unwrap();
        assert!(!text.contains("ssh_auth_sock"));

        hello.ssh_auth_sock = Some("/run/user/1000/wezterm/agent.123".to_owned());
        let decoded: EndpointClientHello =
            serde_json::from_str(&serde_json::to_string(&hello).unwrap()).unwrap();
        assert_eq!(decoded, hello);
    }

    #[test]
    fn terminal_cwd_control_is_a_named_optional_frame() {
        let message = terminal_cwd_message(Some("file://host/tmp%20x".to_owned())).unwrap();
        let ServerMessage::EndpointControl { kind, data } = message else {
            panic!("terminal cwd should use endpoint control");
        };
        assert_eq!(kind, TERMINAL_CWD_KIND);
        let decoded: EndpointTerminalCwd = serde_json::from_str(&data).unwrap();
        assert_eq!(decoded.uri.as_deref(), Some("file://host/tmp%20x"));

        let cleared = terminal_cwd_message(None).unwrap();
        let ServerMessage::EndpointControl { data, .. } = cleared else {
            panic!("terminal cwd should use endpoint control");
        };
        let decoded: EndpointTerminalCwd = serde_json::from_str(&data).unwrap();
        assert_eq!(decoded.uri, None);
    }

    #[test]
    fn frozen_generation_one_handshake_decodes() {
        let hello: EndpointClientHello = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/endpoint-hello-v1.json"
        )))
        .unwrap();
        assert!(hello.supports_required_codecs());

        let welcome: EndpointServerWelcome = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/endpoint-welcome-v1.json"
        )))
        .unwrap();
        assert_eq!(welcome.generation, ENDPOINT_PROTOCOL_GENERATION);
        assert_eq!(welcome.snapshot_codec, SNAPSHOT_CODEC_V1);
        assert_eq!(welcome.surface_codec, SURFACE_CODEC_V1);
        assert_eq!(welcome.input_codec, INPUT_CODEC_V1);
        assert_eq!(welcome.blob_codec, BLOB_CODEC_V1);
        assert!(welcome.capabilities.is_empty());
    }

    #[test]
    fn frozen_generation_one_snapshot_decodes() {
        let snapshot: ClientShellSnapshot = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/endpoint-snapshot-v1.json"
        )))
        .unwrap();
        assert_eq!(snapshot.boot_id, "boot-v1");
        assert_eq!(
            snapshot.workspaces[0].agent_status,
            crate::api::schema::AgentStatus::Unknown
        );
    }

    #[test]
    fn snapshot_message_uses_named_json_control() {
        let snapshot = snapshot();
        let ServerMessage::EndpointControl { kind, data } = snapshot_message(&snapshot).unwrap()
        else {
            panic!("snapshot should use endpoint control");
        };
        assert_eq!(kind, ENDPOINT_SNAPSHOT_KIND);
        let decoded: ClientShellSnapshot = serde_json::from_str(&data).unwrap();
        assert_eq!(decoded, snapshot);
    }

    #[test]
    fn agent_view_projection_is_an_optional_revision_bound_control() {
        let view = crate::api::schema::AgentViewSetParams {
            source: "example.views".into(),
            label: Some("focus".into()),
            filter: None,
            sort: Vec::new(),
        };
        let ServerMessage::EndpointControl { kind, data } =
            agent_view_projection_message("boot", 7, Some(&view)).unwrap()
        else {
            panic!("projection should use endpoint control");
        };
        assert_eq!(kind, AGENT_VIEW_PROJECTION_KIND);
        let projection: EndpointAgentViewProjection = serde_json::from_str(&data).unwrap();
        assert_eq!(projection.boot_id, "boot");
        assert_eq!(projection.revision, 7);
        assert_eq!(
            projection
                .view
                .map(serde_json::from_value)
                .transpose()
                .unwrap(),
            Some(view)
        );
    }

    #[test]
    fn snapshot_json_tolerates_future_fields_and_command_actions() {
        let mut snapshot = match snapshot_message(&snapshot()).unwrap() {
            ServerMessage::EndpointControl { data, .. } => {
                serde_json::from_str::<serde_json::Value>(&data).unwrap()
            }
            _ => unreachable!(),
        };
        snapshot["future_projection"] = serde_json::json!({"enabled": true});
        snapshot["commands"] = serde_json::json!([{
            "command_id": "future",
            "binding_label": "x",
            "binding_labels": ["x"],
            "action": "FutureAction",
            "description": null
        }]);

        let decoded: ClientShellSnapshot = serde_json::from_value(snapshot).unwrap();
        assert_eq!(
            decoded.commands[0].action,
            crate::protocol::ClientShellCommandAction::Unknown
        );
    }

    /// 旧 server 的快照没有活动树 / 外部来源 / 启动序号：缺字段取默认值。冻结
    /// fixture 里的 agent 同样没有这些字段。
    #[test]
    fn snapshot_json_without_activity_fields_decodes_with_defaults() {
        let frozen: ClientShellSnapshot = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/endpoint-snapshot-v1.json"
        )))
        .unwrap();
        assert!(frozen.external_agents.is_empty());
        assert!(!frozen.agents.is_empty(), "夹具前提：冻结快照里有 agent");
        for agent in &frozen.agents {
            assert_eq!(agent.launch_seq, 0, "旧 server 不下发启动序号");
            assert_eq!(
                agent.activity,
                crate::protocol::ClientShellAgentActivity::default()
            );
        }

        // 当前编码去掉新字段后仍可解码（新 client ↔ 旧 server）。
        let mut value = serde_json::to_value(snapshot()).unwrap();
        value.as_object_mut().unwrap().remove("external_agents");
        let decoded: ClientShellSnapshot = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, snapshot());
    }

    /// 新 server → 旧 client 的方向：agent / 活动节点 / 外部条目里出现未知字段与
    /// 未知枚举值都不让整份快照解码失败。
    #[test]
    fn snapshot_json_tolerates_future_agent_and_activity_fields() {
        let mut value: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/endpoint-snapshot-v1.json"
        )))
        .unwrap();
        value["agents"][0]["future_agent_field"] = serde_json::json!({"nested": [1, 2, 3]});
        value["agents"][0]["launch_seq"] = serde_json::json!(7);
        value["agents"][0]["activity"] = serde_json::json!({
            "running": 1,
            "total": 2,
            "future_summary": "x",
            "nodes": [
                {
                    "id": "n1",
                    "kind": "subagent",
                    "label": "explore",
                    "status": "running",
                    "future_node_field": true
                },
                {"id": "n2", "kind": "future_kind", "status": "future_status"}
            ]
        });
        value["external_agents"] = serde_json::json!([{
            "external_id": "zcode:abc",
            "source": "zcode",
            "agent_status": "future_status",
            "future_external_field": 1
        }]);

        let decoded: ClientShellSnapshot = serde_json::from_value(value).unwrap();
        let agent = &decoded.agents[0];
        assert_eq!(agent.launch_seq, 7);
        assert_eq!((agent.activity.running, agent.activity.total), (1, 2));
        assert!(!agent.activity.truncated);
        assert_eq!(
            agent.activity.nodes[0].kind,
            crate::api::schema::AgentActivityKind::Subagent
        );
        assert_eq!(
            agent.activity.nodes[1].kind,
            crate::api::schema::AgentActivityKind::Unknown
        );
        assert_eq!(
            agent.activity.nodes[1].status,
            crate::api::schema::AgentActivityStatus::Unknown
        );
        let external = &decoded.external_agents[0];
        assert_eq!(external.external_id, "zcode:abc");
        assert_eq!(
            external.agent_status,
            crate::api::schema::AgentStatus::Unknown
        );
        assert!(external.readable, "缺省为可读");
        assert!(external.label.is_empty());
    }

    #[test]
    fn legacy_hello_defaults_to_an_active_surface() {
        let mut value = serde_json::to_value(hello()).unwrap();
        value.as_object_mut().unwrap().remove("surface_active");
        value.as_object_mut().unwrap().remove("surface_reuse");
        let decoded: EndpointClientHello = serde_json::from_value(value).unwrap();
        assert!(decoded.surface_active);
        assert!(!decoded.surface_reuse);
    }

    #[test]
    fn compatible_server_advertises_endpoint_lifecycle_capabilities() {
        let welcome = EndpointServerWelcome::compatible(Vec::new());
        assert_eq!(
            welcome.capabilities,
            vec![
                super::super::views::CAPABILITY.to_string(),
                super::super::surface_reuse::CAPABILITY.to_string(),
                SURFACE_INTEREST_CAPABILITY.to_string(),
                PRESENTATION_EFFECTS_FENCE_CAPABILITY.to_string(),
                HEALTH_CHECK_CAPABILITY.to_string(),
                AGENT_VIEW_PROJECTION_CAPABILITY.to_string(),
            ]
        );
    }

    #[test]
    fn required_codecs_are_explicit() {
        let mut value = hello();
        assert!(value.supports_required_codecs());
        value.snapshot_codecs.clear();
        assert!(!value.supports_required_codecs());

        let mut value = hello();
        value.surface_codecs.clear();
        assert!(!value.supports_required_codecs());

        let mut value = hello();
        value.input_codecs.clear();
        assert!(!value.supports_required_codecs());

        let mut value = hello();
        value.blob_codecs.clear();
        assert!(!value.supports_required_codecs());
    }

    #[test]
    fn welcome_ignores_future_named_fields() {
        let welcome = EndpointServerWelcome::compatible(vec!["pane.close".into()]);
        let mut value = serde_json::to_value(&welcome).unwrap();
        value["future_service"] = serde_json::json!("v2");
        let decoded: EndpointServerWelcome = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, welcome);
    }
}
