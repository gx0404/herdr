//! Client-local command snippets: a versioned, atomically written JSON
//! library next to the endpoint catalog, plus the `{{variable}}` rendering
//! and the per-target execution history used by the CLI (and later the UI).

use std::collections::HashSet;
use std::fmt;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

const SNIPPET_LIBRARY_VERSION: u32 = 1;
const MAX_LIBRARY_BYTES: u64 = 1024 * 1024;
const MAX_SNIPPETS: usize = 256;
const MAX_LABEL_BYTES: usize = 128;
const MAX_COMMAND_BYTES: usize = 4 * 1024;
const MAX_DESCRIPTION_BYTES: usize = 512;
const MAX_VARIABLES: usize = 16;
const MAX_VARIABLE_NAME_BYTES: usize = 64;
const MAX_VARIABLE_VALUE_BYTES: usize = 1024;
const MAX_TAGS: usize = 16;
const MAX_TAG_BYTES: usize = 64;
const MAX_HISTORY: usize = 100;
const SNIPPET_ID_BYTES: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct SnippetId(String);

impl SnippetId {
    pub(crate) fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.len() != SNIPPET_ID_BYTES * 2
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("snippet id must be 32 lowercase hexadecimal characters".into());
        }
        Ok(Self(value))
    }

    pub(crate) fn generate() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);

        let sequence = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let digest =
            Sha256::digest(format!("snippet:{}:{now}:{sequence}", std::process::id()).as_bytes());
        Self(
            digest[..SNIPPET_ID_BYTES]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        )
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SnippetId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A saved command template. `variables` declares the `{{name}}` placeholders
/// the command expects; rendering substitutes provided values and refuses to
/// run with unresolved placeholders left.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Snippet {
    pub(crate) id: SnippetId,
    pub(crate) label: String,
    pub(crate) command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) variables: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) tags: Vec<String>,
}

impl Snippet {
    fn validate(&self) -> Result<(), String> {
        SnippetId::parse(self.id.to_string())?;
        validate_snippet_text(&self.label, MAX_LABEL_BYTES, "snippet label", false)?;
        validate_snippet_text(&self.command, MAX_COMMAND_BYTES, "snippet command", false)?;
        if let Some(description) = &self.description {
            validate_snippet_text(
                description,
                MAX_DESCRIPTION_BYTES,
                "snippet description",
                true,
            )?;
        }
        if self.variables.len() > MAX_VARIABLES {
            return Err(format!("snippet allows at most {MAX_VARIABLES} variables"));
        }
        for variable in &self.variables {
            validate_variable_name(variable)?;
        }
        if self.tags.len() > MAX_TAGS {
            return Err(format!("snippet allows at most {MAX_TAGS} tags"));
        }
        for tag in &self.tags {
            validate_snippet_text(tag, MAX_TAG_BYTES, "snippet tag", false)?;
        }
        Ok(())
    }
}

/// One recorded execution target of a snippet run. `machine` is `local` or
/// the saved endpoint profile id; outcomes are per target, never aggregated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnippetExecution {
    pub(crate) snippet_id: SnippetId,
    pub(crate) snippet_label: String,
    pub(crate) machine: String,
    pub(crate) pane_id: String,
    pub(crate) executed_at: u64,
    pub(crate) success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnippetLibrary {
    version: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) snippets: Vec<Snippet>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) history: Vec<SnippetExecution>,
}

impl Default for SnippetLibrary {
    fn default() -> Self {
        Self {
            version: SNIPPET_LIBRARY_VERSION,
            snippets: Vec::new(),
            history: Vec::new(),
        }
    }
}

impl SnippetLibrary {
    pub(crate) fn load() -> Result<Self, String> {
        Self::load_from_path(&snippet_library_path())
    }

    fn load_from_path(path: &Path) -> Result<Self, String> {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => {
                return Err(format!(
                    "failed to open snippet library {}: {error}",
                    path.display()
                ));
            }
        };
        let metadata = file
            .metadata()
            .map_err(|error| format!("failed to inspect snippet library: {error}"))?;
        if metadata.len() > MAX_LIBRARY_BYTES {
            return Err("snippet library exceeds the storage limit".into());
        }
        let mut content = String::new();
        file.take(MAX_LIBRARY_BYTES + 1)
            .read_to_string(&mut content)
            .map_err(|error| format!("failed to read snippet library: {error}"))?;
        if content.len() as u64 > MAX_LIBRARY_BYTES {
            return Err("snippet library exceeds the storage limit".into());
        }
        let library: Self = serde_json::from_str(&content)
            .map_err(|error| format!("stored snippet library is invalid: {error}"))?;
        library.validate()?;
        Ok(library)
    }

    pub(crate) fn store(&self) -> Result<(), String> {
        self.store_to_path(&snippet_library_path())
    }

    fn store_to_path(&self, path: &Path) -> Result<(), String> {
        self.validate()?;
        let content = serde_json::to_vec_pretty(self)
            .map_err(|error| format!("failed to encode snippet library: {error}"))?;
        super::catalog::store_private_json(path, &content, "snippet library")
    }

    pub(crate) fn add_snippet(
        &mut self,
        label: impl Into<String>,
        command: impl Into<String>,
        description: Option<String>,
        variables: Vec<String>,
        tags: Vec<String>,
    ) -> Result<SnippetId, String> {
        if self.snippets.len() >= MAX_SNIPPETS {
            return Err(format!("at most {MAX_SNIPPETS} snippets can be saved"));
        }
        let snippet = Snippet {
            id: SnippetId::generate(),
            label: label.into(),
            command: command.into(),
            description,
            variables,
            tags,
        };
        snippet.validate()?;
        let id = snippet.id.clone();
        self.snippets.push(snippet);
        Ok(id)
    }

    /// Finds a snippet by exact id, then by unique case-sensitive label.
    pub(crate) fn resolve(&self, selector: &str) -> Result<Option<&Snippet>, String> {
        if let Some(snippet) = self
            .snippets
            .iter()
            .find(|snippet| snippet.id.as_str() == selector)
        {
            return Ok(Some(snippet));
        }
        let mut matches = self
            .snippets
            .iter()
            .filter(|snippet| snippet.label == selector);
        let Some(first) = matches.next() else {
            return Ok(None);
        };
        if matches.next().is_some() {
            return Err(format!("snippet label {selector} is ambiguous; use its id"));
        }
        Ok(Some(first))
    }

    pub(crate) fn remove_snippet(&mut self, id: &SnippetId) -> bool {
        let previous_len = self.snippets.len();
        self.snippets.retain(|snippet| &snippet.id != id);
        self.snippets.len() != previous_len
    }

    /// Replaces a snippet's editable fields in place, keeping its id so run
    /// history stays correlated. Unknown ids are an error, never a silent add.
    pub(crate) fn update_snippet(
        &mut self,
        id: &SnippetId,
        label: impl Into<String>,
        command: impl Into<String>,
        description: Option<String>,
        variables: Vec<String>,
        tags: Vec<String>,
    ) -> Result<bool, String> {
        let Some(snippet) = self.snippets.iter_mut().find(|snippet| &snippet.id == id) else {
            return Ok(false);
        };
        let updated = Snippet {
            id: id.clone(),
            label: label.into(),
            command: command.into(),
            description,
            variables,
            tags,
        };
        updated.validate()?;
        *snippet = updated;
        Ok(true)
    }

    /// Appends one record per executed target, keeping only the most recent
    /// [`MAX_HISTORY`] entries for the history view. Recorded text is
    /// sanitized so a hostile or garbled target cannot make the library fail
    /// validation on the next load.
    pub(crate) fn record_executions(&mut self, executions: Vec<SnippetExecution>) {
        self.history
            .extend(executions.into_iter().map(|mut execution| {
                execution.snippet_label =
                    sanitize_history_text(execution.snippet_label, MAX_LABEL_BYTES);
                execution.machine = sanitize_history_text(execution.machine, MAX_LABEL_BYTES);
                execution.pane_id = sanitize_history_text(execution.pane_id, MAX_LABEL_BYTES);
                if let Some(error) = execution.error.take() {
                    execution.error = Some(sanitize_history_text(error, MAX_DESCRIPTION_BYTES));
                }
                execution
            }));
        if self.history.len() > MAX_HISTORY {
            let overflow = self.history.len() - MAX_HISTORY;
            self.history.drain(..overflow);
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.version != SNIPPET_LIBRARY_VERSION {
            return Err(format!(
                "unsupported snippet library version {}; expected {SNIPPET_LIBRARY_VERSION}",
                self.version
            ));
        }
        if self.snippets.len() > MAX_SNIPPETS {
            return Err(format!(
                "snippet library contains more than {MAX_SNIPPETS} snippets"
            ));
        }
        let mut ids = HashSet::new();
        for snippet in &self.snippets {
            snippet.validate()?;
            if !ids.insert(snippet.id.clone()) {
                return Err(format!("duplicate snippet id {}", snippet.id));
            }
        }
        if self.history.len() > MAX_HISTORY {
            return Err(format!(
                "snippet library contains more than {MAX_HISTORY} history records"
            ));
        }
        for execution in &self.history {
            SnippetId::parse(execution.snippet_id.to_string())?;
            validate_snippet_text(
                &execution.snippet_label,
                MAX_LABEL_BYTES,
                "snippet history label",
                false,
            )?;
            validate_snippet_text(
                &execution.machine,
                MAX_LABEL_BYTES,
                "snippet history machine",
                false,
            )?;
            validate_snippet_text(
                &execution.pane_id,
                MAX_LABEL_BYTES,
                "snippet history pane id",
                false,
            )?;
            if let Some(error) = &execution.error {
                validate_snippet_text(error, MAX_DESCRIPTION_BYTES, "snippet history error", true)?;
            }
        }
        Ok(())
    }
}

/// Substitutes `{{name}}` placeholders with the provided values. Placeholders
/// without a value, an unclosed `{{`, and values containing control
/// characters are errors: a rendered snippet always stays a single line.
pub(crate) fn render_snippet_command(
    command: &str,
    values: &[(String, String)],
) -> Result<String, String> {
    for (name, value) in values {
        if value.chars().any(char::is_control) {
            return Err(format!(
                "value for snippet variable {name} must not contain control characters"
            ));
        }
        if value.len() > MAX_VARIABLE_VALUE_BYTES {
            return Err(format!(
                "value for snippet variable {name} must be at most {MAX_VARIABLE_VALUE_BYTES} bytes"
            ));
        }
    }
    let mut rendered = String::with_capacity(command.len());
    let mut rest = command;
    while let Some(start) = rest.find("{{") {
        rendered.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];
        let Some(end) = after_open.find("}}") else {
            return Err("snippet command contains an unclosed '{{' placeholder".into());
        };
        let name = after_open[..end].trim();
        if name.is_empty() || name.contains(['{', '}']) {
            return Err("snippet command contains a malformed placeholder".into());
        }
        let Some((_, value)) = values.iter().find(|(key, _)| key == name) else {
            return Err(format!(
                "snippet variable {name} needs a value; pass --var {name}=VALUE"
            ));
        };
        rendered.push_str(value);
        rest = &after_open[end + 2..];
    }
    rendered.push_str(rest);
    Ok(rendered)
}

fn sanitize_history_text(value: String, max_bytes: usize) -> String {
    let filtered: String = value.chars().filter(|ch| !ch.is_control()).collect();
    if filtered.len() <= max_bytes {
        return filtered;
    }
    let mut end = max_bytes;
    while !filtered.is_char_boundary(end) {
        end -= 1;
    }
    filtered[..end].to_string()
}

fn validate_variable_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("snippet variable name cannot be empty".into());
    }
    if name.len() > MAX_VARIABLE_NAME_BYTES
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
    {
        return Err(format!(
            "snippet variable name must be at most {MAX_VARIABLE_NAME_BYTES} bytes of letters, digits, '_' or '-'"
        ));
    }
    Ok(())
}

fn validate_snippet_text(
    value: &str,
    max_bytes: usize,
    what: &str,
    allow_blank: bool,
) -> Result<(), String> {
    if !allow_blank && value.trim().is_empty() {
        return Err(format!("{what} cannot be empty"));
    }
    if value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err(format!(
            "{what} must be at most {max_bytes} bytes and contain no control characters"
        ));
    }
    Ok(())
}

pub(crate) fn snippet_library_path() -> PathBuf {
    crate::config::state_dir()
        .join("client")
        .join("snippets.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("herdr-snippets-{}-{name}", std::process::id()))
            .join("snippets.json")
    }

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn library_roundtrip_preserves_snippets_and_history() {
        let path = path("roundtrip");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let mut library = SnippetLibrary::default();
        let id = library
            .add_snippet(
                "Deploy",
                "kubectl rollout restart deploy/{{name}}",
                Some("restart a deployment".into()),
                vec!["name".into()],
                vec!["ops".into()],
            )
            .unwrap();
        library.record_executions(vec![SnippetExecution {
            snippet_id: id.clone(),
            snippet_label: "Deploy".into(),
            machine: "local".into(),
            pane_id: "w1:p1".into(),
            executed_at: 1_700_000_000,
            success: true,
            error: None,
        }]);
        library.store_to_path(&path).unwrap();

        let loaded = SnippetLibrary::load_from_path(&path).unwrap();
        assert_eq!(loaded, library);
        let encoded = std::fs::read_to_string(&path).unwrap();
        assert!(encoded.contains("\"version\": 1"), "{encoded}");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn library_rejects_newer_versions_and_control_characters() {
        let path = path("version");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"version": 2, "snippets": []}"#).unwrap();
        assert!(SnippetLibrary::load_from_path(&path)
            .unwrap_err()
            .contains("unsupported snippet library version 2"));

        let mut library = SnippetLibrary::default();
        assert!(library
            .add_snippet("bad", "echo one\necho two", None, Vec::new(), Vec::new())
            .unwrap_err()
            .contains("no control characters"));
        assert!(library
            .add_snippet("bad", "ok", None, vec!["not a name".into()], Vec::new())
            .unwrap_err()
            .contains("variable name"));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn resolve_matches_id_then_unique_label() {
        let mut library = SnippetLibrary::default();
        let id = library
            .add_snippet("deploy", "echo hi", None, Vec::new(), Vec::new())
            .unwrap();
        assert_eq!(library.resolve(id.as_str()).unwrap().unwrap().id, id);
        assert_eq!(library.resolve("deploy").unwrap().unwrap().id, id);
        assert!(library.resolve("missing").unwrap().is_none());

        library
            .add_snippet("deploy", "echo other", None, Vec::new(), Vec::new())
            .unwrap();
        assert!(library.resolve("deploy").unwrap_err().contains("ambiguous"));
        assert!(library.remove_snippet(&id));
        assert!(!library.remove_snippet(&id));
    }

    #[test]
    fn render_substitutes_variables_and_rejects_leftovers() {
        let rendered = render_snippet_command(
            "kubectl -n {{ns}} rollout restart deploy/{{ name }} && echo done",
            &vars(&[("ns", "prod"), ("name", "api")]),
        )
        .unwrap();
        assert_eq!(
            rendered,
            "kubectl -n prod rollout restart deploy/api && echo done"
        );

        assert!(render_snippet_command("echo {{missing}}", &vars(&[]))
            .unwrap_err()
            .contains("--var missing=VALUE"));
        assert!(
            render_snippet_command("echo {{open", &vars(&[("open", "x")]))
                .unwrap_err()
                .contains("unclosed")
        );
        assert!(render_snippet_command("echo {{}}", &vars(&[]))
            .unwrap_err()
            .contains("malformed"));
        assert!(
            render_snippet_command("echo {{v}}", &vars(&[("v", "a\nb")]))
                .unwrap_err()
                .contains("control characters")
        );
    }

    #[test]
    fn render_leaves_text_without_placeholders_untouched() {
        assert_eq!(
            render_snippet_command("echo } and } alone", &vars(&[])).unwrap(),
            "echo } and } alone"
        );
    }

    #[test]
    fn history_is_capped_at_the_most_recent_records() {
        let mut library = SnippetLibrary::default();
        let id = library
            .add_snippet("s", "echo hi", None, Vec::new(), Vec::new())
            .unwrap();
        let records = (0..MAX_HISTORY + 20)
            .map(|index| SnippetExecution {
                snippet_id: id.clone(),
                snippet_label: "s".into(),
                machine: "local".into(),
                pane_id: format!("w1:p{index}"),
                executed_at: index as u64,
                success: true,
                error: None,
            })
            .collect();
        library.record_executions(records);
        assert_eq!(library.history.len(), MAX_HISTORY);
        assert_eq!(library.history[0].pane_id, "w1:p20");
        library.validate().unwrap();
    }

    #[test]
    fn recorded_history_is_sanitized_to_survive_validation() {
        let mut library = SnippetLibrary::default();
        let id = library
            .add_snippet("s", "echo hi", None, Vec::new(), Vec::new())
            .unwrap();
        library.record_executions(vec![SnippetExecution {
            snippet_id: id,
            snippet_label: "s".into(),
            machine: "local".into(),
            pane_id: "w1:p1\ninjected".into(),
            executed_at: 1,
            success: false,
            error: Some("x".repeat(MAX_DESCRIPTION_BYTES * 2)),
        }]);
        library.validate().unwrap();
        let record = &library.history[0];
        assert_eq!(record.pane_id, "w1:p1injected");
        assert_eq!(
            record.error.as_deref().map(str::len),
            Some(MAX_DESCRIPTION_BYTES)
        );
    }
}
