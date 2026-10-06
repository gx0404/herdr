use std::io;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    mpsc, Arc, Mutex,
};

use tokio::sync::{mpsc as tokio_mpsc, OwnedSemaphorePermit, Semaphore};

use crate::api::schema::{ErrorBody, ErrorResponse, Method};

use super::client_transport::ServerEvent;

pub(crate) const MAX_ENDPOINT_COMMAND_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_ENDPOINT_BOOT_ID_BYTES: usize = 128;
pub(crate) const MAX_ENDPOINT_REQUEST_ID_BYTES: usize = 128;
pub(crate) const ENDPOINT_RESPONSE_CHUNK_BYTES: usize = 512 * 1024;
pub(crate) const MAX_ENDPOINT_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const MAX_CLIENT_RESPONSES: usize = 3;
pub(crate) const MAX_SERVER_RESPONSES: usize = 6;
pub(crate) const ENDPOINT_BLOCKS_PER_TURN: usize = 4;
const MAX_CONTROL_RESPONSE_BYTES: usize = crate::protocol::MAX_FRAME_SIZE - 1024;

#[derive(Debug)]
pub(crate) struct EndpointResponseIdentity {
    pub(crate) client_id: u64,
    pub(crate) boot_id: String,
    pub(crate) request_id: String,
    pub(crate) order: u64,
    pub(crate) deferred_request_id: std::sync::OnceLock<String>,
    active: AtomicBool,
    tickets: AtomicUsize,
}

impl EndpointResponseIdentity {
    pub(crate) fn new(client_id: u64, boot_id: String, request_id: String) -> Arc<Self> {
        static NEXT_ORDER: AtomicU64 = AtomicU64::new(1);
        let order = NEXT_ORDER.fetch_add(1, Ordering::Relaxed);
        Arc::new(Self {
            client_id,
            boot_id,
            request_id,
            order,
            deferred_request_id: std::sync::OnceLock::new(),
            active: AtomicBool::new(true),
            tickets: AtomicUsize::new(0),
        })
    }

    pub(crate) fn live(&self) -> bool {
        self.tickets.load(Ordering::Acquire) != 0
    }

    pub(crate) fn active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    pub(crate) fn cancel(&self) {
        self.active.store(false, Ordering::Release);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EndpointResponseKind {
    Command {
        surface_revision: u64,
        navigate: bool,
    },
    Reading,
    Background,
}

#[derive(Debug)]
pub(crate) struct EndpointResponseTicket {
    pub(crate) identity: Arc<EndpointResponseIdentity>,
    _global: OwnedSemaphorePermit,
    _client: OwnedSemaphorePermit,
}

impl EndpointResponseTicket {
    pub(crate) fn acquire(
        global: &Arc<Semaphore>,
        client: &Arc<Semaphore>,
        identity: Arc<EndpointResponseIdentity>,
    ) -> Option<Self> {
        let global = global.clone().try_acquire_owned().ok()?;
        let client = client.clone().try_acquire_owned().ok()?;
        identity.tickets.fetch_add(1, Ordering::AcqRel);
        Some(Self {
            identity,
            _global: global,
            _client: client,
        })
    }
}

impl Drop for EndpointResponseTicket {
    fn drop(&mut self) {
        if self.identity.tickets.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.identity.cancel();
        }
    }
}

pub(crate) struct EndpointResponseReady {
    // Field order frees the body before returning its permits, including a dropped send future.
    pub(crate) body: Box<[u8]>,
    pub(crate) ticket: EndpointResponseTicket,
}

impl std::fmt::Debug for EndpointResponseReady {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EndpointResponseReady")
            .field("bytes", &self.body.len())
            .field("identity", &self.ticket.identity)
            .finish()
    }
}

impl EndpointResponseReady {
    pub(crate) fn new(ticket: EndpointResponseTicket, response: String) -> Self {
        let response = correlate_response_id(response, &ticket.identity.request_id);
        let response = if response.len() > MAX_ENDPOINT_RESPONSE_BYTES {
            drop(response);
            error_response(
                ticket.identity.request_id.clone(),
                "endpoint_response_too_large",
                "endpoint response exceeds 64 MiB; the operation may already have completed",
            )
        } else {
            response
        };
        Self {
            body: response.into_bytes().into_boxed_slice(),
            ticket,
        }
    }
}

#[derive(Debug)]
pub(crate) struct EndpointControlResponse {
    pub(crate) identity: Arc<EndpointResponseIdentity>,
    pub(crate) writer: super::client_transport::ClientControlWriter,
}

impl Drop for EndpointControlResponse {
    fn drop(&mut self) {
        self.identity.cancel();
    }
}

#[derive(Debug)]
pub(crate) enum EndpointResponseTarget {
    Bulk(EndpointResponseTicket),
    Control(EndpointControlResponse),
}

impl EndpointResponseTarget {
    fn active(&self) -> bool {
        match self {
            Self::Bulk(ticket) => ticket.identity.active(),
            Self::Control(control) => control.identity.active(),
        }
    }

    pub(crate) fn send(self, response: String, events: &tokio_mpsc::Sender<ServerEvent>) {
        if !self.active() {
            return;
        }
        let ready = match self {
            Self::Bulk(ticket) => EndpointResponseReady::new(ticket, response),
            Self::Control(control) => {
                send_control_response(
                    &control.writer,
                    &control.identity.boot_id,
                    &control.identity.request_id,
                    response,
                );
                return;
            }
        };
        let event = ServerEvent::EndpointResponseReady { response: ready };
        if let Err(tokio_mpsc::error::TrySendError::Full(event)) = events.try_send(event) {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let events = events.clone();
                runtime.spawn(async move {
                    let _ = events.send(event).await;
                });
            } else {
                let _ = events.blocking_send(event);
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct EndpointResponseSender(Arc<Mutex<Option<EndpointResponseTarget>>>);

#[cfg(test)]
pub(crate) fn test_response_ticket(
    client_id: u64,
    boot_id: &str,
    request_id: &str,
) -> EndpointResponseTicket {
    EndpointResponseTicket::acquire(
        &Arc::new(Semaphore::new(MAX_SERVER_RESPONSES)),
        &Arc::new(Semaphore::new(MAX_CLIENT_RESPONSES)),
        EndpointResponseIdentity::new(client_id, boot_id.into(), request_id.into()),
    )
    .unwrap()
}

impl EndpointResponseSender {
    #[cfg(test)]
    pub(crate) fn test_empty() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }

    pub(crate) fn new(target: EndpointResponseTarget) -> Self {
        Self(Arc::new(Mutex::new(Some(target))))
    }

    pub(crate) fn take(&self) -> Option<EndpointResponseTarget> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .filter(EndpointResponseTarget::active)
    }
}

pub(crate) fn send_control_response(
    writer: &super::client_transport::ClientControlWriter,
    boot_id: &str,
    request_id: &str,
    response: String,
) {
    let response = correlate_response_id(response, request_id);
    let response = if response.len() > MAX_CONTROL_RESPONSE_BYTES {
        drop(response);
        error_response(request_id.into(), "endpoint_response_too_large",
            "control acknowledgement exceeds its bounded control-frame limit; the operation may already have completed")
    } else {
        response
    };
    let message = crate::protocol::ServerMessage::ClientShellEndpointResponseChunk {
        boot_id: boot_id.into(),
        request_id: request_id.into(),
        final_chunk: true,
        data: response.into_bytes(),
    };
    let mut frame = Vec::new();
    if crate::protocol::write_message(&mut frame, &message).is_ok() {
        let _ = writer.send(frame);
    }
}

pub(crate) fn response_text(
    request_id: &str,
    result: Result<crate::api::schema::ResponseResult, (&str, String)>,
) -> String {
    match result {
        Ok(result) => serde_json::to_string(&crate::api::schema::SuccessResponse {
            id: request_id.into(),
            result,
        })
        .unwrap_or_else(|error| {
            error_response(request_id.into(), "serialization_error", error.to_string())
        }),
        Err((code, message)) => error_response(request_id.into(), code, message),
    }
}

const CLIENT_SHELL_METHODS: &[&str] = &[
    "account.binding.set",
    "account.usage.get",
    "account.usage.integration",
    "account.usage.providers",
    "account.usage.refresh",
    "account.usage.report",
    "account.usage.subscribe",
    "account.usage.unsubscribe",
    "agent.activity.read",
    "agent.external.list",
    "client.views.set",
    "client_shell.surface.set",
    "command.invoke",
    "integration.install",
    "integration.list",
    "layout.set_split_ratio",
    "pane.clear",
    "pane.close",
    "pane.copy_motion",
    "pane.copy_search",
    "pane.edit_scrollback",
    "pane.focus",
    "pane.focus_direction",
    "pane.input.set",
    "pane.link.activate",
    "pane.link.resolve",
    "pane.move",
    "pane.rename",
    "pane.resize",
    "pane.scroll",
    "pane.selection.read",
    "pane.split",
    "pane.swap",
    "pane.text_snapshot.capture",
    "pane.text_snapshot.read",
    "pane.text_snapshot.release",
    "pane.text_snapshot.retain",
    "pane.text_snapshot.selection",
    "pane.zoom",
    "product_announcement.dismiss",
    "release_notes.dismiss",
    "server.reload_config",
    "system.metrics.get",
    "system.metrics.subscribe",
    "system.metrics.unsubscribe",
    "system.process.get",
    "system.process.list",
    "system.process.terminate",
    "tab.close",
    "tab.create",
    "tab.focus",
    "tab.move",
    "tab.rename",
    "workspace.close",
    "workspace.create",
    "workspace.focus",
    "workspace.move",
    "workspace.move_block",
    "workspace.rename",
    "worktree.create",
    "worktree.list",
    "worktree.open",
    "worktree.remove",
];

pub(crate) fn supported_client_shell_method_names() -> &'static [&'static str] {
    CLIENT_SHELL_METHODS
}

/// HSR-17：白名单查询是每请求路径，改哈希表（原先线性比较 ~70 条）。
/// 顺序列表仍是握手宣告的真源。
fn client_shell_method_set() -> &'static std::collections::HashSet<&'static str> {
    static SET: std::sync::OnceLock<std::collections::HashSet<&'static str>> =
        std::sync::OnceLock::new();
    SET.get_or_init(|| CLIENT_SHELL_METHODS.iter().copied().collect())
}

pub(crate) fn supports_client_shell_method_name(method: &str) -> bool {
    client_shell_method_set().contains(method)
}

pub(crate) fn supports_client_shell_method(method: &Method) -> bool {
    supports_client_shell_method_name(crate::api::api_method_name(method))
}

pub(crate) fn error_response(id: String, code: &str, message: impl Into<String>) -> String {
    serde_json::to_string(&ErrorResponse {
        id,
        error: ErrorBody {
            code: code.into(),
            message: message.into(),
        },
    })
    .unwrap_or_else(|_| {
        r#"{"id":"","error":{"code":"serialization_error","message":"failed to serialize endpoint response"}}"#.into()
    })
}

pub(crate) fn error_message(
    boot_id: String,
    request_id: String,
    code: &str,
    message: impl Into<String>,
) -> crate::protocol::ServerMessage {
    let response = error_response(request_id.clone(), code, message);
    crate::protocol::ServerMessage::ClientShellEndpointResponseChunk {
        boot_id,
        request_id,
        final_chunk: true,
        data: response.into_bytes(),
    }
}

fn correlate_response_id(response: String, request_id: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&response) else {
        return response;
    };
    let Some(id) = value.get_mut("id") else {
        return response;
    };
    if id.as_str() == Some(request_id) {
        return response;
    }
    *id = serde_json::Value::String(request_id.to_owned());
    serde_json::to_string(&value).unwrap_or(response)
}

pub(crate) fn spawn_response_waiter(
    ticket: EndpointResponseTicket,
    response_rx: mpsc::Receiver<String>,
    server_event_tx: tokio_mpsc::Sender<ServerEvent>,
) -> io::Result<()> {
    std::thread::Builder::new()
        .name("herdr-client-endpoint-response".into())
        .spawn(move || {
            let response = response_rx.recv().unwrap_or_else(|_| {
                error_response(
                    ticket.identity.request_id.clone(),
                    "server_unavailable",
                    "endpoint command ended without a response",
                )
            });
            if !ticket.identity.active() {
                drop(response);
                return;
            }
            let response = EndpointResponseReady::new(ticket, response);
            let _ = server_event_tx.blocking_send(ServerEvent::EndpointResponseReady { response });
        })
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use sha2::{Digest, Sha256};

    use super::*;

    fn collect_schema_refs(value: &serde_json::Value, refs: &mut BTreeSet<String>) {
        match value {
            serde_json::Value::Object(object) => {
                if let Some(reference) = object.get("$ref").and_then(serde_json::Value::as_str) {
                    if let Some(name) = reference.rsplit('/').next() {
                        refs.insert(name.to_owned());
                    }
                }
                for value in object.values() {
                    collect_schema_refs(value, refs);
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    collect_schema_refs(value, refs);
                }
            }
            _ => {}
        }
    }

    fn normalized_wire_schema(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(object) => serde_json::Value::Object(
                object
                    .iter()
                    .filter(|(key, _)| {
                        !matches!(
                            key.as_str(),
                            "description" | "examples" | "readOnly" | "title" | "writeOnly"
                        )
                    })
                    .map(|(key, value)| (key.clone(), normalized_wire_schema(value)))
                    .collect(),
            ),
            serde_json::Value::Array(values) => {
                serde_json::Value::Array(values.iter().map(normalized_wire_schema).collect())
            }
            _ => value.clone(),
        }
    }

    fn endpoint_method_shape_digests() -> BTreeMap<String, String> {
        method_shape_digests(CLIENT_SHELL_METHODS)
    }

    fn method_shape_digests(methods: &[&str]) -> BTreeMap<String, String> {
        let schema = serde_json::to_value(schemars::schema_for!(crate::api::schema::Request))
            .expect("request schema");
        let definitions = schema
            .get("$defs")
            .and_then(serde_json::Value::as_object)
            .expect("request definitions");
        let branches = schema
            .get("oneOf")
            .and_then(serde_json::Value::as_array)
            .expect("request method branches");
        let mut digests = BTreeMap::new();

        for method in methods {
            let branch = branches
                .iter()
                .find(|branch| {
                    branch
                        .pointer("/properties/method/const")
                        .and_then(serde_json::Value::as_str)
                        == Some(method)
                })
                .unwrap_or_else(|| panic!("missing request schema branch for {method}"));
            let mut referenced_names = BTreeSet::new();
            collect_schema_refs(branch, &mut referenced_names);
            let mut visited_names = BTreeSet::new();
            let mut selected_definitions = serde_json::Map::new();
            while let Some(name) = referenced_names.pop_first() {
                if !visited_names.insert(name.clone()) {
                    continue;
                }
                let definition = definitions
                    .get(&name)
                    .unwrap_or_else(|| panic!("missing schema definition {name} for {method}"));
                collect_schema_refs(definition, &mut referenced_names);
                selected_definitions.insert(name, normalized_wire_schema(definition));
            }
            let shape = serde_json::json!({
                "request": normalized_wire_schema(branch),
                "definitions": selected_definitions,
            });
            let bytes = serde_json::to_vec(&shape).expect("method shape json");
            digests.insert(method.to_string(), format!("{:x}", Sha256::digest(bytes)));
        }

        digests
    }

    #[test]
    fn advertised_client_shell_method_shapes_stay_at_the_v1_contract() {
        let expected: BTreeMap<String, String> = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/endpoint-method-shapes-v1.json"
        )))
        .expect("endpoint method shape fixture");
        let mut actual = endpoint_method_shape_digests();
        let snapshot_names = actual
            .keys()
            .filter(|name| name.starts_with("pane.text_snapshot."))
            .cloned()
            .collect::<Vec<_>>();
        let snapshot_shapes = snapshot_names
            .into_iter()
            .map(|name| {
                let digest = actual.remove(&name).expect("快照方法存在");
                (name, digest)
            })
            .collect::<BTreeMap<_, _>>();
        let expected_snapshots: BTreeMap<String, String> =
            serde_json::from_str(include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/endpoint-text-snapshot-shapes-v1.json"
            )))
            .expect("独立冻结的文字快照契约");
        assert_eq!(snapshot_shapes, expected_snapshots);
        // 活动树方法：新增能力，独立摘出比对自己的 fixture，不进任何既有 fixture。
        let activity_names = actual
            .keys()
            .filter(|name| name.starts_with("agent.activity.") || *name == "agent.external.list")
            .cloned()
            .collect::<Vec<_>>();
        let activity_shapes = activity_names
            .into_iter()
            .map(|name| {
                let digest = actual.remove(&name).expect("活动树方法存在");
                (name, digest)
            })
            .collect::<BTreeMap<_, _>>();
        let expected_activity: BTreeMap<String, String> =
            serde_json::from_str(include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/endpoint-agent-activity-shapes-v1.json"
            )))
            .expect("活动树方法的独立契约");
        assert_eq!(activity_shapes, expected_activity);
        // Freeze additive methods separately without rewriting the published fixture.
        assert_eq!(
            actual.remove("pane.clear").as_deref(),
            Some("0301d288ba198ddaa427dd7421c71911cccaf4ea03544531efa8b67ca21b08f6")
        );
        assert_eq!(
            actual.remove("pane.link.resolve").as_deref(),
            Some("f5e4a3e01453ae7b188f127ce951c12c20e0bebcc17cc364eeb6d1a01fd5bf81")
        );
        let additive = actual
            .keys()
            .filter(|name| {
                name.starts_with("system.")
                    || name.starts_with("account.")
                    || name.as_str() == "client.views.set"
                    || name.as_str() == "pane.move"
            })
            .cloned()
            .collect::<Vec<_>>();
        let added = additive
            .into_iter()
            .filter_map(|name| actual.remove(&name).map(|digest| (name, digest)))
            .collect::<BTreeMap<_, _>>();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/endpoint-observability-shapes-v1.json");
        let expected_additions: BTreeMap<String, String> =
            serde_json::from_str(&std::fs::read_to_string(path).expect("新增能力独立契约"))
                .unwrap();
        assert_eq!(
            added, expected_additions,
            "新增接口也独立冻结，不能影响已发布 v1 fixture"
        );

        assert_eq!(
            actual, expected,
            "an existing endpoint method changed shape; add load-bearing behavior as a new advertised method or explicitly gate new fields"
        );
    }

    /// 活动树方法的请求形状钉在自己的 fixture 里：它们已进 `CLIENT_SHELL_METHODS`，
    /// `advertised_client_shell_method_shapes_stay_at_the_v1_contract` 用独立摘出块
    /// 比对同一份 fixture；这里再确认 fixture 恰好只含这两个方法且都已宣告。不得往
    /// 既有 fixture 加键。
    #[test]
    fn agent_activity_method_shapes_are_pinned_in_their_own_fixture() {
        let expected: BTreeMap<String, String> = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/endpoint-agent-activity-shapes-v1.json"
        )))
        .expect("活动树方法的独立契约");
        let methods = expected.keys().map(String::as_str).collect::<Vec<_>>();
        assert_eq!(methods, ["agent.activity.read", "agent.external.list"]);
        assert_eq!(method_shape_digests(&methods), expected);
        for method in methods {
            assert!(supports_client_shell_method_name(method), "{method} 已宣告");
        }
    }

    #[test]
    fn advertised_client_shell_methods_are_sorted_unique_and_in_schema() {
        assert!(CLIENT_SHELL_METHODS
            .windows(2)
            .all(|pair| pair[0] < pair[1]));

        fn collect_method_constants(value: &serde_json::Value, methods: &mut Vec<String>) {
            match value {
                serde_json::Value::Object(object) => {
                    if let Some(method) = object
                        .get("const")
                        .and_then(serde_json::Value::as_str)
                        .filter(|value| value.contains('.'))
                    {
                        methods.push(method.to_owned());
                    }
                    for value in object.values() {
                        collect_method_constants(value, methods);
                    }
                }
                serde_json::Value::Array(values) => {
                    for value in values {
                        collect_method_constants(value, methods);
                    }
                }
                _ => {}
            }
        }

        let schema = serde_json::to_value(schemars::schema_for!(crate::api::schema::Request))
            .expect("request schema");
        let mut schema_methods = Vec::new();
        collect_method_constants(&schema, &mut schema_methods);
        for method in CLIENT_SHELL_METHODS {
            assert!(
                schema_methods.iter().any(|candidate| candidate == method),
                "advertised endpoint method {method:?} is absent from the request schema"
            );
        }
    }

    #[test]
    fn client_shell_lane_excludes_api_front_door_and_lifecycle_methods() {
        assert!(supports_client_shell_method(
            &Method::ClientShellSurfaceSet(crate::api::schema::ClientShellSurfaceSetParams {
                active: false,
            })
        ));
        assert!(supports_client_shell_method(&Method::ServerReloadConfig(
            crate::api::schema::EmptyParams::default(),
        )));
        assert!(supports_client_shell_method(&Method::PaneLinkActivate(
            crate::api::schema::PaneLinkActivateParams {
                pane_id: "w1:p1".into(),
                viewport_row: 0,
                col: 0,
                content_revision: None,
                offset_from_bottom: None,
            },
        )));
        assert!(supports_client_shell_method(&Method::PaneLinkResolve(
            crate::api::schema::PaneLinkActivateParams {
                pane_id: "w1:p1".into(),
                viewport_row: 0,
                col: 0,
                content_revision: None,
                offset_from_bottom: None,
            },
        )));
        assert!(!supports_client_shell_method(&Method::Ping(
            crate::api::schema::PingParams::default(),
        )));
        assert!(!supports_client_shell_method(&Method::ServerStop(
            crate::api::schema::EmptyParams::default(),
        )));
    }

    #[tokio::test]
    async fn response_matrix_waiting_event_future_keeps_64mib_permit_after_cancel() {
        let global = Arc::new(Semaphore::new(MAX_SERVER_RESPONSES));
        let client = Arc::new(Semaphore::new(MAX_CLIENT_RESPONSES));
        let identity = EndpointResponseIdentity::new(7, "boot".into(), "held".into());
        let ticket = EndpointResponseTicket::acquire(&global, &client, identity.clone()).unwrap();
        let (events, mut receiver) = tokio_mpsc::channel(1);
        events.try_send(ServerEvent::QuitSignal).unwrap();
        let reply = crate::server::observability::Reply::Endpoint {
            client_id: 7,
            boot_id: "boot".into(),
            events,
            active: Arc::new(AtomicBool::new(true)),
            response: EndpointResponseSender::new(EndpointResponseTarget::Bulk(ticket)),
        };
        let subscription = reply.clone();
        let overhead = response_text(
            "held",
            Ok(crate::api::schema::ResponseResult::PaneLinkActivated {
                url: Some(String::new()),
                handled: true,
            }),
        )
        .len();
        reply.response(
            "held",
            Ok(crate::api::schema::ResponseResult::PaneLinkActivated {
                url: Some("x".repeat(MAX_ENDPOINT_RESPONSE_BYTES - overhead)),
                handled: true,
            }),
        );
        subscription.response(
            "held",
            Err(("duplicate", "must not create a second response".into())),
        );
        identity.cancel();
        tokio::task::yield_now().await;
        assert_eq!(receiver.len(), 1);
        assert_eq!(
            global.available_permits(),
            5,
            "cancel must not refund a body retained by the send future"
        );
        assert_eq!(client.available_permits(), 2);
        assert!(matches!(
            receiver.recv().await,
            Some(ServerEvent::QuitSignal)
        ));
        let event = tokio::time::timeout(std::time::Duration::from_secs(30), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let ServerEvent::EndpointResponseReady { response } = &event else {
            panic!("complete response");
        };
        assert_eq!(response.body.len(), 64 * 1024 * 1024);
        assert_eq!(global.available_permits(), 5);
        drop(event);
        assert_eq!(global.available_permits(), 6);
        assert_eq!(client.available_permits(), 3);
        subscription.response("held", Ok(crate::api::schema::ResponseResult::Ok {}));
        tokio::task::yield_now().await;
        assert!(receiver.try_recv().is_err());
        println!("waiting_future body_bytes=67108864 permits_while_cancelled=5 permits_after_body_drop=6 client_after=3 subscription_clone_alive=true");
    }

    #[test]
    fn response_matrix_small_control_response_has_an_enforced_byte_boundary() {
        let writer = crate::server::client_transport::ClientWriter::test_paused();
        let identity = EndpointResponseIdentity::new(1, "boot".into(), "control".into());
        let target = EndpointResponseTarget::Control(EndpointControlResponse {
            identity: identity.clone(),
            writer: writer.control.clone(),
        });
        let (events, mut receiver) = tokio_mpsc::channel(1);
        events.try_send(ServerEvent::QuitSignal).unwrap();
        let text = response_text(
            "control",
            Ok(crate::api::schema::ResponseResult::PaneLinkActivated {
                url: Some("x".repeat(MAX_CONTROL_RESPONSE_BYTES + 1)),
                handled: true,
            }),
        );
        target.send(text, &events);
        assert!(!identity.active());
        assert_eq!(
            receiver.len(),
            1,
            "small-control delivery does not queue an event or a waiting future"
        );
        let frames = writer.test_drain();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].len() <= crate::protocol::MAX_FRAME_SIZE + 4);
        let message: crate::protocol::ServerMessage = crate::protocol::read_message(
            &mut frames[0].as_slice(),
            crate::protocol::MAX_FRAME_SIZE,
        )
        .unwrap();
        let crate::protocol::ServerMessage::ClientShellEndpointResponseChunk {
            boot_id,
            request_id,
            final_chunk,
            data,
        } = message
        else {
            panic!("control final");
        };
        assert_eq!(
            (boot_id.as_str(), request_id.as_str(), final_chunk),
            ("boot", "control", true)
        );
        let error: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(error["error"]["code"], "endpoint_response_too_large");
        assert!(error.get("result").is_none());
        assert!(!writer.was_aborted());
        assert!(matches!(receiver.try_recv(), Ok(ServerEvent::QuitSignal)));
    }

    #[test]
    fn endpoint_response_uses_the_client_request_id() {
        let response = serde_json::json!({
            "id": "endpoint:boot-a:7:client-shell:1",
            "result": { "type": "ok" }
        })
        .to_string();

        let correlated = correlate_response_id(response, "client-shell:1");
        let decoded: serde_json::Value = serde_json::from_str(&correlated).expect("response json");

        assert_eq!(decoded["id"], "client-shell:1");
    }

    #[test]
    fn endpoint_responses_are_handed_off_once_without_truncation() {
        let (response_tx, response_rx) = mpsc::channel();
        let (event_tx, mut event_rx) = tokio_mpsc::channel(8);
        spawn_response_waiter(
            test_response_ticket(7, "boot-a", "request-a"),
            response_rx,
            event_tx,
        )
        .unwrap();
        let text = "x".repeat(ENDPOINT_RESPONSE_CHUNK_BYTES + 17);
        response_tx.send(text.clone()).unwrap();
        let ServerEvent::EndpointResponseReady { response } =
            event_rx.blocking_recv().expect("complete response")
        else {
            panic!("expected complete response");
        };
        assert_eq!(response.ticket.identity.client_id, 7);
        assert_eq!(response.ticket.identity.boot_id, "boot-a");
        assert_eq!(response.ticket.identity.request_id, "request-a");
        assert_eq!(response.body.as_ref(), text.as_bytes());
        assert!(event_rx.try_recv().is_err());
    }
}
