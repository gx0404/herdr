//! 多视图采用可选命名扩展，内部复用不可变的 generation 1 帧。

use super::{ClientPaneInputEvent, ServerMessage};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub(crate) const CAPABILITY: &str = "multi_view";
pub(crate) const SURFACE_KIND: &str = "endpoint.view-surface.v1";
pub(crate) const INPUT_KIND: &str = "endpoint.view-input.v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ViewSurface {
    pub boot_id: String,
    pub views_revision: u64,
    pub view_id: String,
    pub tab_id: String,
    pub payload: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ViewInput {
    pub boot_id: String,
    pub views_revision: u64,
    pub view_id: String,
    pub tab_id: String,
    pub pane_id: String,
    pub events: Vec<ClientPaneInputEvent>,
}

pub(crate) struct DecodedView {
    pub boot_id: String,
    pub views_revision: u64,
    pub view_id: String,
    pub tab_id: String,
    pub message: ServerMessage,
}

fn allowed(message: &ServerMessage) -> bool {
    matches!(
        message,
        ServerMessage::PaneSurface(_) | ServerMessage::PaneSurfacePatch(_)
    ) || matches!(message, ServerMessage::EndpointControl { kind, .. } if kind == super::surface_reuse::MESSAGE_KIND)
}

pub(crate) fn message(
    boot_id: &str,
    revision: u64,
    view_id: &str,
    tab_id: &str,
    message: &ServerMessage,
) -> Result<ServerMessage, String> {
    if !allowed(message) {
        return Err("视图扩展只能携带终端帧".into());
    }
    let mut bytes = Vec::new();
    super::write_message(&mut bytes, message).map_err(|error| error.to_string())?;
    if bytes.len() > super::MAX_GRAPHICS_FRAME_SIZE {
        return Err("视图帧过大".into());
    }
    let envelope = ViewSurface {
        boot_id: boot_id.into(),
        views_revision: revision,
        view_id: view_id.into(),
        tab_id: tab_id.into(),
        payload: base64::engine::general_purpose::STANDARD.encode(bytes),
    };
    Ok(ServerMessage::EndpointControl {
        kind: SURFACE_KIND.into(),
        data: serde_json::to_string(&envelope).map_err(|error| error.to_string())?,
    })
}

#[derive(Default)]
pub(crate) struct Decoder {
    boot_id: String,
    revision: u64,
    views: HashMap<String, (String, super::surface_reuse::Decoder)>,
}

impl Decoder {
    pub(crate) fn decode(&mut self, data: &str) -> Result<Option<DecodedView>, String> {
        if data.len() > super::MAX_GRAPHICS_FRAME_SIZE {
            return Err("视图包过大".into());
        }
        let envelope: ViewSurface = serde_json::from_str(data).map_err(|_| "视图包格式无效")?;
        if envelope.view_id.is_empty()
            || envelope.view_id.len() > 128
            || envelope.tab_id.len() > 256
            || envelope.boot_id.len() > 128
        {
            return Err("视图身份无效".into());
        }
        if self.boot_id == envelope.boot_id && envelope.views_revision < self.revision {
            return Ok(None);
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&envelope.payload)
            .map_err(|_| "视图帧编码无效")?;
        let mut cursor = std::io::Cursor::new(&bytes);
        let message: ServerMessage =
            super::read_message(&mut cursor, super::MAX_GRAPHICS_FRAME_SIZE)
                .map_err(|error| error.to_string())?;
        if cursor.position() as usize != bytes.len() || !allowed(&message) {
            return Err("视图帧包含未允许的数据".into());
        }
        let message_boot = match &message {
            ServerMessage::PaneSurface(frame) => {
                if usize::from(frame.frame.width) * usize::from(frame.frame.height)
                    != frame.frame.cells.len()
                {
                    return Err("视图帧面积无效".into());
                }
                frame.boot_id.clone()
            }
            ServerMessage::PaneSurfacePatch(patch) => patch.boot_id.clone(),
            ServerMessage::EndpointControl { data, .. } => {
                serde_json::from_str::<serde_json::Value>(data)
                    .ok()
                    .and_then(|value| {
                        value
                            .pointer("/surface/boot_id")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    })
                    .ok_or("复用视图缺少身份")?
            }
            _ => return Err("视图帧类型无效".into()),
        };
        if message_boot != envelope.boot_id {
            return Err("视图帧的启动身份不一致".into());
        }
        if self.boot_id != envelope.boot_id || self.revision != envelope.views_revision {
            self.views.clear();
            self.boot_id.clone_from(&envelope.boot_id);
            self.revision = envelope.views_revision;
        }
        if self.views.len() >= 32 && !self.views.contains_key(&envelope.view_id) {
            return Err("视图数超过限制".into());
        }
        let (tab, decoder) = self
            .views
            .entry(envelope.view_id.clone())
            .or_insert_with(|| (envelope.tab_id.clone(), Default::default()));
        if tab != &envelope.tab_id {
            *tab = envelope.tab_id.clone();
            *decoder = Default::default();
        }
        let message = decoder.decode(message)?;
        let inner_boot = match &message {
            ServerMessage::PaneSurface(frame) => &frame.boot_id,
            ServerMessage::PaneSurfacePatch(frame) => &frame.boot_id,
            _ => return Err("视图帧类型无效".into()),
        };
        if inner_boot != &envelope.boot_id {
            return Err("视图帧的启动身份不一致".into());
        }
        Ok(Some(DecodedView {
            boot_id: envelope.boot_id,
            views_revision: envelope.views_revision,
            view_id: envelope.view_id,
            tab_id: envelope.tab_id,
            message,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(boot: &str) -> ServerMessage {
        let buffer = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 4, 2));
        ServerMessage::PaneSurface(super::super::PaneSurfaceFrame {
            boot_id: boot.into(),
            projection_revision: 1,
            surface_revision: 1,
            frame: super::super::FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]),
            panes: Vec::new(),
            splits: Vec::new(),
            popup: None,
            graphics: Default::default(),
        })
    }

    #[test]
    fn views_have_independent_baselines_and_old_layouts_are_ignored() {
        let mut decoder = Decoder::default();
        for id in ["one", "two"] {
            let ServerMessage::EndpointControl { data, .. } =
                message("boot", 3, id, id, &frame("boot")).unwrap()
            else {
                panic!("control");
            };
            assert_eq!(decoder.decode(&data).unwrap().unwrap().view_id, id);
        }
        let ServerMessage::EndpointControl { data, .. } =
            message("boot", 2, "one", "one", &frame("boot")).unwrap()
        else {
            panic!("control");
        };
        assert!(decoder.decode(&data).unwrap().is_none());
    }

    #[test]
    fn arbitrary_control_and_cross_boot_frames_are_rejected() {
        assert!(message(
            "boot",
            1,
            "one",
            "tab",
            &ServerMessage::ServerShutdown { reason: None }
        )
        .is_err());
        let ServerMessage::EndpointControl { data, .. } =
            message("boot", 1, "one", "tab", &frame("old")).unwrap()
        else {
            panic!("control");
        };
        assert!(Decoder::default().decode(&data).is_err());
    }

    #[test]
    fn invalid_future_envelope_does_not_retire_current_views() {
        let mut decoder = Decoder::default();
        let ServerMessage::EndpointControl { data, .. } =
            message("boot", 2, "one", "tab", &frame("boot")).unwrap()
        else {
            panic!();
        };
        decoder.decode(&data).unwrap();
        let invalid = ViewSurface {
            boot_id: "boot".into(),
            views_revision: 999,
            view_id: "one".into(),
            tab_id: "tab".into(),
            payload: "invalid".into(),
        };
        assert!(decoder
            .decode(&serde_json::to_string(&invalid).unwrap())
            .is_err());
        assert!(decoder.decode(&data).unwrap().is_some());
    }
}
