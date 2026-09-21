use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationState {
    NotInstalled,
    Current,
    Outdated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IntegrationInfo {
    pub target: IntegrationTarget,
    pub label: String,
    pub command: String,
    pub available: bool,
    pub state: IntegrationState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IntegrationInstallParams {
    pub target: IntegrationTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IntegrationUninstallParams {
    pub target: IntegrationTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationTarget {
    Pi,
    Omp,
    Claude,
    Codex,
    Copilot,
    Devin,
    Droid,
    Kimi,
    Opencode,
    Kilo,
    Hermes,
    Qodercli,
    Qwen,
    Cursor,
    Mastracode,
    AntigravityCli,
    Grok,
}

impl IntegrationTarget {
    /// 本 fork 官方支持的集成。枚举本身是 generation-1 冻结 codec 可达的类型，
    /// 变体不删、不重排、不改 serde 名；其余变体只作为退役墓碑保留，
    /// 不出现在这里（`src/cli/spec.rs` 由它生成 `--target` 取值）。
    pub(crate) const ALL: [Self; 5] = [
        Self::Pi,
        Self::Claude,
        Self::Codex,
        Self::Kimi,
        Self::Opencode,
    ];

    /// 是否为已退役的集成：不在 `ALL` 里的变体一律视为退役，上游日后追加的
    /// 非官方变体因此自动落入退役分支。
    pub(crate) fn is_retired(self) -> bool {
        !Self::ALL.contains(&self)
    }

    /// serde 名（`snake_case`）。退役变体没有独立标签表，错误与日志用它指名。
    pub(crate) fn wire_name(self) -> String {
        match serde_json::to_value(self) {
            Ok(serde_json::Value::String(name)) => name,
            // 单元变体总是序列化成字符串；这里只是不 panic 的兜底。
            _ => format!("{self:?}").to_lowercase(),
        }
    }

    /// 按 serde 名解析（CLI 习惯的 `-` 视同 `_`）。未知名字返回 `None`。
    pub(crate) fn from_wire_name(name: &str) -> Option<Self> {
        serde_json::from_value(serde_json::Value::String(name.replace('-', "_"))).ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IntegrationInstallResult {
    pub messages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IntegrationUninstallResult {
    pub messages: Vec<String>,
}
