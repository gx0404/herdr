use std::collections::{HashMap, VecDeque};
use std::io;
use std::time::{Duration, Instant};

use crate::api::client::ApiClientError;
use crate::api::schema::{Request, ResponseResult};
use crate::protocol::ClientMessage;

use super::endpoint::{ClientEndpointId, EndpointRegistry, EndpointSendOutcome};
use super::shell::ClientShellEndpointError;

const ENDPOINT_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_RETIRED_REQUESTS_PER_ENDPOINT: usize = 128;

struct QueuedCommand {
    generation: u64,
    boot_id: String,
    request: Box<Request>,
    /// 用户连续手势发起，允许被队尾更新的同类手势折叠（见 [`EndpointCommands::enqueue`]）。
    coalesce: bool,
}

struct InFlightCommand {
    generation: u64,
    boot_id: String,
    request_id: String,
    response: Vec<u8>,
    sent_at: Instant,
}

pub(super) struct EndpointCommandResult {
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) generation: u64,
    pub(super) boot_id: String,
    pub(super) request_id: String,
    pub(super) result: Result<ResponseResult, ClientShellEndpointError>,
}

#[derive(Default)]
struct EndpointCommandLane {
    queued: VecDeque<QueuedCommand>,
    in_flight: Option<InFlightCommand>,
    retired: VecDeque<(u64, String, String)>,
}

impl EndpointCommandLane {
    fn retire(&mut self, request: (u64, String, String)) {
        if self.retired.contains(&request) {
            return;
        }
        if self.retired.len() == MAX_RETIRED_REQUESTS_PER_ENDPOINT {
            self.retired.pop_front();
        }
        self.retired.push_back(request);
    }

    fn consume_retired(&mut self, request: &(u64, String, String), final_chunk: bool) -> bool {
        let Some(index) = self.retired.iter().position(|retired| retired == request) else {
            return false;
        };
        if final_chunk {
            self.retired.remove(index);
        }
        true
    }
}

#[derive(Default)]
pub(super) struct EndpointCommands {
    lanes: HashMap<ClientEndpointId, EndpointCommandLane>,
    background: HashMap<ClientEndpointId, EndpointCommandLane>,
    reading: HashMap<ClientEndpointId, EndpointCommandLane>,
}

impl EndpointCommands {
    /// 入队一个端点请求。返回被折叠掉的请求 id，调用方必须对这些 id 做等价清理
    /// （`ClientShellState::supersede_endpoint_request`），否则响应配对项悬空。
    ///
    /// 折叠规则（顺序保证）：只有 `coalesce == true`（用户连续手势）且方法属于
    /// [`folds_queued_predecessors`] 的请求参与折叠；折叠范围只到 lane **队尾连续**的
    /// 同类可折叠请求——从队尾向前遇到第一个不满足条件的请求就停，因此夹在中间的
    /// 其它命令与它们之前的聚焦保持原有相对顺序（`[TabFocus(A), X]` 再入队
    /// `TabFocus(B)` 得到 `[TabFocus(A), X, TabFocus(B)]`，X 仍在 A 聚焦后执行）。
    /// 程序发起（`coalesce == false`）的聚焦既不折叠别人也不会被折叠。已在途的请求
    /// 不受影响。
    #[must_use = "被折叠的请求 id 必须交给 shell 清理配对项"]
    pub(super) fn enqueue(
        &mut self,
        endpoint_id: ClientEndpointId,
        generation: u64,
        boot_id: String,
        request: Box<Request>,
        coalesce: bool,
    ) -> Vec<String> {
        let name = crate::api::api_method_name(&request.method);
        let lanes = if name.starts_with("pane.text_snapshot.") {
            &mut self.reading
        } else if name.starts_with("system.")
            || name.starts_with("account.")
            || name == "client.views.set"
        {
            &mut self.background
        } else {
            &mut self.lanes
        };
        let lane = lanes.entry(endpoint_id).or_default();
        let mut superseded = Vec::new();
        if coalesce && folds_queued_predecessors(&request.method) {
            while lane.queued.back().is_some_and(|tail| {
                tail.coalesce && folds_queued_predecessors(&tail.request.method)
            }) {
                if let Some(tail) = lane.queued.pop_back() {
                    superseded.push(tail.request.id);
                }
            }
            // 队尾向前弹出得到的是倒序；按入队顺序交给调用方。
            superseded.reverse();
        }
        lane.queued.push_back(QueuedCommand {
            generation,
            boot_id,
            request,
            coalesce,
        });
        superseded
    }

    pub(super) fn send_next(
        &mut self,
        endpoint_id: &ClientEndpointId,
        endpoints: &mut EndpointRegistry,
    ) -> Vec<String> {
        let mut cancelled = Self::send_lane(&mut self.lanes, endpoint_id, endpoints);
        cancelled.extend(Self::send_lane(
            &mut self.background,
            endpoint_id,
            endpoints,
        ));
        cancelled.extend(Self::send_lane(&mut self.reading, endpoint_id, endpoints));
        cancelled
    }

    fn send_lane(
        lanes: &mut HashMap<ClientEndpointId, EndpointCommandLane>,
        endpoint_id: &ClientEndpointId,
        endpoints: &mut EndpointRegistry,
    ) -> Vec<String> {
        let lane = lanes.entry(endpoint_id.clone()).or_default();
        let mut cancelled = Vec::new();
        if lane.in_flight.is_some() {
            return cancelled;
        }
        while let Some(queued) = lane.queued.pop_front() {
            let request_id = queued.request.id.clone();
            if !endpoints.accepts(endpoint_id, queued.generation) {
                cancelled.push(request_id);
                continue;
            }
            let request = match serde_json::to_string(&queued.request) {
                Ok(request) => request,
                Err(error) => {
                    tracing::warn!(%error, %request_id, "could not encode endpoint request");
                    cancelled.push(request_id);
                    continue;
                }
            };
            let message = ClientMessage::ClientShellEndpointRequest {
                boot_id: queued.boot_id.clone(),
                request,
            };
            if endpoints.send_to(endpoint_id, &message) != EndpointSendOutcome::Sent {
                cancelled.push(request_id);
                continue;
            }
            lane.in_flight = Some(InFlightCommand {
                generation: queued.generation,
                boot_id: queued.boot_id,
                request_id,
                response: Vec::new(),
                sent_at: Instant::now(),
            });
            break;
        }
        cancelled
    }

    pub(super) fn accepts_response(
        &self,
        endpoint_id: &ClientEndpointId,
        response_generation: u64,
        response_boot_id: &str,
        response_request_id: &str,
    ) -> bool {
        [
            self.lanes.get(endpoint_id),
            self.background.get(endpoint_id),
            self.reading.get(endpoint_id),
        ]
        .into_iter()
        .flatten()
        .filter_map(|lane| lane.in_flight.as_ref())
        .any(|command| {
            command.generation == response_generation
                && command.boot_id == response_boot_id
                && command.request_id == response_request_id
        })
    }

    /// Retire the complete source lane at source-off. The in-flight request is tombstoned for a
    /// late endpoint-local response; every queued request is cancelled before it can run in a
    /// later presentation epoch. Other endpoint lanes are deliberately untouched.
    pub(super) fn retire_lane(&mut self, endpoint_id: &ClientEndpointId) -> Vec<String> {
        let mut request_ids = Vec::new();
        for lane in [
            self.lanes.get_mut(endpoint_id),
            self.background.get_mut(endpoint_id),
            self.reading.get_mut(endpoint_id),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(command) = lane.in_flight.take() {
                lane.retire((
                    command.generation,
                    command.boot_id,
                    command.request_id.clone(),
                ));
                request_ids.push(command.request_id);
            }
            request_ids.extend(lane.queued.drain(..).map(|command| command.request.id));
        }
        request_ids
    }

    pub(super) fn expire(&mut self, now: Instant) -> Vec<EndpointCommandResult> {
        self.lanes
            .iter_mut()
            .chain(self.background.iter_mut())
            .chain(self.reading.iter_mut())
            .filter_map(|(endpoint_id, lane)| {
                let expired = lane.in_flight.as_ref().is_some_and(|command| {
                    now.saturating_duration_since(command.sent_at) >= ENDPOINT_COMMAND_TIMEOUT
                });
                if !expired {
                    return None;
                }
                let command = lane.in_flight.take()?;
                lane.retire((
                    command.generation,
                    command.boot_id.clone(),
                    command.request_id.clone(),
                ));
                Some(EndpointCommandResult {
                    endpoint_id: endpoint_id.clone(),
                    generation: command.generation,
                    boot_id: command.boot_id,
                    request_id: command.request_id,
                    result: Err(ClientShellEndpointError {
                        code: Some("endpoint_timeout".into()),
                        message: "this server did not respond to the action".into(),
                    }),
                })
            })
            .collect()
    }

    pub(super) fn receive_chunk(
        &mut self,
        endpoint_id: &ClientEndpointId,
        response_generation: u64,
        response_boot_id: &str,
        response_request_id: &str,
        final_chunk: bool,
        data: Vec<u8>,
    ) -> io::Result<Option<EndpointCommandResult>> {
        let background = self.background.get(endpoint_id).is_some_and(|lane| {
            lane.in_flight
                .as_ref()
                .is_some_and(|command| command.request_id == response_request_id)
                || lane
                    .retired
                    .iter()
                    .any(|(_, _, id)| id == response_request_id)
        });
        let reading = self.reading.get(endpoint_id).is_some_and(|lane| {
            lane.in_flight
                .as_ref()
                .is_some_and(|command| command.request_id == response_request_id)
                || lane
                    .retired
                    .iter()
                    .any(|(_, _, id)| id == response_request_id)
        });
        let lanes = if reading {
            &mut self.reading
        } else if background {
            &mut self.background
        } else {
            &mut self.lanes
        };
        let Some(lane) = lanes.get_mut(endpoint_id) else {
            return Ok(None);
        };
        let retired = (
            response_generation,
            response_boot_id.to_owned(),
            response_request_id.to_owned(),
        );
        if lane.consume_retired(&retired, final_chunk) {
            return Ok(None);
        }
        let Some(in_flight) = lane.in_flight.as_mut() else {
            return Ok(None);
        };
        if response_generation != in_flight.generation
            || response_boot_id != in_flight.boot_id
            || response_request_id != in_flight.request_id
        {
            return Ok(None);
        }
        in_flight.response.extend(data);
        if !final_chunk {
            return Ok(None);
        }

        let Some(in_flight) = lane.in_flight.take() else {
            return Ok(None);
        };
        let result = parse_response(&in_flight.request_id, &in_flight.response);
        Ok(Some(EndpointCommandResult {
            endpoint_id: endpoint_id.clone(),
            generation: in_flight.generation,
            boot_id: in_flight.boot_id,
            request_id: in_flight.request_id,
            result,
        }))
    }

    /// Disconnecting an endpoint also cancels its shell-pending requests. Connection generation
    /// rejection handles any late wire response after the lane itself is removed.
    pub(super) fn disconnect(&mut self, endpoint_id: &ClientEndpointId) -> Vec<String> {
        let mut request_ids = Vec::new();
        for lane in [
            self.lanes.remove(endpoint_id),
            self.background.remove(endpoint_id),
            self.reading.remove(endpoint_id),
        ]
        .into_iter()
        .flatten()
        {
            request_ids.extend(
                lane.queued
                    .into_iter()
                    .map(|command| command.request.id)
                    .collect::<Vec<_>>(),
            );
            if let Some(command) = lane.in_flight {
                request_ids.push(command.request_id);
            }
        }
        request_ids
    }
}

/// 排队时只保留最新目标的方法：后来的目标覆盖先前排队但未写出的同类请求。
/// 只允许无附加簿记、无 `confirmation_workspace_id`、目标显式且幂等的方法
/// （`ClientShellState::supersede_endpoint_request` 对被折叠项只做静默移除）；
/// `tab.close` / `pane.close` 这类带确认兜底的方法不得加入。
fn folds_queued_predecessors(method: &crate::api::schema::Method) -> bool {
    matches!(method, crate::api::schema::Method::TabFocus(_))
}

pub(super) fn parse_response(
    expected_id: &str,
    response: &[u8],
) -> Result<ResponseResult, ClientShellEndpointError> {
    let value = serde_json::from_slice(response).map_err(|error| ClientShellEndpointError {
        code: None,
        message: format!("invalid endpoint response: {error}"),
    })?;
    match crate::api::client::parse_response_value(value) {
        Ok(response) if response.id == expected_id => Ok(response.result),
        Ok(response) => Err(ClientShellEndpointError {
            code: None,
            message: format!(
                "endpoint response id {:?} did not match {expected_id:?}",
                response.id
            ),
        }),
        Err(ApiClientError::ErrorResponse(response)) if response.id == expected_id => {
            Err(ClientShellEndpointError {
                code: Some(response.error.code),
                message: response.error.message,
            })
        }
        Err(ApiClientError::ErrorResponse(response)) => Err(ClientShellEndpointError {
            code: None,
            message: format!(
                "endpoint error id {:?} did not match {expected_id:?}",
                response.id
            ),
        }),
        Err(error) => Err(ClientShellEndpointError {
            code: None,
            message: error.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{ResponseResult, SuccessResponse};

    fn endpoint() -> ClientEndpointId {
        ClientEndpointId::Local
    }

    fn commands_with_in_flight() -> EndpointCommands {
        EndpointCommands {
            background: HashMap::new(),
            reading: HashMap::new(),
            lanes: HashMap::from([(
                endpoint(),
                EndpointCommandLane {
                    in_flight: Some(InFlightCommand {
                        generation: 1,
                        boot_id: "boot-a".into(),
                        request_id: "request-a".into(),
                        response: Vec::new(),
                        sent_at: Instant::now(),
                    }),
                    ..EndpointCommandLane::default()
                },
            )]),
        }
    }

    fn has_in_flight(commands: &EndpointCommands) -> bool {
        commands
            .lanes
            .get(&endpoint())
            .is_some_and(|lane| lane.in_flight.is_some())
    }

    #[test]
    fn chunked_response_completion_is_correlated_and_clears_the_lane() {
        let mut commands = commands_with_in_flight();
        let response = serde_json::to_string(&SuccessResponse {
            id: "request-a".into(),
            result: ResponseResult::Ok {},
        })
        .unwrap();
        let split = response.len() / 2;

        assert!(commands
            .receive_chunk(
                &endpoint(),
                1,
                "boot-a",
                "request-a",
                false,
                response.as_bytes()[..split].to_vec(),
            )
            .unwrap()
            .is_none());
        let completed = commands
            .receive_chunk(
                &endpoint(),
                1,
                "boot-a",
                "request-a",
                true,
                response.as_bytes()[split..].to_vec(),
            )
            .unwrap()
            .unwrap();

        assert_eq!(completed.endpoint_id, endpoint());
        assert_eq!(completed.generation, 1);
        assert_eq!(completed.boot_id, "boot-a");
        assert_eq!(completed.request_id, "request-a");
        assert!(matches!(completed.result, Ok(ResponseResult::Ok {})));
        assert!(!has_in_flight(&commands));
    }

    #[test]
    fn large_selection_response_reassembles_without_truncation() {
        let mut commands = commands_with_in_flight();
        let selection = "selected".repeat(160_000);
        let response = serde_json::to_vec(&SuccessResponse {
            id: "request-a".into(),
            result: ResponseResult::PaneSelection {
                pane_id: "w1:p1".into(),
                text: selection.clone(),
            },
        })
        .unwrap();
        let chunk_count = response.len().div_ceil(128 * 1024);
        let mut completed = None;
        for (index, chunk) in response.chunks(128 * 1024).enumerate() {
            completed = commands
                .receive_chunk(
                    &endpoint(),
                    1,
                    "boot-a",
                    "request-a",
                    index + 1 == chunk_count,
                    chunk.to_vec(),
                )
                .unwrap();
        }

        assert!(matches!(
            completed.expect("final selection response").result,
            Ok(ResponseResult::PaneSelection { text, .. }) if text == selection
        ));
    }

    #[test]
    fn in_flight_endpoint_command_expires_and_releases_the_lane() {
        let mut commands = commands_with_in_flight();
        let expired = commands
            .expire(std::time::Instant::now() + ENDPOINT_COMMAND_TIMEOUT)
            .pop()
            .expect("expired endpoint command");

        assert_eq!(expired.endpoint_id, endpoint());
        assert_eq!(expired.boot_id, "boot-a");
        assert_eq!(expired.request_id, "request-a");
        assert!(matches!(
            expired.result,
            Err(ClientShellEndpointError {
                code: Some(code),
                ..
            }) if code == "endpoint_timeout"
        ));
        assert!(!has_in_flight(&commands));
        assert!(commands
            .expire(std::time::Instant::now() + ENDPOINT_COMMAND_TIMEOUT)
            .is_empty());
        let late_response = serde_json::to_vec(&SuccessResponse {
            id: "request-a".into(),
            result: ResponseResult::Ok {},
        })
        .unwrap();
        assert!(commands
            .receive_chunk(&endpoint(), 1, "boot-a", "request-a", true, late_response)
            .expect("late retired response is ignored")
            .is_none());
        assert!(!has_in_flight(&commands));
    }

    #[test]
    fn endpoint_lanes_complete_independently() {
        let remote = ClientEndpointId::Ssh(
            crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        );
        let mut commands = commands_with_in_flight();
        commands.lanes.insert(
            remote.clone(),
            EndpointCommandLane {
                in_flight: Some(InFlightCommand {
                    generation: 2,
                    boot_id: "boot-b".into(),
                    request_id: "request-b".into(),
                    response: Vec::new(),
                    sent_at: Instant::now(),
                }),
                ..EndpointCommandLane::default()
            },
        );
        let response = serde_json::to_vec(&SuccessResponse {
            id: "request-b".into(),
            result: ResponseResult::Ok {},
        })
        .unwrap();

        let completed = commands
            .receive_chunk(&remote, 2, "boot-b", "request-b", true, response)
            .unwrap()
            .expect("remote response");

        assert_eq!(completed.endpoint_id, remote);
        assert!(has_in_flight(&commands));
        assert!(commands
            .lanes
            .get(&completed.endpoint_id)
            .is_some_and(|lane| lane.in_flight.is_none()));
    }

    #[test]
    fn retiring_complete_source_lane_cancels_queued_ids_and_keeps_other_lanes() {
        let remote = ClientEndpointId::Ssh(
            crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        );
        let mut commands = commands_with_in_flight();
        commands
            .lanes
            .get_mut(&endpoint())
            .unwrap()
            .queued
            .push_back(QueuedCommand {
                generation: 1,
                boot_id: "boot-a".into(),
                request: Box::new(Request {
                    id: "queued-source".into(),
                    method: crate::api::schema::Method::WorkspaceList(
                        crate::api::schema::EmptyParams::default(),
                    ),
                }),
                coalesce: false,
            });
        commands.lanes.insert(
            remote.clone(),
            EndpointCommandLane {
                queued: VecDeque::from([QueuedCommand {
                    generation: 2,
                    boot_id: "boot-b".into(),
                    request: Box::new(Request {
                        id: "request-b".into(),
                        method: crate::api::schema::Method::WorkspaceList(
                            crate::api::schema::EmptyParams::default(),
                        ),
                    }),
                    coalesce: false,
                }]),
                ..EndpointCommandLane::default()
            },
        );

        assert_eq!(
            commands.retire_lane(&endpoint()),
            vec!["request-a", "queued-source"]
        );
        assert!(!has_in_flight(&commands));
        assert!(commands
            .lanes
            .get(&endpoint())
            .is_some_and(|lane| lane.queued.is_empty()));
        assert!(commands
            .lanes
            .get(&remote)
            .is_some_and(|lane| !lane.queued.is_empty()));

        let late_response = serde_json::to_vec(&SuccessResponse {
            id: "request-a".into(),
            result: ResponseResult::Ok {},
        })
        .unwrap();
        assert!(commands
            .receive_chunk(&endpoint(), 1, "boot-a", "request-a", true, late_response)
            .unwrap()
            .is_none());
    }

    #[test]
    fn disconnect_returns_every_request_that_must_be_discarded() {
        let mut commands = commands_with_in_flight();
        commands
            .lanes
            .get_mut(&endpoint())
            .unwrap()
            .queued
            .push_back(QueuedCommand {
                generation: 1,
                boot_id: "boot-a".into(),
                request: Box::new(Request {
                    id: "queued-a".into(),
                    method: crate::api::schema::Method::WorkspaceList(
                        crate::api::schema::EmptyParams::default(),
                    ),
                }),
                coalesce: false,
            });
        assert_eq!(
            commands.disconnect(&endpoint()),
            vec!["queued-a", "request-a"]
        );
        assert!(!commands.lanes.contains_key(&endpoint()));
    }

    #[test]
    fn retired_request_tombstones_are_bounded() {
        let mut lane = EndpointCommandLane::default();
        for serial in 0..MAX_RETIRED_REQUESTS_PER_ENDPOINT + 10 {
            lane.retire((1, "boot".into(), format!("request-{serial}")));
        }
        assert_eq!(lane.retired.len(), MAX_RETIRED_REQUESTS_PER_ENDPOINT);
        assert!(!lane
            .retired
            .contains(&(1, "boot".into(), "request-0".into())));
        assert!(lane.retired.contains(&(
            1,
            "boot".into(),
            format!("request-{}", MAX_RETIRED_REQUESTS_PER_ENDPOINT + 9)
        )));
    }

    #[test]
    fn stale_or_unknown_responses_do_not_damage_the_live_lane() {
        let mut commands = commands_with_in_flight();
        let unknown = ClientEndpointId::Ssh(
            crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        );

        assert!(commands
            .receive_chunk(&endpoint(), 2, "boot-a", "request-a", true, b"{}".to_vec())
            .unwrap()
            .is_none());
        assert!(commands
            .receive_chunk(&endpoint(), 1, "boot-b", "request-a", true, b"{}".to_vec())
            .unwrap()
            .is_none());
        assert!(commands
            .receive_chunk(&unknown, 1, "boot-a", "request-a", true, b"{}".to_vec())
            .unwrap()
            .is_none());
        assert!(has_in_flight(&commands));
    }

    // -----------------------------------------------------------------------
    // 切换标签闪烁（计划 2.1）：排队中的 TabFocus 折叠为最新目标
    // -----------------------------------------------------------------------

    fn tab_focus(id: &str, tab_id: &str) -> Box<Request> {
        Box::new(Request {
            id: id.into(),
            method: crate::api::schema::Method::TabFocus(crate::api::schema::TabTarget {
                tab_id: tab_id.into(),
            }),
        })
    }

    fn queued_ids(commands: &EndpointCommands, endpoint_id: &ClientEndpointId) -> Vec<String> {
        commands
            .lanes
            .get(endpoint_id)
            .map(|lane| {
                lane.queued
                    .iter()
                    .map(|queued| queued.request.id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn queued_tab_focus_requests_fold_to_the_latest_target_without_touching_in_flight() {
        let mut commands = commands_with_in_flight();
        let list = Box::new(Request {
            id: "list".into(),
            method: crate::api::schema::Method::WorkspaceList(
                crate::api::schema::EmptyParams::default(),
            ),
        });

        assert!(commands
            .enqueue(
                endpoint(),
                1,
                "boot-a".into(),
                tab_focus("focus-1", "tab_1"),
                true
            )
            .is_empty());
        assert!(commands
            .enqueue(endpoint(), 1, "boot-a".into(), list, false)
            .is_empty());
        // focus-1 排在非折叠请求 list 之前：不被跨越折叠，保持「focus-1 → list」顺序。
        assert!(commands
            .enqueue(
                endpoint(),
                1,
                "boot-a".into(),
                tab_focus("focus-2", "tab_2"),
                true
            )
            .is_empty());
        // 队尾连续的手势 focus-2 被 focus-3 折叠。
        assert_eq!(
            commands.enqueue(
                endpoint(),
                1,
                "boot-a".into(),
                tab_focus("focus-3", "tab_3"),
                true
            ),
            vec!["focus-2"]
        );

        assert_eq!(
            queued_ids(&commands, &endpoint()),
            vec!["focus-1", "list", "focus-3"]
        );
        let lane = commands.lanes.get(&endpoint()).unwrap();
        assert!(matches!(
            &lane.queued[2].request.method,
            crate::api::schema::Method::TabFocus(target) if target.tab_id == "tab_3"
        ));
        assert_eq!(
            lane.in_flight
                .as_ref()
                .map(|command| command.request_id.as_str()),
            Some("request-a"),
            "在途请求不受折叠影响"
        );
    }

    #[test]
    fn tab_focus_folding_only_touches_the_same_endpoint_lane() {
        let remote = ClientEndpointId::Ssh(
            crate::client::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
        );
        let mut commands = EndpointCommands::default();
        assert!(commands
            .enqueue(
                remote.clone(),
                2,
                "boot-b".into(),
                tab_focus("remote-focus", "tab_9"),
                true
            )
            .is_empty());
        assert!(commands
            .enqueue(
                endpoint(),
                1,
                "boot-a".into(),
                tab_focus("local-1", "tab_1"),
                true
            )
            .is_empty());
        assert_eq!(
            commands.enqueue(
                endpoint(),
                1,
                "boot-a".into(),
                tab_focus("local-2", "tab_2"),
                true
            ),
            vec!["local-1"]
        );
        assert_eq!(queued_ids(&commands, &remote), vec!["remote-focus"]);
        assert_eq!(queued_ids(&commands, &endpoint()), vec!["local-2"]);
    }

    #[test]
    fn folded_tab_focus_sends_only_the_latest_target() {
        use crate::client::endpoint::{EndpointNegotiation, EndpointRegistry, EndpointTransport};
        use std::sync::{Arc, Mutex};

        struct Recording(Arc<Mutex<Vec<String>>>);
        impl EndpointTransport for Recording {
            fn send(&mut self, message: &ClientMessage) -> io::Result<()> {
                if let ClientMessage::ClientShellEndpointRequest { request, .. } = message {
                    self.0.lock().unwrap().push(request.clone());
                }
                Ok(())
            }
        }

        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut endpoints =
            EndpointRegistry::new(Recording(sent.clone()), 1, EndpointNegotiation::default());
        let mut commands = EndpointCommands::default();
        let mut superseded = Vec::new();
        for (id, tab_id) in [
            ("focus-1", "tab_1"),
            ("focus-2", "tab_2"),
            ("focus-3", "tab_3"),
        ] {
            superseded.extend(commands.enqueue(
                endpoint(),
                1,
                "boot-a".into(),
                tab_focus(id, tab_id),
                true,
            ));
        }
        assert_eq!(superseded, vec!["focus-1", "focus-2"]);

        assert!(commands.send_next(&endpoint(), &mut endpoints).is_empty());
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "三次连续 TabFocus 只写出最后一个: {sent:?}");
        assert!(sent[0].contains("\"focus-3\""), "{}", sent[0]);
        assert!(sent[0].contains("tab_3"), "{}", sent[0]);
        assert!(commands.accepts_response(&endpoint(), 1, "boot-a", "focus-3"));
        assert!(!commands.accepts_response(&endpoint(), 1, "boot-a", "focus-1"));
        assert!(queued_ids(&commands, &endpoint()).is_empty());
    }

    #[test]
    fn identical_tab_focus_targets_fold_to_one_queued_request() {
        // 真实滚轮形态：快照未更新时相对 NextTab 连续三格算出同一个目标，
        // 被折叠的是重复请求而非递进目标。
        let mut commands = commands_with_in_flight();
        let mut superseded = Vec::new();
        for id in ["focus-1", "focus-2", "focus-3"] {
            superseded.extend(commands.enqueue(
                endpoint(),
                1,
                "boot-a".into(),
                tab_focus(id, "tab_2"),
                true,
            ));
        }
        assert_eq!(superseded, vec!["focus-1", "focus-2"]);
        assert_eq!(queued_ids(&commands, &endpoint()), vec!["focus-3"]);
        assert!(has_in_flight(&commands), "在途请求不受折叠影响");
    }

    #[test]
    fn tab_focus_folding_stops_at_the_first_non_foldable_request() {
        // 顺序保证：[TabFocus(A), X] 再入队 TabFocus(B) 得到 [TabFocus(A), X, TabFocus(B)]，
        // X 仍在 A 聚焦后执行；夹在中间的命令不会被跨越。
        let mut commands = EndpointCommands::default();
        let implicit_target = Box::new(Request {
            id: "split".into(),
            method: crate::api::schema::Method::WorkspaceList(
                crate::api::schema::EmptyParams::default(),
            ),
        });
        assert!(commands
            .enqueue(
                endpoint(),
                1,
                "boot-a".into(),
                tab_focus("focus-a", "tab_a"),
                true,
            )
            .is_empty());
        assert!(commands
            .enqueue(endpoint(), 1, "boot-a".into(), implicit_target, false)
            .is_empty());
        assert!(
            commands
                .enqueue(
                    endpoint(),
                    1,
                    "boot-a".into(),
                    tab_focus("focus-b", "tab_b"),
                    true,
                )
                .is_empty(),
            "队尾是非折叠请求：不跨越它折叠更早的聚焦"
        );
        assert_eq!(
            queued_ids(&commands, &endpoint()),
            vec!["focus-a", "split", "focus-b"]
        );

        // 只有队尾连续的可折叠请求才被折叠。
        assert_eq!(
            commands.enqueue(
                endpoint(),
                1,
                "boot-a".into(),
                tab_focus("focus-c", "tab_c"),
                true,
            ),
            vec!["focus-b"]
        );
        assert_eq!(
            queued_ids(&commands, &endpoint()),
            vec!["focus-a", "split", "focus-c"]
        );
    }

    #[test]
    fn programmatic_tab_focus_is_never_folded() {
        // worktree 创建后的自动聚焦 / 上下文菜单前置聚焦（coalesce=false）：
        // 既不会被用户手势折掉，也不会折掉别人。
        let mut commands = EndpointCommands::default();
        assert!(commands
            .enqueue(
                endpoint(),
                1,
                "boot-a".into(),
                tab_focus("worktree-focus", "tab_new"),
                false,
            )
            .is_empty());
        assert!(
            commands
                .enqueue(
                    endpoint(),
                    1,
                    "boot-a".into(),
                    tab_focus("wheel-1", "tab_2"),
                    true,
                )
                .is_empty(),
            "用户手势不折叠程序发起的聚焦"
        );
        assert!(
            commands
                .enqueue(
                    endpoint(),
                    1,
                    "boot-a".into(),
                    tab_focus("menu-focus", "tab_3"),
                    false,
                )
                .is_empty(),
            "程序发起的聚焦不折叠别人"
        );
        assert_eq!(
            queued_ids(&commands, &endpoint()),
            vec!["worktree-focus", "wheel-1", "menu-focus"]
        );
    }
}
