//! OpenSSH client config (`ssh_config`) parsing and catalog import planning.
//!
//! Parsing follows OpenSSH semantics where a static import can observe them:
//!
//! - Only full-line comments exist: `#` starts a comment as the first
//!   non-whitespace character of a line; it is never stripped from values.
//! - Directive names are case-insensitive; keyword and arguments are separated
//!   by whitespace or one optional `=`.
//! - Double-quoted arguments keep their whitespace; `\"` and `\\` escape
//!   inside quotes. `RemoteCommand` takes the rest of the line as its value.
//! - `Host` takes multiple patterns with `*`/`?` wildcards and `!` negation,
//!   matched case-insensitively.
//! - Per directive the first value in file order wins, except `IdentityFile`,
//!   where every occurrence appends. Directives before the first `Host` or
//!   `Match` line form an implicit global block that matches every host (and,
//!   being first, wins over later blocks).
//! - `Include` splices the included files' items at the directive position;
//!   top-level directives of an included file therefore inherit the including
//!   block context. Relative include paths resolve against the root config's
//!   directory (OpenSSH resolves them against `~/.ssh` for user configs), `~`
//!   expands to the user's home directory, and glob patterns never cross a
//!   path separator. Includes nest at most 16 deep with a cycle guard and
//!   file/byte budgets; missing matches are ignored like OpenSSH does.
//! - `Match` blocks cannot be evaluated statically, so their content —
//!   directives and nested `Include`s alike — is dropped rather than
//!   misattributed to neighbouring `Host` blocks.
//!
//! Only the directives a saved machine profile can carry are extracted;
//! everything else is ignored silently. `~user` expansion is not supported
//! (left literal). Values that fail to parse are skipped with a warning
//! instead of aborting the whole import the way `ssh` would.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::client::endpoint::{
    ProfileId, ProxyJumpHop, SavedSshEndpoint, SshProfileOptions, StrictHostKeyChecking,
};

const MAX_INCLUDE_DEPTH: usize = 16;
const MAX_INCLUDE_FILES: usize = 256;
const MAX_TOTAL_BYTES: u64 = 1024 * 1024;
const MAX_INCLUDE_PATTERN_BYTES: usize = 1024;

/// A non-fatal parse problem: the offending directive (or include) was
/// skipped and import continues with the rest of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SshConfigWarning {
    pub(crate) origin: PathBuf,
    pub(crate) line: usize,
    pub(crate) message: String,
}

/// Parsed ssh client config: an ordered list of `Host` blocks flattened
/// across `Include` expansion.
#[derive(Debug, Default)]
pub(crate) struct SshConfig {
    blocks: Vec<HostBlock>,
    warnings: Vec<SshConfigWarning>,
}

#[derive(Debug)]
struct HostBlock {
    /// The implicit top-of-file block matches every alias. A `Host` line
    /// without patterns is rejected at parse time, so an empty `patterns`
    /// list on a non-global block never occurs.
    global: bool,
    patterns: Vec<HostPattern>,
    directives: Vec<ConfigDirective>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HostPattern {
    negated: bool,
    /// As written; matching and deduplication lowercase on demand.
    pattern: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfigDirective {
    key: DirectiveKey,
    value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectiveKey {
    HostName,
    Port,
    User,
    IdentityFile,
    IdentitiesOnly,
    IdentityAgent,
    ProxyJump,
    ForwardAgent,
    RemoteCommand,
    ServerAliveInterval,
    ServerAliveCountMax,
    ControlPersist,
    StrictHostKeyChecking,
}

impl DirectiveKey {
    fn parse(keyword: &str) -> Option<Self> {
        Some(match keyword.to_ascii_lowercase().as_str() {
            "hostname" => Self::HostName,
            "port" => Self::Port,
            "user" => Self::User,
            "identityfile" => Self::IdentityFile,
            "identitiesonly" => Self::IdentitiesOnly,
            "identityagent" => Self::IdentityAgent,
            "proxyjump" => Self::ProxyJump,
            "forwardagent" => Self::ForwardAgent,
            "remotecommand" => Self::RemoteCommand,
            "serveraliveinterval" => Self::ServerAliveInterval,
            "serveralivecountmax" => Self::ServerAliveCountMax,
            "controlpersist" => Self::ControlPersist,
            "stricthostkeychecking" => Self::StrictHostKeyChecking,
            _ => return None,
        })
    }
}

/// One parse-stream entry; `Include` is expanded into the stream before
/// blocks are folded, so block assembly never sees it.
#[derive(Debug)]
enum ConfigItem {
    StartHost(Vec<HostPattern>),
    /// A `Match` line: everything until the next `Host`/`Match` is dropped.
    StartMatch,
    Directive(ConfigDirective),
    Include {
        origin: PathBuf,
        line: usize,
        patterns: Vec<String>,
    },
}

impl SshConfig {
    /// Loads and parses a config file, resolving `Include` relative to the
    /// file's own directory and `~` against the platform home directory.
    pub(crate) fn load(path: &Path) -> io::Result<Self> {
        Self::load_with_home(path, crate::platform::ssh_config_home_dir().as_deref())
    }

    /// Home is explicit so tests never touch the real environment.
    pub(crate) fn load_with_home(path: &Path, home: Option<&Path>) -> io::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let mut state = ExpandState {
            home,
            base_dir: path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf(),
            visited: HashSet::new(),
            files_read: 1,
            bytes_read: content.len() as u64,
        };
        state.visited.insert(canonical_or_original(path));
        let mut warnings = Vec::new();
        let items = parse_file_items(&content, path, &mut warnings);
        let items = expand_includes(items, 0, &mut state, &mut warnings);
        let mut config = Self::default();
        config.fold_items(items);
        config.warnings = warnings;
        Ok(config)
    }

    /// Parses one in-memory file without include resolution (unit fixtures).
    #[cfg(test)]
    fn parse_str(content: &str) -> Self {
        let mut warnings = Vec::new();
        let origin = PathBuf::from("<fixture>");
        let items = parse_file_items(content, &origin, &mut warnings);
        let mut config = Self::default();
        config.fold_items(items);
        config.warnings = warnings;
        config
    }

    pub(crate) fn warnings(&self) -> &[SshConfigWarning] {
        &self.warnings
    }

    fn fold_items(&mut self, items: Vec<ConfigItem>) {
        // The implicit global block sits at position 0 and matches every host.
        let mut current = HostBlock {
            global: true,
            patterns: Vec::new(),
            directives: Vec::new(),
        };
        let mut in_match = false;
        for item in items {
            match item {
                ConfigItem::StartHost(patterns) => {
                    self.blocks.push(std::mem::replace(
                        &mut current,
                        HostBlock {
                            global: false,
                            patterns,
                            directives: Vec::new(),
                        },
                    ));
                    in_match = false;
                }
                ConfigItem::StartMatch => {
                    in_match = true;
                }
                ConfigItem::Directive(directive) => {
                    if !in_match {
                        current.directives.push(directive);
                    }
                }
                ConfigItem::Include { .. } => {
                    // Includes are expanded before folding; the in-memory
                    // test parser skips expansion and simply drops them.
                }
            }
        }
        self.blocks.push(current);
    }

    /// Concrete and wildcard host aliases in first-seen order, written case
    /// preserved. Case-insensitive duplicates collapse to their first
    /// occurrence.
    pub(crate) fn discover(&self) -> Vec<DiscoveredHost> {
        let mut seen = HashSet::new();
        let mut hosts = Vec::new();
        for block in &self.blocks {
            for pattern in &block.patterns {
                if pattern.negated {
                    continue;
                }
                if seen.insert(pattern.pattern.to_ascii_lowercase()) {
                    hosts.push(DiscoveredHost {
                        alias: pattern.pattern.clone(),
                        wildcard: is_wildcard_pattern(&pattern.pattern),
                    });
                }
            }
        }
        hosts
    }

    /// Effective directives for one alias: every matching block applies in
    /// file order, first value wins, `IdentityFile` appends.
    pub(crate) fn resolve(&self, alias: &str) -> ResolvedSshHost {
        let alias = alias.to_ascii_lowercase();
        let mut resolved = ResolvedSshHost::default();
        for block in &self.blocks {
            if !block.matches(&alias) {
                continue;
            }
            for directive in &block.directives {
                resolved.apply(directive);
            }
        }
        resolved
    }

    /// Import drafts for every discovered host, in discovery order.
    pub(crate) fn import_candidates(&self) -> Vec<ImportCandidate> {
        self.discover()
            .iter()
            .map(|host| self.import_candidate(host))
            .collect()
    }

    /// Import draft for one alias, or `None` when the alias is not declared
    /// by any `Host` block (used by `machine add --from-config`).
    pub(crate) fn import_candidate_named(&self, alias: &str) -> Option<ImportCandidate> {
        let host = self
            .discover()
            .into_iter()
            .find(|host| host.alias.eq_ignore_ascii_case(alias))?;
        Some(self.import_candidate(&host))
    }

    fn import_candidate(&self, host: &DiscoveredHost) -> ImportCandidate {
        let resolved = self.resolve(&host.alias);
        let mut notes = Vec::new();
        let target = match &resolved.hostname {
            Some(hostname) => expand_hostname_tokens(
                hostname,
                &host.alias,
                resolved.port,
                host.wildcard,
                &mut notes,
            ),
            None => host.alias.clone(),
        };
        let strict_host_key_checking = resolved.strict_host_key_checking.and_then(|value| {
            match value.as_str() {
                "ask" => Some(StrictHostKeyChecking::Ask),
                "accept-new" => Some(StrictHostKeyChecking::AcceptNew),
                "yes" | "true" | "on" => Some(StrictHostKeyChecking::Yes),
                other => {
                    notes.push(format!(
                        "StrictHostKeyChecking {other} cannot be stored on a saved machine and was dropped"
                    ));
                    None
                }
            }
        });
        let hops = match &resolved.proxy_jump {
            Some(value) if value.eq_ignore_ascii_case("none") => Vec::new(),
            Some(value) => value
                .split(',')
                .map(str::trim)
                .filter(|hop| !hop.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            None => Vec::new(),
        };
        // A hop maps to a profile reference only when it is exactly a bare
        // alias of the same import batch; user@ and :port decorations would be
        // lost by a profile reference, so those hops stay literal targets.
        let hop_aliases = hops
            .iter()
            .map(|hop| bare_hop_alias(hop).map(str::to_owned))
            .collect::<Vec<_>>();
        let server_alive_interval = resolved.server_alive_interval.and_then(|value| {
            u16::try_from(value).ok().or_else(|| {
                notes.push(format!(
                    "ServerAliveInterval {value} exceeds 65535 and was dropped"
                ));
                None
            })
        });
        let server_alive_count_max = resolved.server_alive_count_max.and_then(|value| {
            u16::try_from(value).ok().or_else(|| {
                notes.push(format!(
                    "ServerAliveCountMax {value} exceeds 65535 and was dropped"
                ));
                None
            })
        });
        ImportCandidate {
            label: host.alias.clone(),
            target,
            wildcard: host.wildcard,
            options: SshProfileOptions {
                user: resolved.user,
                port: resolved.port,
                identity_file: resolved.identity_file,
                identities_only: resolved.identities_only,
                identity_agent: resolved.identity_agent,
                strict_host_key_checking,
                proxy_jump: hops
                    .iter()
                    .map(|hop| ProxyJumpHop::Target(hop.clone()))
                    .collect(),
                forward_agent: resolved.forward_agent,
                server_alive_interval,
                server_alive_count_max,
                control_persist: resolved.control_persist,
                remote_command: resolved.remote_command,
                ..SshProfileOptions::default()
            },
            notes,
            hop_aliases,
        }
    }
}

/// One host alias discovered in the config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiscoveredHost {
    pub(crate) alias: String,
    pub(crate) wildcard: bool,
}

/// Effective ssh-config values for one alias. Only directives a saved
/// machine profile can carry are extracted; the rest is ignored by design.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ResolvedSshHost {
    pub(crate) hostname: Option<String>,
    pub(crate) port: Option<u16>,
    pub(crate) user: Option<String>,
    pub(crate) identity_file: Vec<String>,
    pub(crate) identities_only: Option<bool>,
    pub(crate) identity_agent: Option<String>,
    pub(crate) proxy_jump: Option<String>,
    pub(crate) forward_agent: Option<bool>,
    pub(crate) remote_command: Option<String>,
    pub(crate) server_alive_interval: Option<u32>,
    pub(crate) server_alive_count_max: Option<u32>,
    pub(crate) control_persist: Option<String>,
    pub(crate) strict_host_key_checking: Option<String>,
}

impl ResolvedSshHost {
    fn apply(&mut self, directive: &ConfigDirective) {
        let value = directive.value.clone();
        match directive.key {
            DirectiveKey::HostName => {
                self.hostname.get_or_insert(value);
            }
            DirectiveKey::Port => {
                if let Ok(port) = directive.value.parse::<u16>() {
                    self.port.get_or_insert(port);
                }
            }
            DirectiveKey::User => {
                self.user.get_or_insert(value);
            }
            DirectiveKey::IdentityFile => self.identity_file.push(value),
            DirectiveKey::IdentitiesOnly => {
                if let Some(flag) = parse_ssh_bool(&directive.value) {
                    self.identities_only.get_or_insert(flag);
                }
            }
            DirectiveKey::IdentityAgent => {
                self.identity_agent.get_or_insert(value);
            }
            DirectiveKey::ProxyJump => {
                self.proxy_jump.get_or_insert(value);
            }
            DirectiveKey::ForwardAgent => {
                if let Some(flag) = parse_ssh_bool(&directive.value) {
                    self.forward_agent.get_or_insert(flag);
                }
            }
            DirectiveKey::RemoteCommand => {
                self.remote_command.get_or_insert(value);
            }
            DirectiveKey::ServerAliveInterval => {
                if let Ok(interval) = directive.value.parse::<u32>() {
                    self.server_alive_interval.get_or_insert(interval);
                }
            }
            DirectiveKey::ServerAliveCountMax => {
                if let Ok(count) = directive.value.parse::<u32>() {
                    self.server_alive_count_max.get_or_insert(count);
                }
            }
            DirectiveKey::ControlPersist => {
                self.control_persist.get_or_insert(value);
            }
            DirectiveKey::StrictHostKeyChecking => {
                self.strict_host_key_checking
                    .get_or_insert_with(|| directive.value.to_ascii_lowercase());
            }
        }
    }
}

impl HostBlock {
    fn matches(&self, alias: &str) -> bool {
        if self.global {
            return true;
        }
        let mut matched = false;
        for pattern in &self.patterns {
            if ssh_host_pattern_matches(&pattern.pattern, alias) {
                if pattern.negated {
                    return false;
                }
                matched = true;
            }
        }
        matched
    }
}

/// Catalog import draft for one config host, before batch-level planning.
#[derive(Debug)]
pub(crate) struct ImportCandidate {
    pub(crate) label: String,
    pub(crate) target: String,
    pub(crate) options: SshProfileOptions,
    pub(crate) wildcard: bool,
    /// Settings that could not be represented on a saved machine and were
    /// dropped; surfaced to the user per host.
    pub(crate) notes: Vec<String>,
    /// Parallel to `options.proxy_jump`: `Some(alias)` when the hop is a bare
    /// alias that may resolve to a same-batch profile reference.
    hop_aliases: Vec<Option<String>>,
}

/// One candidate ready to add, in dependency order: a candidate only ever
/// references profiles at earlier positions of [`ImportPlan::ready`].
#[derive(Debug)]
pub(crate) struct PlannedImport {
    pub(crate) label: String,
    pub(crate) target: String,
    pub(crate) options: SshProfileOptions,
    pub(crate) notes: Vec<String>,
    /// (proxy_jump hop index, ready-list index) pairs to rewrite as
    /// `ProxyJumpHop::Profile` once the dependency's id is known.
    hop_refs: Vec<(usize, usize)>,
    /// Raw hop text parallel to `options.proxy_jump`, restored when a
    /// referenced profile was skipped or failed to save.
    hop_fallback: Vec<String>,
}

impl PlannedImport {
    /// Options with same-batch proxy hops resolved to profile ids. `ids` is
    /// parallel to [`ImportPlan::ready`]; a `None` entry (not selected or
    /// failed) keeps the hop a literal target, which ssh still resolves
    /// through the user's own config at connect time.
    pub(crate) fn options_with_resolved_hops(
        &self,
        ids: &[Option<ProfileId>],
    ) -> SshProfileOptions {
        let mut options = self.options.clone();
        for (hop, dependency) in &self.hop_refs {
            options.proxy_jump[*hop] = match ids.get(*dependency) {
                Some(Some(id)) => ProxyJumpHop::Profile(id.clone()),
                _ => ProxyJumpHop::Target(self.hop_fallback[*hop].clone()),
            };
        }
        options
    }
}

/// Result of batch-level import planning.
#[derive(Debug, Default)]
pub(crate) struct ImportPlan {
    /// Candidates to add, in dependency order (a referenced jump profile
    /// always precedes its dependents).
    pub(crate) ready: Vec<PlannedImport>,
    /// Candidates not imported, in original discovery order, each with a
    /// human-readable reason.
    pub(crate) skipped: Vec<ImportSkip>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportSkip {
    pub(crate) label: String,
    pub(crate) reason: String,
}

/// Plans a batch import against the current catalog. Duplicates are skipped
/// and reported rather than overwritten:
///
/// - wildcard patterns unless `include_wildcards` is set;
/// - labels already used by a saved machine (exact match);
/// - hosts whose resolved target, user, and port all match a saved machine;
/// - repeats of either inside the batch itself.
///
/// Skip reasons come from the shared i18n tables (`cli_errors.import_skip_*`)
/// so the CLI and the TUI wizard show identical wording.
pub(crate) fn plan_import(
    candidates: Vec<ImportCandidate>,
    existing: &[SavedSshEndpoint],
    include_wildcards: bool,
) -> ImportPlan {
    let t = &crate::i18n::texts().cli_errors;
    let mut plan = ImportPlan::default();
    let mut survivors = Vec::new();
    let mut batch_labels = HashSet::new();
    let mut batch_targets = HashSet::new();
    for candidate in candidates {
        if candidate.wildcard && !include_wildcards {
            plan.skipped.push(ImportSkip {
                label: candidate.label,
                reason: t.import_skip_wildcard.into(),
            });
            continue;
        }
        if existing
            .iter()
            .any(|profile| profile.label == candidate.label)
        {
            plan.skipped.push(ImportSkip {
                label: candidate.label,
                reason: t.import_skip_label_exists.into(),
            });
            continue;
        }
        let target_key = (
            candidate.target.to_ascii_lowercase(),
            candidate.options.user.clone(),
            candidate.options.port,
        );
        if existing.iter().any(|profile| {
            profile.target.eq_ignore_ascii_case(&candidate.target)
                && profile.user == candidate.options.user
                && profile.port == candidate.options.port
        }) {
            plan.skipped.push(ImportSkip {
                label: candidate.label,
                reason: crate::i18n::fill(
                    t.import_skip_target_exists_fmt,
                    &[("target", &candidate.target)],
                ),
            });
            continue;
        }
        if !batch_labels.insert(candidate.label.to_ascii_lowercase()) {
            plan.skipped.push(ImportSkip {
                label: candidate.label,
                reason: t.import_skip_batch_label.into(),
            });
            continue;
        }
        if !batch_targets.insert(target_key) {
            plan.skipped.push(ImportSkip {
                label: candidate.label,
                reason: crate::i18n::fill(
                    t.import_skip_batch_target_fmt,
                    &[("target", &candidate.target)],
                ),
            });
            continue;
        }
        survivors.push(candidate);
    }

    // Dependency order: a jump-host profile must exist before profiles that
    // reference it. DFS post-order; a back edge (cycle) keeps that hop a
    // literal target instead of recursing forever.
    let index_by_alias: HashMap<String, usize> = survivors
        .iter()
        .enumerate()
        .map(|(index, candidate)| (candidate.label.to_ascii_lowercase(), index))
        .collect();
    let mut state = vec![0_u8; survivors.len()];
    let mut order = Vec::with_capacity(survivors.len());
    // (dependent, proxy_jump hop index, dependency) triples to resolve.
    let mut kept_edges: Vec<(usize, usize, usize)> = Vec::new();
    for index in 0..survivors.len() {
        visit_import_candidate(
            index,
            &survivors,
            &index_by_alias,
            &mut state,
            &mut order,
            &mut kept_edges,
        );
    }
    let planned_positions: HashMap<usize, usize> = order
        .iter()
        .enumerate()
        .map(|(position, original)| (*original, position))
        .collect();
    let mut survivors: Vec<Option<ImportCandidate>> = survivors.into_iter().map(Some).collect();
    plan.ready = order
        .into_iter()
        .map(|original| {
            let candidate = survivors[original]
                .take()
                .unwrap_or_else(|| unreachable!("dfs order lists every survivor once"));
            let hop_fallback = candidate
                .options
                .proxy_jump
                .iter()
                .map(|hop| match hop {
                    ProxyJumpHop::Target(target) => target.clone(),
                    ProxyJumpHop::Profile(id) => id.to_string(),
                })
                .collect();
            let hop_refs = kept_edges
                .iter()
                .filter(|(dependent, _, _)| *dependent == original)
                .filter_map(|(_, hop, dependency)| {
                    Some((*hop, *planned_positions.get(dependency)?))
                })
                .collect();
            PlannedImport {
                label: candidate.label,
                target: candidate.target,
                options: candidate.options,
                notes: candidate.notes,
                hop_refs,
                hop_fallback,
            }
        })
        .collect();
    plan
}

fn visit_import_candidate(
    index: usize,
    candidates: &[ImportCandidate],
    index_by_alias: &HashMap<String, usize>,
    state: &mut [u8],
    order: &mut Vec<usize>,
    kept_edges: &mut Vec<(usize, usize, usize)>,
) {
    if state[index] != 0 {
        return;
    }
    state[index] = 1;
    for (hop, alias) in candidates[index].hop_aliases.iter().enumerate() {
        let Some(alias) = alias else { continue };
        let Some(&dependency) = index_by_alias.get(&alias.to_ascii_lowercase()) else {
            continue;
        };
        if dependency == index {
            continue; // self-references stay literal targets
        }
        match state[dependency] {
            0 => {
                visit_import_candidate(
                    dependency,
                    candidates,
                    index_by_alias,
                    state,
                    order,
                    kept_edges,
                );
                kept_edges.push((index, hop, dependency));
            }
            1 => {} // cycle: keep the hop a literal target
            _ => kept_edges.push((index, hop, dependency)),
        }
    }
    state[index] = 2;
    order.push(index);
}

/// Effective target for an alias: OpenSSH expands `%h` to the hostname given
/// on the command line and `%p` to the port, so a config that maps short
/// aliases through a dynamic `HostName` keeps working with the imported
/// target. Wildcard aliases keep every token literal. Other tokens stay
/// untouched and are noted.
fn expand_hostname_tokens(
    hostname: &str,
    alias: &str,
    port: Option<u16>,
    wildcard: bool,
    notes: &mut Vec<String>,
) -> String {
    if wildcard || !hostname.contains('%') {
        return hostname.to_string();
    }
    let port = port.unwrap_or(22).to_string();
    let mut expanded = String::with_capacity(hostname.len());
    let mut chars = hostname.chars();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            expanded.push(ch);
            continue;
        }
        match chars.next() {
            Some('%') => expanded.push('%'),
            Some('h') => expanded.push_str(alias),
            Some('p') => expanded.push_str(&port),
            Some(token) => {
                expanded.push('%');
                expanded.push(token);
                notes.push(format!(
                    "HostName token %{token} is not expanded during import and was kept literal"
                ));
            }
            None => expanded.push('%'),
        }
    }
    expanded
}

/// A ProxyJump hop maps to a profile reference only when it is exactly a bare
/// alias: no `user@` prefix, no `:port` suffix, no wildcards.
fn bare_hop_alias(hop: &str) -> Option<&str> {
    if hop.contains('@') || hop.contains(':') || is_wildcard_pattern(hop) {
        return None;
    }
    Some(hop)
}

fn is_wildcard_pattern(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?')
}

fn parse_ssh_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "yes" | "true" | "on" => Some(true),
        "no" | "false" | "off" => Some(false),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Line parsing
// ---------------------------------------------------------------------------

fn parse_file_items(
    content: &str,
    origin: &Path,
    warnings: &mut Vec<SshConfigWarning>,
) -> Vec<ConfigItem> {
    let mut items = Vec::new();
    for (index, raw_line) in content.lines().enumerate() {
        let line_number = index + 1;
        let warning = |message: String| SshConfigWarning {
            origin: origin.to_path_buf(),
            line: line_number,
            message,
        };
        let line = raw_line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let keyword_end = line
            .find(|ch: char| ch.is_whitespace() || ch == '=')
            .unwrap_or(line.len());
        let keyword = &line[..keyword_end];
        let mut rest = line[keyword_end..].trim_start();
        if let Some(stripped) = rest.strip_prefix('=') {
            rest = stripped.trim_start();
        }
        match keyword.to_ascii_lowercase().as_str() {
            "host" => match tokenize(rest) {
                Ok(tokens) if !tokens.is_empty() => {
                    let patterns = tokens
                        .iter()
                        .map(|token| match token.strip_prefix('!') {
                            Some(pattern) if !pattern.is_empty() => HostPattern {
                                negated: true,
                                pattern: pattern.to_owned(),
                            },
                            _ => HostPattern {
                                negated: false,
                                pattern: token.clone(),
                            },
                        })
                        .collect();
                    items.push(ConfigItem::StartHost(patterns));
                }
                Ok(_) => warnings.push(warning("Host line without patterns ignored".into())),
                Err(message) => warnings.push(warning(message)),
            },
            "match" => items.push(ConfigItem::StartMatch),
            "include" => match tokenize(rest) {
                Ok(patterns) if !patterns.is_empty() => {
                    items.push(ConfigItem::Include {
                        origin: origin.to_path_buf(),
                        line: line_number,
                        patterns,
                    });
                }
                Ok(_) => warnings.push(warning("Include without paths ignored".into())),
                Err(message) => warnings.push(warning(message)),
            },
            _ => {
                let Some(key) = DirectiveKey::parse(keyword) else {
                    continue; // unsupported directive: ignored by design
                };
                if key == DirectiveKey::RemoteCommand {
                    let command = rest.trim();
                    if command.is_empty() {
                        warnings.push(warning("RemoteCommand without a command ignored".into()));
                        continue;
                    }
                    items.push(ConfigItem::Directive(ConfigDirective {
                        key,
                        value: strip_outer_quotes(command).to_owned(),
                    }));
                    continue;
                }
                match tokenize(rest) {
                    Ok(tokens) => match tokens.split_first() {
                        Some((value, extra)) => {
                            if !extra.is_empty() {
                                warnings.push(warning(format!(
                                    "{keyword}: extra arguments after '{value}' ignored"
                                )));
                            }
                            items.push(ConfigItem::Directive(ConfigDirective {
                                key,
                                value: value.clone(),
                            }));
                        }
                        None => {
                            warnings.push(warning(format!("{keyword} without a value ignored")))
                        }
                    },
                    Err(message) => warnings.push(warning(message)),
                }
            }
        }
    }
    items
}

/// Whitespace tokenizer with double-quote support. Inside quotes `\"` and
/// `\\` escape; a quote opened and never closed is an error (OpenSSH rejects
/// the line too).
fn tokenize(rest: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut has_content = false;
    let mut chars = rest.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                quoted = !quoted;
                has_content = true;
            }
            '\\' if quoted => match chars.next() {
                Some(escaped @ ('"' | '\\')) => current.push(escaped),
                Some(other) => {
                    current.push('\\');
                    current.push(other);
                }
                None => current.push('\\'),
            },
            ch if ch.is_whitespace() && !quoted => {
                if has_content {
                    tokens.push(std::mem::take(&mut current));
                    has_content = false;
                }
            }
            ch => {
                current.push(ch);
                has_content = true;
            }
        }
    }
    if quoted {
        return Err("unterminated quoted argument".into());
    }
    if has_content {
        tokens.push(current);
    }
    Ok(tokens)
}

fn strip_outer_quotes(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(value)
}

// ---------------------------------------------------------------------------
// Include expansion
// ---------------------------------------------------------------------------

struct ExpandState<'a> {
    home: Option<&'a Path>,
    /// Root config's directory; OpenSSH resolves relative includes against
    /// the user config directory, not against each nested file's directory.
    base_dir: PathBuf,
    visited: HashSet<PathBuf>,
    files_read: usize,
    bytes_read: u64,
}

fn expand_includes(
    items: Vec<ConfigItem>,
    depth: usize,
    state: &mut ExpandState,
    warnings: &mut Vec<SshConfigWarning>,
) -> Vec<ConfigItem> {
    let mut expanded = Vec::with_capacity(items.len());
    for item in items {
        let ConfigItem::Include {
            origin,
            line,
            patterns,
        } = item
        else {
            expanded.push(item);
            continue;
        };
        let warning = |message: String| SshConfigWarning {
            origin: origin.clone(),
            line,
            message,
        };
        for pattern in &patterns {
            if pattern.len() > MAX_INCLUDE_PATTERN_BYTES {
                warnings.push(warning(
                    "Include pattern is too long and was ignored".into(),
                ));
                continue;
            }
            for path in expand_include_pattern(pattern, state) {
                let canonical = canonical_or_original(&path);
                if !state.visited.insert(canonical) {
                    continue; // include cycle: already read once
                }
                if depth + 1 >= MAX_INCLUDE_DEPTH || state.files_read >= MAX_INCLUDE_FILES {
                    warnings.push(warning(format!(
                        "Include nesting limit reached; {} skipped",
                        path.display()
                    )));
                    continue;
                }
                let Ok(metadata) = path.metadata() else {
                    continue;
                };
                if state.bytes_read + metadata.len() > MAX_TOTAL_BYTES {
                    warnings.push(warning(format!(
                        "Include size budget exceeded; {} skipped",
                        path.display()
                    )));
                    continue;
                }
                let Ok(content) = std::fs::read_to_string(&path) else {
                    warnings.push(warning(format!(
                        "Include could not be read; {} skipped",
                        path.display()
                    )));
                    continue;
                };
                state.files_read += 1;
                state.bytes_read += content.len() as u64;
                let nested = parse_file_items(&content, &path, warnings);
                expanded.extend(expand_includes(nested, depth + 1, state, warnings));
            }
        }
    }
    expanded
}

fn expand_include_pattern(pattern: &str, state: &ExpandState) -> Vec<PathBuf> {
    let expanded = expand_home_prefix(pattern, state.home);
    let path = if expanded.is_absolute() {
        expanded
    } else {
        state.base_dir.join(expanded)
    };
    if !path.to_string_lossy().contains(['*', '?', '[']) {
        return if path.is_file() {
            vec![path]
        } else {
            Vec::new()
        };
    }
    glob_expand(&path)
}

/// `~` and `~/` (also `~\`) expand to the user's home; `~user` expansion is
/// not supported and stays literal.
fn expand_home_prefix(pattern: &str, home: Option<&Path>) -> PathBuf {
    let rest = if pattern == "~" {
        Some("")
    } else {
        pattern
            .strip_prefix("~/")
            .or_else(|| pattern.strip_prefix("~\\"))
    };
    match (rest, home) {
        (Some(""), Some(home)) => home.to_path_buf(),
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(pattern),
    }
}

/// Expands one glob path component-wise; `*`, `?`, and `[class]` match within
/// a single component and never cross a separator. Dotfiles only match when
/// the pattern component starts with `.` (glob(3) semantics).
fn glob_expand(pattern: &Path) -> Vec<PathBuf> {
    let mut frontier = vec![PathBuf::new()];
    for component in pattern.components() {
        let mut next = Vec::new();
        match component {
            Component::Prefix(prefix) => {
                for base in &frontier {
                    next.push(base.join(prefix.as_os_str()));
                }
            }
            Component::RootDir | Component::CurDir | Component::ParentDir => {
                for base in &frontier {
                    next.push(base.join(component.as_os_str()));
                }
            }
            Component::Normal(part) => {
                let part = part.to_string_lossy();
                let globby = part.contains(['*', '?', '[']);
                for base in &frontier {
                    if globby {
                        let Ok(entries) = std::fs::read_dir(base) else {
                            continue;
                        };
                        for entry in entries.flatten() {
                            let name = entry.file_name();
                            let Some(name) = name.to_str() else { continue };
                            if glob_component_matches(&part, name) {
                                next.push(entry.path());
                            }
                        }
                    } else {
                        let candidate = base.join(part.as_ref());
                        if candidate.exists() {
                            next.push(candidate);
                        }
                    }
                }
            }
        }
        frontier = next;
    }
    let mut matches: Vec<PathBuf> = frontier.into_iter().filter(|path| path.is_file()).collect();
    matches.sort();
    matches.dedup();
    matches
}

fn glob_component_matches(pattern: &str, name: &str) -> bool {
    if name.starts_with('.') && !pattern.starts_with('.') {
        return false;
    }
    if cfg!(windows) {
        return glob_match(
            &pattern.to_ascii_lowercase(),
            &name.to_ascii_lowercase(),
            true,
        );
    }
    glob_match(pattern, name, true)
}

/// OpenSSH host patterns support `*` and `?` only and match case-insensitively.
pub(crate) fn ssh_host_pattern_matches(pattern: &str, name: &str) -> bool {
    glob_match(
        &pattern.to_ascii_lowercase(),
        &name.to_ascii_lowercase(),
        false,
    )
}

/// Memoized glob matcher. `classes` enables `[...]` (path globs); host
/// patterns treat brackets literally like OpenSSH.
fn glob_match(pattern: &str, name: &str, classes: bool) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let mut memo = HashMap::new();
    glob_match_at(&pattern, &name, 0, 0, classes, &mut memo)
}

fn glob_match_at(
    pattern: &[char],
    name: &[char],
    pi: usize,
    ni: usize,
    classes: bool,
    memo: &mut HashMap<(usize, usize), bool>,
) -> bool {
    if let Some(result) = memo.get(&(pi, ni)) {
        return *result;
    }
    let result = glob_match_step(pattern, name, pi, ni, classes, memo);
    memo.insert((pi, ni), result);
    result
}

fn glob_match_step(
    pattern: &[char],
    name: &[char],
    pi: usize,
    ni: usize,
    classes: bool,
    memo: &mut HashMap<(usize, usize), bool>,
) -> bool {
    if pi == pattern.len() {
        return ni == name.len();
    }
    match pattern[pi] {
        '*' => {
            (ni..=name.len()).any(|next| glob_match_at(pattern, name, pi + 1, next, classes, memo))
        }
        '?' => ni < name.len() && glob_match_at(pattern, name, pi + 1, ni + 1, classes, memo),
        '[' if classes => match parse_glob_class(&pattern[pi..]) {
            Some((class_matches, consumed)) => {
                ni < name.len()
                    && class_matches(name[ni])
                    && glob_match_at(pattern, name, pi + consumed, ni + 1, classes, memo)
            }
            None => {
                ni < name.len()
                    && name[ni] == '['
                    && glob_match_at(pattern, name, pi + 1, ni + 1, classes, memo)
            }
        },
        literal => {
            ni < name.len()
                && name[ni] == literal
                && glob_match_at(pattern, name, pi + 1, ni + 1, classes, memo)
        }
    }
}

/// Parses `[abc]`, `[a-z]`, `[!a-z]`, `[^a-z]` at the start of `pattern`.
/// Returns (matcher, chars consumed) or `None` for an unterminated class,
/// which glob(3) treats as a literal `[`.
fn parse_glob_class(pattern: &[char]) -> Option<(impl Fn(char) -> bool, usize)> {
    let mut index = 1;
    let negated = matches!(pattern.get(index), Some('!') | Some('^'));
    if negated {
        index += 1;
    }
    let mut chars = Vec::new();
    let mut ranges = Vec::new();
    // A `]` immediately after `[` or `[!` is a literal member.
    if matches!(pattern.get(index), Some(']')) {
        chars.push(']');
        index += 1;
    }
    while let Some(&ch) = pattern.get(index) {
        if ch == ']' {
            let matches_char = move |candidate: char| {
                let member = chars.contains(&candidate)
                    || ranges
                        .iter()
                        .any(|(start, end)| *start <= candidate && candidate <= *end);
                member != negated
            };
            return Some((matches_char, index + 1));
        }
        if ch == '-'
            && chars.last().is_some()
            && pattern.get(index + 1).is_some_and(|next| *next != ']')
        {
            let start = chars.pop().unwrap_or('-');
            index += 1;
            if let Some(&end) = pattern.get(index) {
                ranges.push((start, end));
                index += 1;
                continue;
            }
        }
        chars.push(ch);
        index += 1;
    }
    None
}

fn canonical_or_original(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(content: &str, alias: &str) -> ResolvedSshHost {
        SshConfig::parse_str(content).resolve(alias)
    }

    #[test]
    fn first_value_wins_across_blocks_and_host_star_fallback_applies() {
        let config = r#"
ServerAliveInterval 10

Host web
    HostName web.internal
    User deploy
    User ignored

Host web db
    Port 2222

Host *
    ServerAliveInterval 30
    ForwardAgent no
"#;
        let web = resolve(config, "web");
        assert_eq!(web.hostname.as_deref(), Some("web.internal"));
        assert_eq!(web.user.as_deref(), Some("deploy"));
        assert_eq!(web.port, Some(2222));
        // The implicit global block is first and therefore wins over `Host *`.
        assert_eq!(web.server_alive_interval, Some(10));
        assert_eq!(web.forward_agent, Some(false));
        let db = resolve(config, "db");
        assert_eq!(db.port, Some(2222));
        assert_eq!(db.hostname, None);
        assert_eq!(db.server_alive_interval, Some(10));
    }

    #[test]
    fn host_matching_is_case_insensitive_with_negation() {
        let config = r#"
Host * !bastion
    User deploy

Host BASTION
    User root
"#;
        assert_eq!(resolve(config, "WEB").user.as_deref(), Some("deploy"));
        let bastion = resolve(config, "Bastion");
        assert_eq!(bastion.user.as_deref(), Some("root"));
    }

    #[test]
    fn separators_comments_and_quotes_parse_like_openssh() {
        let config = r#"
# a comment line
Host=web
    HostName=web.internal
    Port = 2222
    IdentityFile "~/.ssh/id build"
    RemoteCommand tmux new -A # trailing text is part of the command
    User "deploy ops"
"#;
        let web = resolve(config, "web");
        assert_eq!(web.hostname.as_deref(), Some("web.internal"));
        assert_eq!(web.port, Some(2222));
        assert_eq!(web.identity_file, vec!["~/.ssh/id build".to_string()]);
        assert_eq!(
            web.remote_command.as_deref(),
            Some("tmux new -A # trailing text is part of the command")
        );
        assert_eq!(web.user.as_deref(), Some("deploy ops"));
    }

    #[test]
    fn quoted_remote_command_loses_only_its_outer_quotes() {
        let web = resolve("Host web\n  RemoteCommand \"tmux new -A\"\n", "web");
        assert_eq!(web.remote_command.as_deref(), Some("tmux new -A"));
    }

    #[test]
    fn multiple_identity_files_append_in_order() {
        let config = r#"
Host web
    IdentityFile ~/.ssh/first
    IdentityFile ~/.ssh/second
    IdentitiesOnly yes

Host *
    IdentityFile ~/.ssh/fallback
"#;
        let web = resolve(config, "web");
        assert_eq!(
            web.identity_file,
            vec![
                "~/.ssh/first".to_string(),
                "~/.ssh/second".to_string(),
                "~/.ssh/fallback".to_string()
            ]
        );
        assert_eq!(web.identities_only, Some(true));
    }

    #[test]
    fn match_block_content_is_dropped_not_misattributed() {
        let config = r#"
Host web
    HostName web.internal

Match user root
    User root
    HostName evil.example

Host db
    HostName db.internal
"#;
        let web = resolve(config, "web");
        assert_eq!(web.hostname.as_deref(), Some("web.internal"));
        assert_eq!(web.user, None);
        let db = resolve(config, "db");
        assert_eq!(db.hostname.as_deref(), Some("db.internal"));
        assert_eq!(db.user, None);
    }

    #[test]
    fn discovery_skips_negated_patterns_and_dedupes_case_insensitively() {
        let config = r#"
Host web WEB !secret *.internal
Host db
Host web
"#;
        let hosts = SshConfig::parse_str(config).discover();
        assert_eq!(
            hosts,
            vec![
                DiscoveredHost {
                    alias: "web".into(),
                    wildcard: false
                },
                DiscoveredHost {
                    alias: "*.internal".into(),
                    wildcard: true
                },
                DiscoveredHost {
                    alias: "db".into(),
                    wildcard: false
                },
            ]
        );
    }

    #[test]
    fn malformed_values_warn_and_are_skipped() {
        let config = r#"
Host web
    Port not-a-port
    HostName web.internal
    User
    IdentityFile ~/.ssh/unterminated
"#;
        let config = SshConfig::parse_str(config);
        let web = config.resolve("web");
        assert_eq!(web.port, None);
        assert_eq!(web.hostname.as_deref(), Some("web.internal"));
        assert!(
            config
                .warnings()
                .iter()
                .any(|warning| warning.message.contains("without a value")),
            "{:?}",
            config.warnings()
        );
    }

    #[test]
    fn unterminated_quote_warns_and_drops_the_line() {
        let config =
            SshConfig::parse_str("Host web\n  IdentityFile \"never closed\n  User deploy\n");
        assert_eq!(config.resolve("web").user.as_deref(), Some("deploy"));
        assert!(config
            .warnings()
            .iter()
            .any(|warning| warning.message.contains("unterminated quoted argument")));
    }

    #[test]
    fn candidate_maps_config_to_profile_fields() {
        let config = r#"
Host web
    HostName web.internal
    Port 2222
    User deploy
    IdentityFile ~/.ssh/web
    IdentityFile ~/.ssh/fallback
    IdentitiesOnly yes
    IdentityAgent ~/.ssh/agent.sock
    ProxyJump bastion
    ForwardAgent yes
    ServerAliveInterval 45
    ServerAliveCountMax 3
    ControlPersist 10m
    StrictHostKeyChecking accept-new
    RemoteCommand tmux attach
"#;
        let candidates = SshConfig::parse_str(config).import_candidates();
        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];
        assert_eq!(candidate.label, "web");
        assert_eq!(candidate.target, "web.internal");
        assert!(!candidate.wildcard);
        assert!(candidate.notes.is_empty(), "{:?}", candidate.notes);
        assert_eq!(
            candidate.options,
            SshProfileOptions {
                user: Some("deploy".into()),
                port: Some(2222),
                identity_file: vec!["~/.ssh/web".into(), "~/.ssh/fallback".into()],
                identities_only: Some(true),
                identity_agent: Some("~/.ssh/agent.sock".into()),
                strict_host_key_checking: Some(StrictHostKeyChecking::AcceptNew),
                proxy_jump: vec![ProxyJumpHop::Target("bastion".into())],
                forward_agent: Some(true),
                server_alive_interval: Some(45),
                server_alive_count_max: Some(3),
                control_persist: Some("10m".into()),
                remote_command: Some("tmux attach".into()),
                ..SshProfileOptions::default()
            }
        );
    }

    #[test]
    fn candidate_notes_unsupported_values_and_keeps_importing() {
        let config = r#"
Host web
    HostName %h.ops.example
    StrictHostKeyChecking no
    ServerAliveInterval 70000
"#;
        let candidates = SshConfig::parse_str(config).import_candidates();
        let candidate = &candidates[0];
        assert_eq!(
            candidate.target, "web.ops.example",
            "%h expands to the alias"
        );
        assert_eq!(candidate.options.strict_host_key_checking, None);
        assert_eq!(candidate.options.server_alive_interval, None);
        assert_eq!(candidate.notes.len(), 2, "{:?}", candidate.notes);
    }

    #[test]
    fn proxy_jump_none_and_chains_map_to_hops() {
        let config = r#"
Host web
    ProxyJump bastion,ops@relay:2222

Host direct
    ProxyJump none
"#;
        let candidates = SshConfig::parse_str(config).import_candidates();
        assert_eq!(
            candidates[0].options.proxy_jump,
            vec![
                ProxyJumpHop::Target("bastion".into()),
                ProxyJumpHop::Target("ops@relay:2222".into()),
            ]
        );
        assert!(candidates[1].options.proxy_jump.is_empty());
    }

    #[test]
    fn plan_skips_wildcards_and_duplicates_against_the_catalog() {
        let config = r#"
Host web
    HostName web.internal
    User deploy
    Port 2222

Host dup-label
Host dup-target
    HostName web.internal

Host *.internal
"#;
        let existing = vec![
            SavedSshEndpoint::new("dup-label", "other.example", "default").unwrap(),
            SavedSshEndpoint::new("other", "web.internal", "default").unwrap(),
        ];
        // The existing "other" profile targets web.internal without user/port,
        // so it must not shadow the richer config entry; "dup-target" resolves
        // to the exact same target and is skipped instead.
        let candidates = SshConfig::parse_str(config).import_candidates();
        let plan = plan_import(candidates, &existing, false);
        assert_eq!(plan.ready.len(), 1, "{:?}", plan.skipped);
        assert_eq!(plan.ready[0].label, "web");
        let t = &crate::i18n::texts().cli_errors;
        let reasons: Vec<(&str, String)> = plan
            .skipped
            .iter()
            .map(|skip| (skip.label.as_str(), skip.reason.clone()))
            .collect();
        assert_eq!(
            reasons,
            vec![
                ("dup-label", t.import_skip_label_exists.to_owned()),
                (
                    "dup-target",
                    crate::i18n::fill(
                        t.import_skip_target_exists_fmt,
                        &[("target", "web.internal")]
                    )
                ),
                ("*.internal", t.import_skip_wildcard.to_owned()),
            ]
        );
    }

    #[test]
    fn plan_orders_jump_profiles_before_dependents_and_resolves_references() {
        let config = r#"
Host web
    ProxyJump bastion

Host api
    ProxyJump bastion

Host bastion
    HostName bastion.internal
"#;
        let candidates = SshConfig::parse_str(config).import_candidates();
        let plan = plan_import(candidates, &[], false);
        let order: Vec<&str> = plan
            .ready
            .iter()
            .map(|planned| planned.label.as_str())
            .collect();
        assert_eq!(order, vec!["bastion", "web", "api"]);
        let ids: Vec<Option<ProfileId>> = plan
            .ready
            .iter()
            .map(|_| Some(ProfileId::generate()))
            .collect();
        let web = &plan.ready[1];
        let options = web.options_with_resolved_hops(&ids);
        assert_eq!(
            options.proxy_jump,
            vec![ProxyJumpHop::Profile(ids[0].clone().unwrap())]
        );
        // A missing dependency falls back to the literal hop target.
        let options = web.options_with_resolved_hops(&[None, None, None]);
        assert_eq!(
            options.proxy_jump,
            vec![ProxyJumpHop::Target("bastion".into())]
        );
    }

    #[test]
    fn plan_breaks_proxy_jump_cycles_without_recursing() {
        let config = r#"
Host a
    ProxyJump b

Host b
    ProxyJump a
"#;
        let candidates = SshConfig::parse_str(config).import_candidates();
        let plan = plan_import(candidates, &[], false);
        assert_eq!(plan.ready.len(), 2);
        let ids: Vec<Option<ProfileId>> = plan
            .ready
            .iter()
            .map(|_| Some(ProfileId::generate()))
            .collect();
        let resolved: Vec<Vec<ProxyJumpHop>> = plan
            .ready
            .iter()
            .map(|planned| planned.options_with_resolved_hops(&ids).proxy_jump)
            .collect();
        // One direction resolves to a profile reference, the other keeps the
        // literal alias so resolution never recurses.
        let references = resolved
            .iter()
            .flatten()
            .filter(|hop| matches!(hop, ProxyJumpHop::Profile(_)))
            .count();
        let targets = resolved
            .iter()
            .flatten()
            .filter(|hop| matches!(hop, ProxyJumpHop::Target(_)))
            .count();
        assert_eq!((references, targets), (1, 1), "{resolved:?}");
    }

    #[test]
    fn plan_keeps_decorated_and_unknown_hops_literal() {
        let config = r#"
Host web
    ProxyJump ops@bastion:2222,elsewhere

Host bastion
"#;
        let candidates = SshConfig::parse_str(config).import_candidates();
        let plan = plan_import(candidates, &[], false);
        let ids: Vec<Option<ProfileId>> = plan
            .ready
            .iter()
            .map(|_| Some(ProfileId::generate()))
            .collect();
        let web = plan
            .ready
            .iter()
            .find(|planned| planned.label == "web")
            .unwrap();
        assert_eq!(
            web.options_with_resolved_hops(&ids).proxy_jump,
            vec![
                ProxyJumpHop::Target("ops@bastion:2222".into()),
                ProxyJumpHop::Target("elsewhere".into()),
            ]
        );
    }

    // ---------------------------------------------------------------
    // Include expansion (real temporary files)
    // ---------------------------------------------------------------

    fn fixture_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "herdr-ssh-config-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn include_splices_files_and_keeps_first_value_wins_order() {
        let root = fixture_root("include");
        std::fs::create_dir_all(root.join("conf.d")).unwrap();
        std::fs::write(
            root.join("conf.d").join("10-web.conf"),
            "Host web\n  HostName web.from-include\n  User deploy\n",
        )
        .unwrap();
        std::fs::write(
            root.join("conf.d").join("20-extra.conf"),
            "Host web\n  User ignored\n  Port 2222\n",
        )
        .unwrap();
        std::fs::write(
            root.join("config"),
            "Host *\n  ServerAliveInterval 30\nInclude conf.d/*.conf\nHost db\n  HostName db.internal\n",
        )
        .unwrap();
        let config = SshConfig::load_with_home(&root.join("config"), None).unwrap();
        let web = config.resolve("web");
        assert_eq!(web.hostname.as_deref(), Some("web.from-include"));
        assert_eq!(web.user.as_deref(), Some("deploy"));
        assert_eq!(web.port, Some(2222));
        assert_eq!(web.server_alive_interval, Some(30));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn include_expands_tilde_and_missing_globs_are_ignored() {
        let root = fixture_root("tilde");
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(
            home.join(".ssh").join("extra"),
            "Host web\n  User from-tilde\n",
        )
        .unwrap();
        std::fs::write(
            root.join("config"),
            "Include ~/.ssh/extra ~/.ssh/absent-*\nHost web\n  HostName web.internal\n",
        )
        .unwrap();
        let config = SshConfig::load_with_home(&root.join("config"), Some(&home)).unwrap();
        assert_eq!(
            config.resolve("web").user.as_deref(),
            Some("from-tilde"),
            "the include precedes the Host block, so its User wins"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn include_cycles_and_depth_are_bounded() {
        let root = fixture_root("cycle");
        std::fs::write(root.join("a"), "Include b\nHost a\n  User a\n").unwrap();
        std::fs::write(root.join("b"), "Include a\nHost b\n  User b\n").unwrap();
        let config = SshConfig::load_with_home(&root.join("a"), None).unwrap();
        assert_eq!(config.resolve("a").user.as_deref(), Some("a"));
        assert_eq!(config.resolve("b").user.as_deref(), Some("b"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn include_inside_a_host_block_attributes_to_that_block() {
        let root = fixture_root("nested-context");
        std::fs::write(
            root.join("web.conf"),
            "User deploy\nHost inner\n  HostName inner.internal\n",
        )
        .unwrap();
        std::fs::write(
            root.join("config"),
            "Host web\n  Include web.conf\nHost db\n  HostName db.internal\n",
        )
        .unwrap();
        let config = SshConfig::load_with_home(&root.join("config"), None).unwrap();
        // Top-level directives of the included file inherit the including
        // Host block's context (textual splice).
        assert_eq!(config.resolve("web").user.as_deref(), Some("deploy"));
        assert_eq!(config.resolve("db").user, None);
        assert_eq!(
            config.resolve("inner").hostname.as_deref(),
            Some("inner.internal")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn include_globs_do_not_match_dotfiles_unless_asked() {
        let root = fixture_root("dotfiles");
        std::fs::create_dir_all(root.join("conf.d")).unwrap();
        std::fs::write(root.join("conf.d").join(".hidden"), "Host hidden\n").unwrap();
        std::fs::write(root.join("conf.d").join("shown"), "Host shown\n").unwrap();
        std::fs::write(root.join("config"), "Include conf.d/*\n").unwrap();
        let config = SshConfig::load_with_home(&root.join("config"), None).unwrap();
        let aliases: Vec<String> = config
            .discover()
            .into_iter()
            .map(|host| host.alias)
            .collect();
        assert_eq!(aliases, vec!["shown".to_string()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---------------------------------------------------------------
    // Matchers
    // ---------------------------------------------------------------

    #[test]
    fn host_patterns_support_star_and_question_mark_only() {
        assert!(ssh_host_pattern_matches("web*", "web-1"));
        assert!(ssh_host_pattern_matches("web-?", "web-1"));
        assert!(!ssh_host_pattern_matches("web-?", "web-12"));
        assert!(
            ssh_host_pattern_matches("[web]", "[web]"),
            "brackets stay literal"
        );
        assert!(!ssh_host_pattern_matches("[web]", "w"));
        assert!(ssh_host_pattern_matches("WEB*", "web-1"));
    }

    #[test]
    fn path_globs_support_classes_and_ranges() {
        assert!(glob_component_matches("*.conf", "10-web.conf"));
        assert!(glob_component_matches("[0-9]*.conf", "10-web.conf"));
        assert!(!glob_component_matches("[!0-9]*.conf", "10-web.conf"));
        assert!(glob_component_matches("[^a]bc", "bbc"));
        assert!(glob_component_matches("[]a]x", "]x"));
        assert!(glob_component_matches("[a-]x", "-x"));
        assert!(!glob_component_matches("*", ".hidden"));
        assert!(glob_component_matches(".*", ".hidden"));
        assert!(
            glob_component_matches("[", "["),
            "unterminated class is literal"
        );
    }
}
