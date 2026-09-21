//! 官方来源清单。Agent 身份与实际计费 provider 分开处理。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use crate::config::{AccountUsageConfig, UsageAccountConfig};

#[derive(Clone, Copy)]
pub(super) enum Query {
    Codex,
    Kimi,
    /// 非交互子命令查询。`args` 是首选形态；`fallback_args` 在帮助预检确认首选形态不受支持
    /// （子命令或 flag 未列出）、或首选形态运行失败时回退一次（见 `transport::capture_query`）。
    /// 目前只有 opencode：首选官方只读入口 `opencode db <query> --format json`，回退到
    /// `opencode stats` 的人类可读框线表。
    Json {
        args: &'static [&'static str],
        fallback_args: Option<&'static [&'static str]>,
    },
    /// 官方 statusline 回调（claude）：厂商每次刷新状态栏都把官方 JSON 交给 herdr；herdr 能
    /// 改写其官方 `settings.json` 来接入 / 解除（`account.usage.integration`）。
    Callback,
    /// herdr 自带集成扩展的推送（pi）：扩展在会话事件里取数，经 socket 调
    /// `account.usage.report`。没有可改写的官方 `settings.json`，接入与否取决于集成是否安装。
    ExtensionPush,
}

pub(super) struct Provider {
    pub agent: &'static str,
    pub label: &'static str,
    pub command: &'static str,
    pub source: &'static str,
    pub method: &'static str,
    pub scope: &'static str,
    pub query: Query,
}

/// opencode 会话表的只读聚合查询：只取计数与合计，不读标题、目录等会话内容。列名与
/// `--format json` 的输出形状（`JSON.stringify(rows)`）在 1.17.20 与 1.18.31 上一致。
pub(super) const OPENCODE_SESSION_TOTALS_SQL: &str = "SELECT COUNT(*) AS sessions, \
COALESCE(SUM(CASE WHEN parent_id IS NULL OR parent_id = '' THEN 0 ELSE 1 END), 0) AS child_sessions, \
COALESCE(SUM(cost), 0) AS cost, \
COALESCE(SUM(tokens_input), 0) AS tokens_input, \
COALESCE(SUM(tokens_output), 0) AS tokens_output, \
COALESCE(SUM(tokens_reasoning), 0) AS tokens_reasoning, \
COALESCE(SUM(tokens_cache_read), 0) AS tokens_cache_read, \
COALESCE(SUM(tokens_cache_write), 0) AS tokens_cache_write \
FROM session";

/// 官方来源只登记范围内的五家；zcode 的用量等其外部来源适配器，不在这里。
pub(super) const PROVIDERS: &[Provider] = &[
    Provider { agent: "codex", label: "Codex", command: "codex", source: "https://learn.chatgpt.com/docs/app-server", method: "account/rateLimits/read; account/usage/read", scope: "account", query: Query::Codex },
    // claude 主路径是官方 statusline 回调；`/usage` 交互探测只在 `interactive_probe` 开启且
    // 显式刷新时作为兜底（见 `interactive_fallback`），登录态由非交互 `auth status` 预检。
    Provider { agent: "claude", label: "Claude Code", command: "claude", source: "https://code.claude.com/docs/en/statusline", method: "statusline JSON rate_limits / cost / context_window；/usage", scope: "account", query: Query::Callback },
    Provider { agent: "kimi", label: "Kimi Code", command: "kimi", source: "https://www.kimi.com/code/docs/en/kimi-code-cli/reference/server-api.html", method: "GET /api/v1/oauth/usage；/usage", scope: "account", query: Query::Kimi },
    // opencode 没有任何账号额度接口：两种形态给出的都是本地会话统计（`scope = local`）。
    Provider { agent: "opencode", label: "OpenCode", command: "opencode", source: "https://opencode.ai/docs/cli/", method: "opencode db <query> --format json", scope: "local", query: Query::Json { args: &["db", OPENCODE_SESSION_TOTALS_SQL, "--format", "json"], fallback_args: Some(&["stats"]) } },
    // pi 是多服务商 CLI：RPC 模式是另起的 headless 进程，连不进正在跑的 TUI，所以用量由 herdr
    // 的 pi 扩展在会话内取数后推送；条目是会话级统计，不是账号额度。
    Provider { agent: "pi", label: "Pi", command: "pi", source: "https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/extensions.md", method: "herdr pi 扩展：ctx.getContextUsage() + 会话用量合计", scope: "session", query: Query::ExtensionPush },
];

pub(super) fn provider(agent: &str) -> Option<&'static Provider> {
    let agent = agent.trim().to_ascii_lowercase();
    let canonical = match agent.as_str() {
        "claude-code" | "claude code" => "claude",
        "kimi-code" | "kimi code" => "kimi",
        other => other,
    };
    PROVIDERS.iter().find(|entry| entry.agent == canonical)
}

/// 交互探测使用的斜杠命令：目前只有 claude——主路径已是官方回调，`/usage` 只在显式刷新且
/// `interactive_probe` 开启时作为兜底。
pub(super) fn interactive_fallback(provider: &Provider) -> Option<&'static str> {
    match provider.query {
        Query::Callback if provider.agent == "claude" => Some("/usage"),
        _ => None,
    }
}

/// 该厂商是否只能靠推送（官方 statusline 回调或 herdr 扩展推送）拿到样本：显式刷新不会产生
/// 新样本，但仍可能刷新登录 / 绑定占位（claude 的登录预检就是这样）。claude 开启
/// `interactive_probe` 后有可回落的 `/usage` 探测（在稳定探测目录里，需用户信任一次），不再
/// 是纯回调。
pub(super) fn callback_only(provider: &Provider, config: &AccountUsageConfig) -> bool {
    match provider.query {
        Query::Callback => !(provider.agent == "claude" && config.interactive_probe),
        Query::ExtensionPush => true,
        _ => false,
    }
}

/// 该厂商是否支持官方 statusline 回调开关（`account.usage.integration`）：回调型查询且
/// `integration::usage` 能改写其官方 `settings.json`。厂商名单的唯一真源在
/// `integration::usage::supports_statusline`，这里只把它与查询方式合成一条谓词，供
/// `UsageProviderInfo.supports_callback` 宣告与账号级回调态判定共用。扩展推送型厂商（pi）
/// 没有可改写的 `settings.json`，不宣告这个开关（见 `supports_extension_push`）。
pub(super) fn supports_callback(provider: &Provider) -> bool {
    matches!(provider.query, Query::Callback)
        && crate::integration::usage_supports_statusline(provider.agent)
}

/// 该厂商的用量是否由 herdr 自带的集成扩展推送：推送型查询且 `integration::usage` 登记了
/// 该扩展。与 `supports_callback` 是两种来源——这里没有开关可切，接入与否取决于集成是否
/// 已安装（`herdr integration install <agent>`）。
pub(super) fn supports_extension_push(provider: &Provider) -> bool {
    matches!(provider.query, Query::ExtensionPush)
        && crate::integration::usage_supports_extension_push(provider.agent)
}

/// 支持官方 statusline 回调的厂商的账号：其官方 `settings.json` 是否已接入 herdr 用量回调；
/// 文件不存在（全新安装）算未接入，读不了 / 无法解析 / 含无法识别的 herdr 回调时为 `None`。
/// 判据与 `integration::usage` 的启用逻辑同源（按本平台命令形态比对，不匹配明文子串）；
/// 路径推导直接复用 `integration::usage::settings_path`（写入与检测指向同一个文件）。
/// 不支持回调的厂商为 `None`。
///
/// 结果按文件戳（mtime + 大小）缓存：读路径每 2 秒轮询、事件扇出与 providers 都会调用，
/// 不能在服务循环里每次同步读文件；戳未变且未超 `AVAILABILITY_TTL` 时直接复用
/// （mtime 粗粒度，同尺寸连续改写可能戳相同，TTL 兜底）。
pub(super) fn statusline_enabled(account: &UsageAccountConfig) -> Option<bool> {
    let path = crate::integration::usage_settings_path(account).ok()?;
    let stamp = file_stamp(&path);
    let now = Instant::now();
    if let Ok(cache) = statusline_cache().lock() {
        if let Some((cached, at, verdict)) = cache.get(&path) {
            if *cached == stamp && now.duration_since(*at) < AVAILABILITY_TTL {
                return *verdict;
            }
        }
    }
    let verdict = match std::fs::read_to_string(&path) {
        Ok(settings) => crate::integration::usage_statusline_enabled(&settings, &account.agent),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    };
    if let Ok(mut cache) = statusline_cache().lock() {
        cache.insert(path, (stamp, now, verdict));
    }
    verdict
}

/// `statusline_enabled` 的按路径缓存：文件戳 + 采样时刻 + 判定。
type StatuslineCache = HashMap<PathBuf, (Option<FileStamp>, Instant, Option<bool>)>;

fn statusline_cache() -> &'static Mutex<StatuslineCache> {
    static CACHE: OnceLock<Mutex<StatuslineCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// PATH / 安装布局扫描结果的缓存时长；服务循环里依赖这些扫描的周期都与它对齐。
pub(super) const AVAILABILITY_TTL: Duration = Duration::from_secs(30);

fn availability_cache() -> &'static Mutex<HashMap<&'static str, (Instant, bool)>> {
    static CACHE: OnceLock<Mutex<HashMap<&'static str, (Instant, bool)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `command_path` 的按命令缓存：与 `availability_cache` 同源的 TTL，避免每次指纹计算都
/// 完整遍历 PATH。
type CommandPathCache = HashMap<String, (Instant, Option<PathBuf>)>;

fn command_path_cache() -> &'static Mutex<CommandPathCache> {
    static CACHE: OnceLock<Mutex<CommandPathCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 大型可变状态文件里登录子树的摘要缓存：文件戳未变且未超过 TTL 就不重新解析
/// （文件系统 mtime 是粗粒度的，同尺寸的连续改写可能戳相同，所以 TTL 兜底）。
type LoginDigestCache = HashMap<PathBuf, (FileStamp, Instant, Option<String>)>;

fn login_digest_cache() -> &'static Mutex<LoginDigestCache> {
    static CACHE: OnceLock<Mutex<LoginDigestCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Providers that share a client-facing integration target reuse the
/// settings-page detection (multi-alias PATH scan plus layout fallbacks) so
/// both surfaces agree on what "installed" means.
fn integration_target_for(agent: &str) -> Option<crate::api::schema::IntegrationTarget> {
    use crate::api::schema::IntegrationTarget;
    match agent {
        "codex" => Some(IntegrationTarget::Codex),
        "claude" => Some(IntegrationTarget::Claude),
        "kimi" => Some(IntegrationTarget::Kimi),
        "opencode" => Some(IntegrationTarget::Opencode),
        "pi" => Some(IntegrationTarget::Pi),
        _ => None,
    }
}

/// Whether the provider's official CLI is installed locally. PATH lookups
/// hit the filesystem, so results are cached briefly and reused across the
/// provider-list polling cadence.
pub(super) fn provider_installed(provider: &Provider) -> bool {
    let now = Instant::now();
    if let Ok(cache) = availability_cache().lock() {
        if let Some((at, value)) = cache.get(&provider.agent) {
            if now.duration_since(*at) < AVAILABILITY_TTL {
                return *value;
            }
        }
    }
    // Version-manager installs (nvm/volta/standalone) are invisible to a
    // detached server's PATH; the integration layout checks cover them.
    let value = match integration_target_for(provider.agent) {
        Some(target) => crate::integration::integration_target_available(target),
        None => crate::integration::command_available(provider.command),
    };
    if let Ok(mut cache) = availability_cache().lock() {
        cache.insert(provider.agent, (now, value));
    }
    value
}

/// 一个文件的身份戳：路径 + 修改时间 + 大小。文件不存在时整体为 `None`，
/// 这样「从无到有」（首次登录）同样算变化。
pub(super) type FileStamp = (PathBuf, Option<SystemTime>, u64);

/// 终态自愈的判据：官方 CLI 可执行文件（路径 + 版本指纹）、只承载登录态的凭据文件的
/// 身份戳，以及大型可变状态文件里登录子树的内容摘要。任一变化都意味着
/// 「未登录 / 不支持 / 无权限」的结论可能已过时；与登录无关的文件改动不得计入，否则指纹
/// 抖动等价于无限重试。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ProbeFingerprint {
    pub cli: Option<FileStamp>,
    pub credentials: Vec<Option<FileStamp>>,
    /// 登录子树的摘要（如 claude `.claude.json` 的 `oauthAccount`）；文件或子树缺失为 `None`。
    pub login_digest: Option<String>,
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((path.to_path_buf(), metadata.modified().ok(), metadata.len()))
}

/// PATH 上第一个可执行的 `command`；找不到时回退到布局解析（目前只有 codex）。
/// 结果按 `AVAILABILITY_TTL` 缓存：指纹计算按周期对全部终态账号调用，不能每次都遍历 PATH。
pub(super) fn command_path(command: &str) -> Option<PathBuf> {
    let now = Instant::now();
    if let Ok(cache) = command_path_cache().lock() {
        if let Some((at, value)) = cache.get(command) {
            if now.duration_since(*at) < AVAILABILITY_TTL {
                return value.clone();
            }
        }
    }
    let value = command_path_uncached(command);
    if let Ok(mut cache) = command_path_cache().lock() {
        cache.insert(command.to_owned(), (now, value.clone()));
    }
    value
}

/// 官方 CLI 可执行文件的身份戳（路径 + mtime + 大小）：帮助文本缓存以它为失效判据。
pub(super) fn command_stamp(command: &str) -> Option<FileStamp> {
    command_path(command).as_deref().and_then(file_stamp)
}

fn command_path_uncached(command: &str) -> Option<PathBuf> {
    let from_path = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            crate::integration::command_path_candidates(&dir, command)
                .into_iter()
                .find(|path| crate::integration::executable_file_exists(path))
        })
    });
    from_path.or_else(|| {
        if command == "codex" {
            crate::integration::codex_layout_binary_path()
        } else {
            None
        }
    })
}

/// 各厂商登录状态落在哪些文件：只列本仓库已核实的路径，未知厂商只看 CLI 指纹。
/// 目录推导全部走 `integration::env`（环境覆盖、`~` 展开与平台分支的唯一真源）；
/// `profile_dir` 配置优先于环境变量与默认目录。
fn credential_paths(provider: &Provider, account: &UsageAccountConfig) -> Vec<PathBuf> {
    let profile = account.profile_dir.clone();
    let dir = |fallback: std::io::Result<PathBuf>| profile.clone().or_else(|| fallback.ok());
    let mut paths = Vec::new();
    match provider.agent {
        "claude" => {
            // 只取 OAuth 凭据文件；`.claude.json` 是高频重写的通用状态文件，整文件戳不能
            // 当登录判据，其登录子树走 `login_digest`。
            if let Some(dir) = dir(crate::integration::claude_dir()) {
                paths.push(dir.join(".credentials.json"));
            }
        }
        "codex" => {
            if let Some(dir) = dir(crate::integration::codex_dir()) {
                paths.push(dir.join("auth.json"));
            }
        }
        "kimi" => {
            // Kimi Code 的 `credentials` 是目录（内含 `kimi-code.json` 与 `mcp/`）：目录的 mtime
            // 只在增删条目时变化，原地改写登录文件不会动它，所以要 stat 登录文件本身。
            if let Some(dir) = dir(crate::integration::kimi_dir()) {
                paths.push(dir.join("credentials").join("kimi-code.json"));
            }
        }
        "opencode" => {
            if let Some(dir) = dir(crate::integration::opencode_data_dir()) {
                paths.push(dir.join("auth.json"));
            }
        }
        _ => {}
    }
    paths
}

/// 承载登录子树的大型状态文件：`(路径, JSON 指针)`。目前只有 claude 的 `.claude.json`
/// （`oauthAccount`：账号 uuid/邮箱/组织），`profile_dir` 即 `CLAUDE_CONFIG_DIR` 时文件在其中。
fn login_subtree(
    provider: &Provider,
    account: &UsageAccountConfig,
) -> Option<(PathBuf, &'static str)> {
    match provider.agent {
        "claude" => {
            let file = match &account.profile_dir {
                Some(dir) => dir.join(".claude.json"),
                None => crate::integration::claude_state_file().ok()?,
            };
            Some((file, "/oauthAccount"))
        }
        _ => None,
    }
}

/// 状态文件过大时不解析（正常 `.claude.json` 在几百 KiB 量级）。
const MAX_LOGIN_STATE_BYTES: u64 = 16 * 1024 * 1024;

/// 读取登录子树并求摘要：文件戳未变时直接复用缓存，避免每个周期都解析整份 JSON。
/// 文件不存在、过大、不是 JSON 或没有该子树 ⇒ `None`（登出后 `oauthAccount` 被移除也算变化）。
fn login_digest(path: &Path, pointer: &str) -> Option<String> {
    let stamp = file_stamp(path)?;
    if stamp.2 > MAX_LOGIN_STATE_BYTES {
        return None;
    }
    let now = Instant::now();
    if let Ok(cache) = login_digest_cache().lock() {
        if let Some((cached, at, digest)) = cache.get(path) {
            if *cached == stamp && now.duration_since(*at) < AVAILABILITY_TTL {
                return digest.clone();
            }
        }
    }
    let digest = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| value.pointer(pointer).cloned())
        .map(|subtree| {
            use sha2::{Digest, Sha256};
            let canonical = subtree.to_string();
            format!("{:x}", Sha256::digest(canonical.as_bytes()))
        });
    if let Ok(mut cache) = login_digest_cache().lock() {
        cache.insert(path.to_path_buf(), (stamp, now, digest.clone()));
    }
    digest
}

/// 计算当前指纹：几次 `stat` 加缓存的 PATH 解析，由服务循环按 `AVAILABILITY_TTL` 周期对
/// 终态条目调用，探测完成时也调用一次记录基线。
pub(super) fn probe_fingerprint(
    provider: &Provider,
    account: &UsageAccountConfig,
) -> ProbeFingerprint {
    ProbeFingerprint {
        cli: command_path(provider.command)
            .as_deref()
            .and_then(file_stamp),
        credentials: credential_paths(provider, account)
            .iter()
            .map(|path| file_stamp(path))
            .collect(),
        login_digest: login_subtree(provider, account)
            .and_then(|(path, pointer)| login_digest(&path, pointer)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 官方来源只登记范围内的五家：每一家都对应一个仍受支持的 `detect::Agent`，范围外的厂商
    /// （含历史别名）一律查不到，不会再被探测或接受回调。
    #[test]
    fn registry_lists_only_the_in_scope_providers() {
        assert_eq!(
            PROVIDERS.iter().map(|p| p.agent).collect::<Vec<_>>(),
            vec!["codex", "claude", "kimi", "opencode", "pi"]
        );
        let detectable = crate::detect::Agent::ALL
            .into_iter()
            .map(crate::detect::agent_label)
            .collect::<std::collections::HashSet<_>>();
        for entry in PROVIDERS {
            assert!(
                detectable.contains(entry.agent),
                "{} 必须是可识别的 agent",
                entry.agent
            );
            assert!(entry.source.starts_with("https://"));
        }
        assert!(provider("Claude Code").is_some(), "别名仍归一到 claude");
        assert!(provider("kimi-code").is_some(), "别名仍归一到 kimi");
        for retired in [
            "gemini",
            "cursor",
            "devin",
            "antigravity",
            "agy",
            "cline",
            "omp",
            "mastracode",
            "github-copilot",
            "copilot",
            "kiro",
            "droid",
            "amp",
            "grok",
            "hermes",
            "kilo",
            "qodercli",
            "qoder",
            "qwen",
            "letta",
            "maki",
            "muse",
            "zcode",
        ] {
            assert!(provider(retired).is_none(), "{retired} 不在用量范围内");
        }
    }

    #[test]
    fn opencode_reads_the_session_table_and_falls_back_to_the_stats_table() {
        let opencode = provider("opencode").unwrap();
        let Query::Json {
            args,
            fallback_args,
        } = opencode.query
        else {
            panic!("opencode 是非交互子命令查询");
        };
        assert_eq!(
            args,
            &["db", OPENCODE_SESSION_TOTALS_SQL, "--format", "json"],
            "首选官方只读入口（1.17.20 与 1.18.31 都有 db 子命令与 --format）"
        );
        assert_eq!(
            fallback_args,
            Some(&["stats"][..]),
            "没有 db 子命令或查询失败的版本回退到框线表"
        );
        assert_eq!(opencode.scope, "local", "会话统计不是账号额度");
        // 查询只读聚合：不写库，也不取标题 / 目录等会话内容。
        let sql = OPENCODE_SESSION_TOTALS_SQL.to_ascii_uppercase();
        assert!(sql.starts_with("SELECT "));
        for forbidden in [
            "INSERT",
            "UPDATE",
            "DELETE",
            "DROP",
            "ALTER",
            "PRAGMA",
            "ATTACH",
            ";",
            "TITLE",
            "DIRECTORY",
        ] {
            assert!(!sql.contains(forbidden), "查询不得包含 {forbidden}");
        }
    }

    #[test]
    fn claude_uses_the_callback_path_with_an_opt_in_interactive_fallback() {
        let claude = provider("claude").unwrap();
        assert!(
            matches!(claude.query, Query::Callback),
            "claude 主路径是官方回调"
        );
        assert_eq!(interactive_fallback(claude), Some("/usage"));
        for agent in ["codex", "kimi", "opencode", "pi"] {
            assert_eq!(interactive_fallback(provider(agent).unwrap()), None);
        }

        let mut config = AccountUsageConfig::default();
        assert!(
            callback_only(claude, &config),
            "默认关闭交互探测：显式刷新拿不到新数据"
        );
        assert!(callback_only(provider("pi").unwrap(), &config));
        assert!(!callback_only(provider("kimi").unwrap(), &config));
        assert!(!callback_only(provider("opencode").unwrap(), &config));
        config.interactive_probe = true;
        assert!(
            !callback_only(claude, &config),
            "开启后显式刷新可回落到稳定探测目录里的 /usage（需用户信任该目录一次）"
        );
        assert!(
            callback_only(provider("pi").unwrap(), &config),
            "扩展推送型厂商不受交互探测开关影响"
        );
    }

    /// 「官方 statusline 回调」与「herdr 扩展推送」是两种来源：前者宣告可切换的回调开关，后者
    /// 没有 `settings.json` 可改写，只宣告自己由扩展推送。两者互斥，其余厂商两者皆否。
    #[test]
    fn statusline_callbacks_and_extension_pushes_are_distinct_sources() {
        let claude = provider("claude").unwrap();
        let pi = provider("pi").unwrap();
        assert!(matches!(pi.query, Query::ExtensionPush));
        assert_eq!(pi.scope, "session", "pi 的条目是会话级统计");
        assert!(supports_callback(claude) && !supports_extension_push(claude));
        assert!(supports_extension_push(pi) && !supports_callback(pi));
        for agent in ["codex", "kimi", "opencode"] {
            let entry = provider(agent).unwrap();
            assert!(!supports_callback(entry) && !supports_extension_push(entry));
        }
    }

    #[test]
    fn claude_statusline_detection_follows_the_configured_profile() {
        let base = std::env::temp_dir().join(format!(
            "herdr-claude-statusline-{}-{}",
            std::process::id(),
            crate::server::observability::now_ms()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let account = UsageAccountConfig {
            id: "claude:work".into(),
            agent: "claude".into(),
            profile_dir: Some(base.clone()),
            ..Default::default()
        };
        assert_eq!(
            statusline_enabled(&account),
            Some(false),
            "无 settings.json：全新安装算未接入，不是「无法判定」"
        );
        std::fs::write(
            base.join("settings.json"),
            format!(
                "{{\"statusLine\":{{\"type\":\"command\",\"command\":{}}}}}",
                serde_json::Value::String(
                    crate::platform::usage_statusline_pipeline("claude", "").unwrap()
                )
            ),
        )
        .unwrap();
        assert_eq!(
            statusline_enabled(&account),
            Some(true),
            "判据按本平台命令形态比对（Windows 是 -EncodedCommand 形态）"
        );
        std::fs::write(
            base.join("settings.json"),
            "{\"statusLine\":{\"type\":\"command\",\"command\":\"python custom.py\"}}",
        )
        .unwrap();
        assert_eq!(statusline_enabled(&account), Some(false));
        std::fs::write(base.join("settings.json"), "{").unwrap();
        assert_eq!(statusline_enabled(&account), None, "无法解析");
        let _ = std::fs::remove_dir_all(base);
    }

    /// 回调开关能力与检测共用一份厂商名单与路径推导：不支持回调的厂商既不宣告能力也没有
    /// 当前态（含扩展推送型的 pi）。
    #[test]
    fn statusline_detection_shares_the_integration_path_and_provider_list() {
        let claude = provider("claude").unwrap();
        assert!(supports_callback(claude));
        assert!(
            !supports_callback(provider("pi").unwrap()),
            "扩展推送型厂商没有可改写的 settings，不宣告回调开关"
        );
        assert!(!supports_callback(provider("codex").unwrap()));
        let base = std::env::temp_dir().join(format!(
            "herdr-statusline-path-{}-{}",
            std::process::id(),
            crate::server::observability::now_ms()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let account = UsageAccountConfig {
            id: "claude:work".into(),
            agent: "claude".into(),
            profile_dir: Some(base.clone()),
            ..Default::default()
        };
        assert_eq!(
            crate::integration::usage_settings_path(&account).unwrap(),
            base.join("settings.json"),
            "检测与写入指向同一个文件"
        );
        assert_eq!(statusline_enabled(&account), Some(false));
        std::fs::write(
            base.join("settings.json"),
            format!(
                "{{\"statusLine\":{{\"type\":\"command\",\"command\":{}}}}}",
                serde_json::Value::String(
                    crate::platform::usage_statusline_pipeline("claude", "").unwrap()
                )
            ),
        )
        .unwrap();
        assert_eq!(
            statusline_enabled(&account),
            Some(true),
            "文件戳变化即重读，不被缓存挡住"
        );
        for agent in ["codex", "pi"] {
            let other = UsageAccountConfig {
                id: format!("{agent}:default"),
                agent: agent.into(),
                profile_dir: Some(base.clone()),
                ..Default::default()
            };
            assert_eq!(
                statusline_enabled(&other),
                None,
                "不支持 statusline 回调的厂商没有当前态"
            );
        }
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn integration_detection_covers_every_provider() {
        // 五家都有对应的集成目标，安装判定与设置页共用同一套检测；冻结枚举里其余变体是
        // 已退役的墓碑，这里不引用。
        for provider in PROVIDERS {
            let target = integration_target_for(provider.agent)
                .unwrap_or_else(|| panic!("{} 缺少集成目标", provider.agent));
            assert_eq!(
                crate::integration::integration_target_label(target),
                provider.agent
            );
        }
        assert!(integration_target_for("gemini").is_none());
    }

    #[test]
    fn probe_fingerprint_tracks_the_credential_file_of_the_configured_profile() {
        let base = std::env::temp_dir().join(format!(
            "herdr-fingerprint-{}-{}",
            std::process::id(),
            crate::server::observability::now_ms()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let provider = provider("codex").unwrap();
        let account = UsageAccountConfig {
            id: "codex:work".into(),
            agent: "codex".into(),
            profile_dir: Some(base.clone()),
            ..Default::default()
        };
        // 文件尚不存在：凭据戳为 None，但仍占一个位置。
        let before = probe_fingerprint(provider, &account);
        assert_eq!(before.credentials, vec![None]);
        assert_eq!(before, before.clone());

        std::fs::write(base.join("auth.json"), b"{\"tokens\":{}}").unwrap();
        let after = probe_fingerprint(provider, &account);
        assert_ne!(before, after, "首次登录（文件从无到有）必须算变化");
        let stamp = after.credentials[0].as_ref().expect("凭据文件已存在");
        assert_eq!(stamp.0, base.join("auth.json"));
        assert_eq!(stamp.2, 13);
        // 内容不变时指纹稳定；CLI 指纹与凭据无关。
        assert_eq!(after, probe_fingerprint(provider, &account));
        assert_eq!(after.cli, before.cli);

        std::fs::write(base.join("auth.json"), b"{\"tokens\":{\"a\":1}}").unwrap();
        assert_ne!(
            after,
            probe_fingerprint(provider, &account),
            "改写凭据必须算变化"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn claude_fingerprint_ignores_unrelated_state_churn_and_tracks_the_login_subtree() {
        let base = std::env::temp_dir().join(format!(
            "herdr-claude-fingerprint-{}-{}",
            std::process::id(),
            crate::server::observability::now_ms()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let provider = provider("claude").unwrap();
        let account = UsageAccountConfig {
            id: "claude:work".into(),
            agent: "claude".into(),
            profile_dir: Some(base.clone()),
            ..Default::default()
        };
        let state = base.join(".claude.json");
        let write_state = |starts: u64, account_json: Option<&str>| {
            let oauth = account_json
                .map(|json| format!(",\"oauthAccount\":{json}"))
                .unwrap_or_default();
            std::fs::write(
                &state,
                format!("{{\"numStartups\":{starts},\"projects\":{{}}{oauth}}}"),
            )
            .unwrap();
        };
        // 未登录：无凭据文件、无 oauthAccount。
        write_state(1, None);
        let signed_out = probe_fingerprint(provider, &account);
        assert_eq!(signed_out.credentials, vec![None]);
        assert_eq!(signed_out.login_digest, None);

        // claude 正常使用中反复重写 .claude.json（启动计数、项目历史）：登录态未变 ⇒ 指纹不变。
        write_state(2, None);
        std::fs::write(
            &state,
            format!("{}{}", std::fs::read_to_string(&state).unwrap(), "  "),
        )
        .unwrap();
        assert_eq!(
            probe_fingerprint(provider, &account),
            signed_out,
            "无关字段抖动不算变化"
        );

        // 登录：凭据文件出现 + oauthAccount 写入。
        std::fs::write(base.join(".credentials.json"), b"{\"claudeAiOauth\":{}}").unwrap();
        write_state(
            3,
            Some("{\"accountUuid\":\"a\",\"emailAddress\":\"a@example.test\"}"),
        );
        let signed_in = probe_fingerprint(provider, &account);
        assert_ne!(signed_in, signed_out, "登录必须算变化");
        assert!(signed_in.login_digest.is_some());
        // 同一账号再抖动一次通用字段：仍稳定。
        write_state(
            4,
            Some("{\"accountUuid\":\"a\",\"emailAddress\":\"a@example.test\"}"),
        );
        assert_eq!(probe_fingerprint(provider, &account), signed_in);
        // 切换账号：只有子树变了。文件系统 mtime 粗粒度（毫秒级），先等一拍让戳变化，
        // 否则摘要缓存会命中（生产里由 TTL 兜底）。
        std::thread::sleep(Duration::from_millis(50));
        write_state(
            4,
            Some("{\"accountUuid\":\"b\",\"emailAddress\":\"b@example.test\"}"),
        );
        let switched = probe_fingerprint(provider, &account);
        assert_ne!(
            switched.login_digest, signed_in.login_digest,
            "账号切换必须算变化"
        );
        assert_eq!(switched.credentials, signed_in.credentials);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn command_path_is_cached_per_command() {
        let first = command_path("herdr-no-such-command-for-tests");
        assert_eq!(first, None);
        let cached = command_path_cache()
            .lock()
            .map(|cache| cache.contains_key("herdr-no-such-command-for-tests"))
            .unwrap_or(false);
        assert!(cached, "结果进入缓存");
    }

    #[test]
    fn providers_without_a_verified_credential_file_only_fingerprint_the_cli() {
        let provider = provider("pi").unwrap();
        let account = UsageAccountConfig {
            id: "pi:default".into(),
            agent: "pi".into(),
            ..Default::default()
        };
        assert!(probe_fingerprint(provider, &account).credentials.is_empty());
    }

    /// Kimi Code 的 `credentials` 是目录：登录态在其中的 `kimi-code.json`。原地改写登录文件
    /// 不会改变目录自身的 mtime / 大小，所以指纹必须落在文件上，否则重新登录永远不算变化。
    #[test]
    fn kimi_fingerprint_tracks_the_login_file_inside_the_credentials_directory() {
        let base = std::env::temp_dir().join(format!(
            "herdr-kimi-fingerprint-{}-{}",
            std::process::id(),
            crate::server::observability::now_ms()
        ));
        std::fs::create_dir_all(base.join("credentials").join("mcp")).unwrap();
        let provider = provider("kimi").unwrap();
        let account = UsageAccountConfig {
            id: "kimi:work".into(),
            agent: "kimi".into(),
            profile_dir: Some(base.clone()),
            ..Default::default()
        };
        let signed_out = probe_fingerprint(provider, &account);
        assert_eq!(
            signed_out.credentials,
            vec![None],
            "只有目录、没有登录文件：未登录"
        );

        let login = base.join("credentials").join("kimi-code.json");
        std::fs::write(&login, b"{\"placeholder\":1}").unwrap();
        let signed_in = probe_fingerprint(provider, &account);
        assert_ne!(signed_in, signed_out, "首次登录必须算变化");
        let stamp = signed_in.credentials[0].as_ref().expect("登录文件已存在");
        assert_eq!(stamp.0, login);
        assert_eq!(stamp.2, 17, "戳取自登录文件本身，不是目录");

        std::fs::write(&login, b"{\"placeholder\":12}").unwrap();
        assert_ne!(
            probe_fingerprint(provider, &account),
            signed_in,
            "原地改写登录文件必须算变化"
        );
        let _ = std::fs::remove_dir_all(base);
    }
}
