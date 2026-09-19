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
    /// 非交互子命令查询。`args` 是首选形态；`fallback_args` 在子命令级 `--help` 预检确认首选
    /// flag 不受支持、或首选形态以用法错误失败时回退一次（见 `transport::capture_query`）。
    /// 目前只有 opencode：1.17.x 的 `stats` 没有 `--json`，回退到人类可读的框线表。
    Json {
        args: &'static [&'static str],
        fallback_args: Option<&'static [&'static str]>,
    },
    Interactive(&'static str),
    Callback,
    Portal,
}

/// 只有首选形态、没有回退的非交互查询。
const fn json(args: &'static [&'static str]) -> Query {
    Query::Json {
        args,
        fallback_args: None,
    }
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

pub(super) const PROVIDERS: &[Provider] = &[
    Provider { agent: "codex", label: "Codex", command: "codex", source: "https://learn.chatgpt.com/docs/app-server", method: "account/rateLimits/read; account/usage/read", scope: "account", query: Query::Codex },
    // claude 主路径是官方 statusline 回调；`/usage` 交互探测只在 `interactive_probe` 开启且
    // 显式刷新时作为兜底（见 `interactive_fallback`），登录态由非交互 `auth status` 预检。
    Provider { agent: "claude", label: "Claude Code", command: "claude", source: "https://code.claude.com/docs/en/statusline", method: "statusline JSON rate_limits；/usage", scope: "account", query: Query::Callback },
    Provider { agent: "kimi", label: "Kimi Code", command: "kimi", source: "https://www.kimi.com/code/docs/en/kimi-code-cli/reference/server-api.html", method: "GET /api/v1/oauth/usage；/usage", scope: "account", query: Query::Kimi },
    Provider { agent: "gemini", label: "Gemini CLI", command: "gemini", source: "https://geminicli.com/docs/get-started/", method: "/stats（刷新官方配额）", scope: "account", query: Query::Interactive("/stats") },
    Provider { agent: "cursor", label: "Cursor", command: "cursor-agent", source: "https://cursor.com/docs/account/teams/admin-api", method: "Admin API /teams/spend", scope: "organization", query: Query::Portal },
    Provider { agent: "devin", label: "Devin", command: "devin", source: "https://docs.devin.ai/api-reference/v3/consumption/consumption-daily-users", method: "Consumption API", scope: "organization", query: Query::Portal },
    Provider { agent: "antigravity", label: "Antigravity", command: "agy", source: "https://antigravity.google/docs/cli/commands/usage", method: "statusline JSON quota；/usage", scope: "account", query: Query::Callback },
    Provider { agent: "cline", label: "Cline", command: "cline", source: "https://docs.cline.bot/enterprise-solutions/api-reference", method: "GET /api/v1/users/{id}/balance; usages", scope: "account", query: Query::Portal },
    Provider { agent: "omp", label: "Oh My Pi", command: "omp", source: "https://github.com/can1357/oh-my-pi/blob/main/packages/coding-agent/src/commands/usage.ts", method: "omp usage --json", scope: "account", query: json(&["usage", "--json"]) },
    Provider { agent: "mastracode", label: "Mastra Code", command: "mastracode", source: "https://code.mastra.ai/", method: "/cost；实际 provider 的官方接口", scope: "session", query: Query::Callback },
    Provider { agent: "opencode", label: "OpenCode", command: "opencode", source: "https://opencode.ai/v2/docs/cli/commands/", method: "opencode stats --json", scope: "local", query: Query::Json { args: &["stats", "--json"], fallback_args: Some(&["stats"]) } },
    Provider { agent: "github-copilot", label: "GitHub Copilot", command: "copilot", source: "https://docs.github.com/en/rest/billing/usage", method: "Billing Usage API", scope: "billing_account", query: Query::Portal },
    Provider { agent: "kiro", label: "Kiro", command: "kiro-cli", source: "https://kiro.dev/docs/cli/reference/slash-commands/", method: "kiro-cli chat --no-interactive /usage", scope: "account", query: json(&["chat", "--no-interactive", "/usage"]) },
    Provider { agent: "droid", label: "Factory Droid", command: "droid", source: "https://docs.factory.ai/api-reference/analytics", method: "GET /api/v1/analytics/cost/me/query", scope: "account", query: Query::Portal },
    Provider { agent: "amp", label: "Amp", command: "amp", source: "https://ampcode.com/docs/pricing", method: "amp usage", scope: "account", query: json(&["usage"]) },
    Provider { agent: "grok", label: "Grok", command: "grok", source: "https://x.ai/build/changelog", method: "/usage；xAI Management API", scope: "account", query: Query::Interactive("/usage") },
    Provider { agent: "hermes", label: "Hermes", command: "hermes", source: "https://hermes-agent.nousresearch.com/docs/reference/slash-commands", method: "/usage Account limits", scope: "account", query: Query::Interactive("/usage") },
    Provider { agent: "kilo", label: "Kilo", command: "kilo", source: "https://kilo.ai/docs/code-with-ai/platforms/cli", method: "kilo profile；余额视图", scope: "account", query: json(&["profile"]) },
    Provider { agent: "qodercli", label: "Qoder CLI", command: "qodercli", source: "https://docs.qoder.com/cli/usage", method: "/usage", scope: "account", query: Query::Interactive("/usage") },
    Provider { agent: "qwen", label: "Qwen Code", command: "qwen", source: "https://qwenlm.github.io/qwen-code-docs/en/users/features/commands/", method: "/stats；实际 provider 的官方接口", scope: "session", query: Query::Callback },
    Provider { agent: "letta", label: "Letta", command: "letta", source: "https://docs.letta.com/platform/cli/slash-commands", method: "letta usage", scope: "account", query: json(&["usage"]) },
    Provider { agent: "maki", label: "Maki", command: "maki", source: "https://maki.sh/docs/token-economy/", method: "/usage；实际 provider 的官方接口", scope: "session", query: Query::Callback },
    Provider { agent: "pi", label: "Pi", command: "pi", source: "https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc.md", method: "get_session_stats；实际 provider 的官方接口", scope: "session", query: Query::Callback },
];

pub(super) fn provider(agent: &str) -> Option<&'static Provider> {
    let agent = agent.trim().to_ascii_lowercase();
    let canonical = match agent.as_str() {
        "claude-code" | "claude code" => "claude",
        "kimi-code" | "kimi code" => "kimi",
        "copilot" | "github copilot" | "githubcopilot" => "github-copilot",
        "qoder" => "qodercli",
        "mastra-code" | "mastra code" => "mastracode",
        "antigravity-cli" | "agy" => "antigravity",
        other => other,
    };
    PROVIDERS.iter().find(|entry| entry.agent == canonical)
}

/// 交互探测使用的斜杠命令：交互型厂商取登记值；claude 的主路径已是官方回调，`/usage` 只在
/// 显式刷新且 `interactive_probe` 开启时作为兜底。
pub(super) fn interactive_fallback(provider: &Provider) -> Option<&'static str> {
    match provider.query {
        Query::Interactive(command) => Some(command),
        Query::Callback if provider.agent == "claude" => Some("/usage"),
        _ => None,
    }
}

/// 该厂商是否只能靠官方回调拿到额度样本：显式刷新不会产生新的额度样本，但仍可能刷新
/// 登录 / 绑定占位（claude 的登录预检就是这样）。claude 开启 `interactive_probe` 后有可回落
/// 的 `/usage` 探测（在稳定探测目录里，需用户信任一次），不再是纯回调。
pub(super) fn callback_only(provider: &Provider, config: &AccountUsageConfig) -> bool {
    matches!(provider.query, Query::Callback)
        && !(provider.agent == "claude" && config.interactive_probe)
}

/// claude 官方 `settings.json` 的 statusLine 是否已接入 herdr 用量回调；文件不存在（全新
/// 安装）算未接入，读不了 / 无法解析 / 含无法识别的 herdr 回调时为 `None`。判据与
/// `integration::usage` 的启用逻辑同源（按本平台命令形态比对，不匹配明文子串）；目录推导与
/// `credential_paths` 同源：`profile_dir` 优先于默认目录。
pub(super) fn claude_statusline_enabled(account: &UsageAccountConfig) -> Option<bool> {
    let dir = account
        .profile_dir
        .clone()
        .or_else(|| crate::integration::claude_dir().ok())?;
    let settings = match std::fs::read_to_string(dir.join("settings.json")) {
        Ok(settings) => settings,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(false),
        Err(_) => return None,
    };
    crate::integration::usage_statusline_enabled(&settings, "claude")
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
        "github-copilot" => Some(IntegrationTarget::Copilot),
        "devin" => Some(IntegrationTarget::Devin),
        "droid" => Some(IntegrationTarget::Droid),
        "kilo" => Some(IntegrationTarget::Kilo),
        "hermes" => Some(IntegrationTarget::Hermes),
        "qodercli" => Some(IntegrationTarget::Qodercli),
        "qwen" => Some(IntegrationTarget::Qwen),
        "cursor" => Some(IntegrationTarget::Cursor),
        "mastracode" => Some(IntegrationTarget::Mastracode),
        "antigravity" => Some(IntegrationTarget::AntigravityCli),
        "grok" => Some(IntegrationTarget::Grok),
        "omp" => Some(IntegrationTarget::Omp),
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
            // TODO: `credentials` 文件名来自 Kimi Code CLI 的本地登录存储，尚未在多个版本上
            // 核实；不存在时戳恒为 None，自愈退化为只看 CLI 指纹。
            if let Some(dir) = dir(crate::integration::kimi_dir()) {
                paths.push(dir.join("credentials"));
            }
        }
        "gemini" => {
            if let Some(dir) = dir(crate::integration::gemini_dir()) {
                paths.push(dir.join("oauth_creds.json"));
                paths.push(dir.join("google_accounts.json"));
            }
        }
        "opencode" => {
            if let Some(dir) = dir(crate::integration::opencode_data_dir()) {
                paths.push(dir.join("auth.json"));
            }
        }
        "grok" => {
            if let Some(dir) = dir(crate::integration::grok_dir()) {
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

    #[test]
    fn registry_covers_all_supported_agents_except_explicitly_excluded_muse() {
        assert_eq!(PROVIDERS.len(), 23);
        for agent in crate::detect::Agent::ALL {
            let label = crate::detect::agent_label(agent);
            if label == "muse" {
                assert!(provider(label).is_none());
            } else {
                assert!(provider(label).is_some(), "missing {label}");
            }
        }
        let unique = PROVIDERS
            .iter()
            .map(|p| p.agent)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), PROVIDERS.len());
        assert!(PROVIDERS.iter().all(|p| p.source.starts_with("https://")));
    }

    #[test]
    fn opencode_stats_has_a_plain_fallback_and_other_json_queries_do_not() {
        let opencode = provider("opencode").unwrap();
        let Query::Json {
            args,
            fallback_args,
        } = opencode.query
        else {
            panic!("opencode 是非交互子命令查询");
        };
        assert_eq!(args, &["stats", "--json"]);
        assert_eq!(
            fallback_args,
            Some(&["stats"][..]),
            "1.17.x 没有 --json：回退到框线表"
        );
        assert_eq!(opencode.scope, "local", "会话统计不是账号额度");
        for agent in ["omp", "kiro", "amp", "kilo", "letta"] {
            let entry = provider(agent).unwrap();
            assert!(
                matches!(
                    entry.query,
                    Query::Json {
                        fallback_args: None,
                        ..
                    }
                ),
                "{agent} 没有回退形态"
            );
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
        assert_eq!(
            interactive_fallback(provider("gemini").unwrap()),
            Some("/stats")
        );
        assert_eq!(interactive_fallback(provider("pi").unwrap()), None);
        assert_eq!(interactive_fallback(provider("antigravity").unwrap()), None);

        let mut config = AccountUsageConfig::default();
        assert!(
            callback_only(claude, &config),
            "默认关闭交互探测：显式刷新拿不到新数据"
        );
        assert!(callback_only(provider("pi").unwrap(), &config));
        assert!(!callback_only(provider("gemini").unwrap(), &config));
        config.interactive_probe = true;
        assert!(
            !callback_only(claude, &config),
            "开启后显式刷新可回落到稳定探测目录里的 /usage（需用户信任该目录一次）"
        );
        assert!(
            callback_only(provider("pi").unwrap(), &config),
            "其它回调型厂商不受影响"
        );
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
            claude_statusline_enabled(&account),
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
            claude_statusline_enabled(&account),
            Some(true),
            "判据按本平台命令形态比对（Windows 是 -EncodedCommand 形态）"
        );
        std::fs::write(
            base.join("settings.json"),
            "{\"statusLine\":{\"type\":\"command\",\"command\":\"python custom.py\"}}",
        )
        .unwrap();
        assert_eq!(claude_statusline_enabled(&account), Some(false));
        std::fs::write(base.join("settings.json"), "{").unwrap();
        assert_eq!(claude_statusline_enabled(&account), None, "无法解析");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn integration_detection_covers_shared_targets_and_only_them() {
        // Providers outside the frozen integration enum must not invent a
        // mapping; the shared ones must match the settings-page detection.
        let unmapped = PROVIDERS
            .iter()
            .map(|p| p.agent)
            .filter(|agent| integration_target_for(agent).is_none())
            .collect::<Vec<_>>();
        assert_eq!(
            unmapped,
            vec!["gemini", "cline", "kiro", "amp", "letta", "maki"],
            "providers without an integration target must stay single-command"
        );
        for provider in PROVIDERS {
            if let Some(target) = integration_target_for(provider.agent) {
                assert_eq!(
                    crate::integration::integration_target_label(target),
                    provider
                        .agent
                        .replace("github-copilot", "copilot")
                        .replace("antigravity", "antigravity-cli")
                );
            }
        }
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
    fn unknown_providers_only_fingerprint_the_cli() {
        let provider = provider("amp").unwrap();
        let account = UsageAccountConfig {
            id: "amp:default".into(),
            agent: "amp".into(),
            ..Default::default()
        };
        assert!(probe_fingerprint(provider, &account).credentials.is_empty());
    }
}
