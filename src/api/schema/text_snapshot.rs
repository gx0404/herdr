use serde::{Deserialize, Serialize};

pub use crate::terminal::text_snapshot::FrozenText;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TextSnapshotCaptureParams {
    pub pane_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TextSnapshotTarget {
    pub snapshot_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TextSnapshotReadParams {
    pub snapshot_id: String,
    pub start_row: u32,
    pub rows: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TextSnapshotSelectionParams {
    pub snapshot_id: String,
    pub anchor: super::PaneTextPoint,
    pub cursor: super::PaneTextPoint,
}
