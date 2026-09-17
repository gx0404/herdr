//! 官方来源清单。Agent 身份与实际计费 provider 分开处理。

#[derive(Clone, Copy)]
pub(super) enum Query {
    Codex,
    Kimi,
    Json(&'static [&'static str]),
    Interactive(&'static str),
    Callback,
    Portal,
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
    Provider { agent: "claude", label: "Claude Code", command: "claude", source: "https://code.claude.com/docs/en/statusline", method: "statusline JSON rate_limits；/usage", scope: "account", query: Query::Interactive("/usage") },
    Provider { agent: "kimi", label: "Kimi Code", command: "kimi", source: "https://www.kimi.com/code/docs/en/kimi-code-cli/reference/server-api.html", method: "GET /api/v1/oauth/usage；/usage", scope: "account", query: Query::Kimi },
    Provider { agent: "gemini", label: "Gemini CLI", command: "gemini", source: "https://geminicli.com/docs/get-started/", method: "/stats（刷新官方配额）", scope: "account", query: Query::Interactive("/stats") },
    Provider { agent: "cursor", label: "Cursor", command: "cursor-agent", source: "https://cursor.com/docs/account/teams/admin-api", method: "Admin API /teams/spend", scope: "organization", query: Query::Portal },
    Provider { agent: "devin", label: "Devin", command: "devin", source: "https://docs.devin.ai/api-reference/v3/consumption/consumption-daily-users", method: "Consumption API", scope: "organization", query: Query::Portal },
    Provider { agent: "antigravity", label: "Antigravity", command: "agy", source: "https://antigravity.google/docs/cli/commands/usage", method: "statusline JSON quota；/usage", scope: "account", query: Query::Callback },
    Provider { agent: "cline", label: "Cline", command: "cline", source: "https://docs.cline.bot/enterprise-solutions/api-reference", method: "GET /api/v1/users/{id}/balance; usages", scope: "account", query: Query::Portal },
    Provider { agent: "omp", label: "Oh My Pi", command: "omp", source: "https://github.com/can1357/oh-my-pi/blob/main/packages/coding-agent/src/commands/usage.ts", method: "omp usage --json", scope: "account", query: Query::Json(&["usage", "--json"]) },
    Provider { agent: "mastracode", label: "Mastra Code", command: "mastracode", source: "https://code.mastra.ai/", method: "/cost；实际 provider 的官方接口", scope: "session", query: Query::Callback },
    Provider { agent: "opencode", label: "OpenCode", command: "opencode", source: "https://opencode.ai/v2/docs/cli/commands/", method: "opencode stats --json", scope: "local", query: Query::Json(&["stats", "--json"]) },
    Provider { agent: "github-copilot", label: "GitHub Copilot", command: "copilot", source: "https://docs.github.com/en/rest/billing/usage", method: "Billing Usage API", scope: "billing_account", query: Query::Portal },
    Provider { agent: "kiro", label: "Kiro", command: "kiro-cli", source: "https://kiro.dev/docs/cli/reference/slash-commands/", method: "kiro-cli chat --no-interactive /usage", scope: "account", query: Query::Json(&["chat", "--no-interactive", "/usage"]) },
    Provider { agent: "droid", label: "Factory Droid", command: "droid", source: "https://docs.factory.ai/api-reference/analytics", method: "GET /api/v1/analytics/cost/me/query", scope: "account", query: Query::Portal },
    Provider { agent: "amp", label: "Amp", command: "amp", source: "https://ampcode.com/docs/pricing", method: "amp usage", scope: "account", query: Query::Json(&["usage"]) },
    Provider { agent: "grok", label: "Grok", command: "grok", source: "https://x.ai/build/changelog", method: "/usage；xAI Management API", scope: "account", query: Query::Interactive("/usage") },
    Provider { agent: "hermes", label: "Hermes", command: "hermes", source: "https://hermes-agent.nousresearch.com/docs/reference/slash-commands", method: "/usage Account limits", scope: "account", query: Query::Interactive("/usage") },
    Provider { agent: "kilo", label: "Kilo", command: "kilo", source: "https://kilo.ai/docs/code-with-ai/platforms/cli", method: "kilo profile；余额视图", scope: "account", query: Query::Json(&["profile"]) },
    Provider { agent: "qodercli", label: "Qoder CLI", command: "qodercli", source: "https://docs.qoder.com/cli/usage", method: "/usage", scope: "account", query: Query::Interactive("/usage") },
    Provider { agent: "qwen", label: "Qwen Code", command: "qwen", source: "https://qwenlm.github.io/qwen-code-docs/en/users/features/commands/", method: "/stats；实际 provider 的官方接口", scope: "session", query: Query::Callback },
    Provider { agent: "letta", label: "Letta", command: "letta", source: "https://docs.letta.com/platform/cli/slash-commands", method: "letta usage", scope: "account", query: Query::Json(&["usage"]) },
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
}
