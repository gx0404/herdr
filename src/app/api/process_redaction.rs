//! `pane.process_info` 输出层的凭据打码（RL12）。
//!
//! 能连上 socket 的调用方都能查任何窗格的前台进程，而命令行里常带认证头、token、
//! 口令。进程采集（`platform` / `detect`）保持原样——检测要看完整 argv；这里只在
//! 把结果交给调用方之前，把疑似凭据的值换成 `[REDACTED]`：程序名、选项名与参数
//! 位置原样保留，调用方仍看得出跑的是什么。覆盖：
//!
//! - 凭据类选项的值：`--token abc`、`--token=abc`、`--api-key abc`、`-password=abc`；
//! - 敏感请求头：`-H 'Authorization: …'`、`--header Authorization: …`、
//!   `-HX-Api-Key: …`，保留 `Bearer` / `Basic` 等认证方案名；
//! - 独立出现的 `Bearer <token>`；
//! - 凭据类环境变量赋值：`OPENAI_API_KEY=…`、`GITHUB_TOKEN=…`；
//! - URL 里的口令与凭据类查询参数：`https://user:pass@host`、`?api_key=…`；
//! - 整段命令行形态的参数（`sh -c '…'`）按以上规则递归处理。
//!
//! 只认形态、不认程序：`-p<口令>` 这类短选项的含义随程序而异，不在覆盖范围内。

/// 打码后的占位值。
const REDACTED: &str = "[REDACTED]";

/// 参数里套命令行（`sh -c 'sh -c …'`）最多递归处理的层数。
const MAX_NESTING: usize = 3;

/// 名字以这些词结尾即视为凭据（不分大小写，`-` 与 `_` 等价）。
const CREDENTIAL_SUFFIXES: [&str; 10] = [
    "token",
    "secret",
    "password",
    "passwd",
    "passphrase",
    "apikey",
    "credential",
    "credentials",
    "cookie",
    "signature",
];

/// 这些词要独立成词（整个名字，或前面是 `_`）才算凭据：`--api-key`、`DB_PASS`、
/// `--auth` 算，`MONKEY`、`--bypass` 不算。
const CREDENTIAL_WORDS: [&str; 4] = ["key", "pass", "auth", "sig"];

/// 敏感请求头里保留、不打码的认证方案名。
const AUTH_SCHEMES: [&str; 6] = ["bearer", "basic", "token", "digest", "negotiate", "ntlm"];

/// 前台进程的 argv 与 cmdline 打码。argv[0]（程序本身）原样保留。
pub(super) fn redact_command(
    argv: Option<Vec<String>>,
    cmdline: Option<String>,
) -> (Option<Vec<String>>, Option<String>) {
    let redacted_argv = argv.as_deref().map(|argv| {
        let mut words = argv.to_vec();
        redact_words(&mut words, 1, 0);
        words
    });
    let cmdline = cmdline.map(
        |cmdline| match (argv.as_deref(), redacted_argv.as_deref()) {
            // Unix 的 cmdline 就是 argv 用空格拼起来的：直接拼打码后的 argv。拼起来
            // 之后已分不清参数边界（带空格的值会被切开），不能对它重新切词。
            (Some(original), Some(redacted)) if cmdline == original.join(" ") => redacted.join(" "),
            // Windows 的 cmdline 是进程自己的原始命令行：按 CommandLineToArgvW 的引号
            // 规则切词、原位替换要打码的词。
            _ => redact_line(&cmdline, 1, 0, Quotes::Windows),
        },
    );
    (redacted_argv, cmdline)
}

/// 上一个词对当前词的约定。
#[derive(Clone, Copy)]
enum Pending {
    None,
    /// 上一个词是凭据类选项（`--token`、`--api-key` …）：当前词是它的值。
    Value,
    /// 上一个词是 `-H` / `--header`：当前词是 `名字: 值` 形式的请求头。
    Header,
    /// 上一个词是 `Bearer`，或值被 shell 拆到后面去的敏感头名（`Authorization:`）：
    /// 当前词若是认证方案名就保留、接着等下一个，否则它就是凭据本身。
    Credential,
}

/// 从下标 `first` 起逐词打码，原位替换。
fn redact_words(words: &mut [String], first: usize, depth: usize) {
    let mut pending = Pending::None;
    for word in words.iter_mut().skip(first) {
        let (redacted, next) = redact_word(word, pending, depth);
        if let Some(redacted) = redacted {
            *word = redacted;
        }
        pending = next;
    }
}

/// 打码一个词：返回替换值（不用打码时为 `None`）与它对下一个词的约定。
fn redact_word(word: &str, pending: Pending, depth: usize) -> (Option<String>, Pending) {
    // 值的位置上出现选项，说明前一个词只是开关：约定作废，按选项处理。
    if looks_like_option(word) {
        return redact_option(word, depth);
    }
    match pending {
        Pending::Value => return (Some(REDACTED.to_owned()), Pending::None),
        Pending::Header => return redact_header(word, depth),
        Pending::Credential if is_auth_scheme(word) => return (None, Pending::Credential),
        Pending::Credential => return (Some(REDACTED.to_owned()), Pending::None),
        Pending::None => {}
    }
    if let Some((name, value)) = split_assignment(word) {
        return (redact_assignment(name, value, depth), Pending::None);
    }
    if word.eq_ignore_ascii_case("bearer") || ends_with_sensitive_header_name(word) {
        return (None, Pending::Credential);
    }
    (redact_text(word, depth), Pending::None)
}

/// `-x` / `--xx` 形式的选项；单独的 `-`（标准输入）与负数不算。
fn looks_like_option(word: &str) -> bool {
    word.len() > 1 && word.starts_with('-') && !word[1..].starts_with(|c: char| c.is_ascii_digit())
}

fn redact_option(word: &str, depth: usize) -> (Option<String>, Pending) {
    if word == "--" {
        return (None, Pending::None);
    }
    if word == "-H" || word == "--header" {
        return (None, Pending::Header);
    }
    // curl 允许 `-H` 紧贴值：`-HAuthorization: …`。
    if !word.starts_with("--") {
        if let Some(header) = word.strip_prefix("-H") {
            let (redacted, next) = redact_header(header, depth);
            return (redacted.map(|header| format!("-H{header}")), next);
        }
    }
    match word.split_once('=') {
        Some(("--header", header)) => {
            let (redacted, next) = redact_header(header, depth);
            (redacted.map(|header| format!("--header={header}")), next)
        }
        Some((name, value)) if is_credential_name(name) => (
            (!value.is_empty()).then(|| format!("{name}={REDACTED}")),
            Pending::None,
        ),
        Some((name, value)) => (
            redact_text(value, depth).map(|value| format!("{name}={value}")),
            Pending::None,
        ),
        None if is_credential_name(word) => (None, Pending::Value),
        None => (None, Pending::None),
    }
}

/// `名字: 值` 形式的请求头：敏感头的值打码（保留认证方案名），其它头当普通文本。
fn redact_header(header: &str, depth: usize) -> (Option<String>, Pending) {
    let Some((name, rest)) = header.split_once(':') else {
        return (redact_text(header, depth), Pending::None);
    };
    if !is_sensitive_header(name.trim()) {
        return (redact_text(header, depth), Pending::None);
    }
    let value = rest.trim_start();
    // 值被 shell 拆到了后面的词里（`--header Authorization: Bearer x` 没加引号）。
    if value.is_empty() || is_auth_scheme(value) {
        return (None, Pending::Credential);
    }
    let lead = &rest[..rest.len() - value.len()];
    let redacted = match value.split_once(char::is_whitespace) {
        Some((scheme, secret)) if is_auth_scheme(scheme) && !secret.trim().is_empty() => {
            format!("{name}:{lead}{scheme} {REDACTED}")
        }
        _ => format!("{name}:{lead}{REDACTED}"),
    };
    (Some(redacted), Pending::None)
}

/// `NAME=value` 形式的环境变量赋值（NAME 是合法的 shell 变量名）。
fn split_assignment(word: &str) -> Option<(&str, &str)> {
    let (name, value) = word.split_once('=')?;
    let mut chars = name.chars();
    let first = chars.next()?;
    ((first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_'))
    .then_some((name, value))
}

fn redact_assignment(name: &str, value: &str, depth: usize) -> Option<String> {
    if is_credential_name(name) {
        return (!value.is_empty()).then(|| format!("{name}={REDACTED}"));
    }
    redact_text(value, depth).map(|value| format!("{name}={value}"))
}

/// 值被拆到后面词里的敏感头名：`Authorization:`、`http.extraHeader=Authorization:`。
fn ends_with_sensitive_header_name(word: &str) -> bool {
    word.strip_suffix(':').is_some_and(|head| {
        let name = head.rsplit('=').next().unwrap_or(head);
        !name.is_empty() && is_sensitive_header(name)
    })
}

/// 不在选项 / 赋值位置上的文本：带空白的当一段命令行递归处理（`sh -c '…'`、
/// 带空格的参数），否则只看其中的 URL。
fn redact_text(text: &str, depth: usize) -> Option<String> {
    if depth < MAX_NESTING && text.contains(char::is_whitespace) {
        let redacted = redact_line(text, 0, depth + 1, Quotes::Shell);
        return (redacted != text).then_some(redacted);
    }
    redact_urls(text)
}

/// URL 里的口令（`scheme://user:pass@host` → `user:[REDACTED]@`）与凭据类查询参数。
fn redact_urls(text: &str) -> Option<String> {
    let userinfo = redact_url_passwords(text);
    let current = userinfo.as_deref().unwrap_or(text);
    redact_url_query(current).or(userinfo)
}

fn redact_url_passwords(text: &str) -> Option<String> {
    let mut redacted = String::with_capacity(text.len());
    let mut rest = text;
    let mut changed = false;
    while let Some(index) = rest.find("://") {
        let (head, tail) = rest.split_at(index + 3);
        redacted.push_str(head);
        let authority_len = tail
            .find(|c: char| matches!(c, '/' | '?' | '#') || c.is_whitespace())
            .unwrap_or(tail.len());
        let (authority, remainder) = tail.split_at(authority_len);
        match authority.rsplit_once('@').and_then(|(userinfo, host)| {
            userinfo
                .split_once(':')
                .map(|(user, password)| (user, password, host))
        }) {
            Some((user, password, host)) if !password.is_empty() => {
                redacted.push_str(user);
                redacted.push(':');
                redacted.push_str(REDACTED);
                redacted.push('@');
                redacted.push_str(host);
                changed = true;
            }
            _ => redacted.push_str(authority),
        }
        rest = remainder;
    }
    redacted.push_str(rest);
    changed.then_some(redacted)
}

/// 第一个 URL 的查询串里，名字像凭据的参数值打码（`?api_key=…&page=2`）。
fn redact_url_query(text: &str) -> Option<String> {
    let scheme = text.find("://")?;
    let query_start = scheme + text[scheme..].find('?')? + 1;
    let query_end = text[query_start..]
        .find(|c: char| c == '#' || c.is_whitespace())
        .map_or(text.len(), |len| query_start + len);
    let mut changed = false;
    let params: Vec<String> = text[query_start..query_end]
        .split('&')
        .map(|param| match param.split_once('=') {
            Some((name, value)) if !value.is_empty() && is_credential_name(name) => {
                changed = true;
                format!("{name}={REDACTED}")
            }
            _ => param.to_owned(),
        })
        .collect();
    changed.then(|| {
        format!(
            "{}{}{}",
            &text[..query_start],
            params.join("&"),
            &text[query_end..]
        )
    })
}

/// 名字像凭据：选项名（去掉前导 `-`）、环境变量名、URL 查询参数名。
fn is_credential_name(name: &str) -> bool {
    let name: String = name
        .trim_start_matches('-')
        .chars()
        .map(|c| {
            if c == '-' {
                '_'
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect();
    CREDENTIAL_SUFFIXES
        .iter()
        .any(|suffix| name.ends_with(suffix))
        || CREDENTIAL_WORDS.iter().any(|word| {
            name == *word
                || name
                    .strip_suffix(word)
                    .is_some_and(|head| head.ends_with('_'))
        })
}

/// 请求头名是否敏感：认证类（Authorization、Proxy-Authorization、X-Auth-Token …）、
/// Cookie，以及名字本身像凭据的（X-Api-Key、Private-Token …）。
fn is_sensitive_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("auth") || lower.contains("cookie") || is_credential_name(&lower)
}

fn is_auth_scheme(word: &str) -> bool {
    AUTH_SCHEMES
        .iter()
        .any(|scheme| word.eq_ignore_ascii_case(scheme))
}

/// 切词时认哪些引号。
#[derive(Clone, Copy)]
enum Quotes {
    /// Windows 进程的原始命令行：CommandLineToArgvW 只认 `"`，路径里的 `'`（如
    /// `C:\Users\O'Brien\…`）是普通字符。
    Windows,
    /// 参数里套的一段 shell 命令行（`sh -c '…'`）：`'` 与 `"` 都是引号。
    Shell,
}

impl Quotes {
    fn opens(self, ch: char) -> bool {
        match self {
            Quotes::Windows => ch == '"',
            Quotes::Shell => ch == '"' || ch == '\'',
        }
    }
}

/// 一段命令行里的一个词：原文范围、去掉引号后的内容、最先出现的引号。
struct Word {
    span: std::ops::Range<usize>,
    text: String,
    quote: Option<char>,
}

/// 按空白切词：引号里的空白不切、引号本身去掉；反斜杠按字面处理（Windows 路径里
/// 到处是反斜杠）。只用来找出要打码的词，不求还原 shell 语义。
fn split_words(line: &str, quotes: Quotes) -> Vec<Word> {
    let mut words = Vec::new();
    let mut current: Option<Word> = None;
    let mut open_quote: Option<char> = None;
    for (index, ch) in line.char_indices() {
        if open_quote.is_none() && ch.is_whitespace() {
            words.extend(current.take());
            continue;
        }
        let word = current.get_or_insert_with(|| Word {
            span: index..index,
            text: String::new(),
            quote: None,
        });
        word.span.end = index + ch.len_utf8();
        match open_quote {
            Some(quote) if ch == quote => open_quote = None,
            Some(_) => word.text.push(ch),
            None if quotes.opens(ch) => {
                open_quote = Some(ch);
                word.quote.get_or_insert(ch);
            }
            None => word.text.push(ch),
        }
    }
    words.extend(current);
    words
}

/// 一段命令行打码：从第 `first` 个词起按规则处理，只替换要打码的词，其余原文
/// （空白、引号写法）保持不动。
fn redact_line(line: &str, first: usize, depth: usize, quotes: Quotes) -> String {
    let words = split_words(line, quotes);
    let mut texts: Vec<String> = words.iter().map(|word| word.text.clone()).collect();
    // 跳过的第 0 个词（程序本身）带空白：要么是带空格的路径，要么是没配对的引号把
    // 后面的参数一起吞了进来。当一段命令行再过一遍——带空格的路径切开后没有可打码的，
    // 被吞进来的参数则照常打码。
    if first > 0 && depth < MAX_NESTING {
        if let Some(program) = texts.first_mut() {
            if program.contains(char::is_whitespace) {
                *program = redact_line(program, 1, depth + 1, quotes);
            }
        }
    }
    redact_words(&mut texts, first, depth);
    let mut redacted = String::with_capacity(line.len());
    let mut copied = 0;
    for (word, text) in words.iter().zip(&texts) {
        if *text == word.text {
            continue;
        }
        redacted.push_str(&line[copied..word.span.start]);
        match word
            .quote
            .or_else(|| text.contains(char::is_whitespace).then_some('"'))
        {
            Some(quote) => {
                redacted.push(quote);
                redacted.push_str(text);
                redacted.push(quote);
            }
            None => redacted.push_str(text),
        }
        copied = word.span.end;
    }
    redacted.push_str(&line[copied..]);
    redacted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    /// 样例里的假凭据一律带 `s3cr3t`：打码后的 argv 与 cmdline 都不能再出现它。
    #[track_caller]
    fn assert_redacted(input: &[&str], expected: &[&str]) {
        let argv = words(input);
        let (redacted, cmdline) = redact_command(Some(argv.clone()), Some(argv.join(" ")));
        assert_eq!(redacted, Some(words(expected)));
        assert_eq!(cmdline, Some(expected.join(" ")));
        assert!(
            !expected.join(" ").contains("s3cr3t"),
            "expected output itself leaks a sample secret"
        );
    }

    #[test]
    fn sensitive_header_values_are_redacted_but_the_scheme_and_other_headers_stay() {
        assert_redacted(
            &[
                "curl",
                "--header",
                "Authorization: Bearer s3cr3t-1",
                "-H",
                "Content-Type: application/json",
                "https://api.example.invalid/v1",
            ],
            &[
                "curl",
                "--header",
                "Authorization: Bearer [REDACTED]",
                "-H",
                "Content-Type: application/json",
                "https://api.example.invalid/v1",
            ],
        );
        assert_redacted(
            &[
                "curl",
                "-H",
                "Authorization: token s3cr3t-2",
                "-H",
                "X-Api-Key: s3cr3t-3",
                "-HCookie: session=s3cr3t-4",
                "--header=Private-Token: s3cr3t-5",
            ],
            &[
                "curl",
                "-H",
                "Authorization: token [REDACTED]",
                "-H",
                "X-Api-Key: [REDACTED]",
                "-HCookie: [REDACTED]",
                "--header=Private-Token: [REDACTED]",
            ],
        );
        // 没加引号的 `--header Authorization: Basic …` 被 shell 拆成了几个词。
        assert_redacted(
            &[
                "curl",
                "--header",
                "Authorization:",
                "Basic",
                "s3cr3t-6",
                "https://example.invalid",
            ],
            &[
                "curl",
                "--header",
                "Authorization:",
                "Basic",
                "[REDACTED]",
                "https://example.invalid",
            ],
        );
    }

    #[test]
    fn credential_option_values_are_redacted_in_both_spellings() {
        assert_redacted(
            &[
                "gh",
                "--token=s3cr3t-1",
                "--api-key",
                "s3cr3t-2",
                "-password=s3cr3t-3",
                "--client-secret",
                "s3cr3t-4",
                "--model",
                "opus",
            ],
            &[
                "gh",
                "--token=[REDACTED]",
                "--api-key",
                "[REDACTED]",
                "-password=[REDACTED]",
                "--client-secret",
                "[REDACTED]",
                "--model",
                "opus",
            ],
        );
        // 值的位置上是另一个选项：前一个只是开关，不动。
        assert_redacted(
            &["tool", "--api-key", "--verbose"],
            &["tool", "--api-key", "--verbose"],
        );
        // 名字里带凭据词根、但不是凭据的选项不动。
        let plain = [
            "claude",
            "--max-tokens",
            "4096",
            "--key-file",
            "/etc/app.pem",
            "--bypass",
            "x",
        ];
        assert_redacted(&plain, &plain);
    }

    #[test]
    fn credential_environment_assignments_are_redacted() {
        assert_redacted(
            &[
                "env",
                "OPENAI_API_KEY=s3cr3t-1",
                "GITHUB_TOKEN=s3cr3t-2",
                "PGPASSWORD=s3cr3t-3",
                "PATH=/usr/bin",
                "MONKEY=banana",
                "codex",
            ],
            &[
                "env",
                "OPENAI_API_KEY=[REDACTED]",
                "GITHUB_TOKEN=[REDACTED]",
                "PGPASSWORD=[REDACTED]",
                "PATH=/usr/bin",
                "MONKEY=banana",
                "codex",
            ],
        );
    }

    #[test]
    fn bearer_tokens_are_redacted_wherever_they_appear() {
        assert_redacted(
            &["tool", "Bearer", "s3cr3t-1"],
            &["tool", "Bearer", "[REDACTED]"],
        );
        assert_redacted(
            &["tool", "--auth-header", "Bearer s3cr3t-2"],
            &["tool", "--auth-header", "Bearer [REDACTED]"],
        );
        assert_redacted(
            &["curl", "-H", "X-Custom: bearer s3cr3t-3"],
            &["curl", "-H", "X-Custom: bearer [REDACTED]"],
        );
    }

    #[test]
    fn url_passwords_and_credential_query_parameters_are_redacted() {
        assert_redacted(
            &[
                "git",
                "clone",
                "https://user:s3cr3t-1@example.invalid/repo.git",
                "ssh://git@example.invalid/repo.git",
            ],
            &[
                "git",
                "clone",
                "https://user:[REDACTED]@example.invalid/repo.git",
                "ssh://git@example.invalid/repo.git",
            ],
        );
        assert_redacted(
            &[
                "curl",
                "https://api.example.invalid/v1?api_key=s3cr3t-2&page=2#top",
                "--proxy",
                "http://proxy:s3cr3t-3@proxy.invalid:8080",
                "HTTPS_PROXY=http://me:s3cr3t-4@proxy.invalid",
            ],
            &[
                "curl",
                "https://api.example.invalid/v1?api_key=[REDACTED]&page=2#top",
                "--proxy",
                "http://proxy:[REDACTED]@proxy.invalid:8080",
                "HTTPS_PROXY=http://me:[REDACTED]@proxy.invalid",
            ],
        );
    }

    #[test]
    fn command_line_arguments_are_redacted_recursively() {
        assert_redacted(
            &[
                "sh",
                "-c",
                "curl -H \"Authorization: Bearer s3cr3t-1\" --token=s3cr3t-2 https://u:s3cr3t-3@example.invalid",
            ],
            &[
                "sh",
                "-c",
                "curl -H \"Authorization: Bearer [REDACTED]\" --token=[REDACTED] https://u:[REDACTED]@example.invalid",
            ],
        );
        // 只是提到这些词的普通文本（提示词、提交说明）不动。
        let prose = ["claude", "fix the token parser and the api key docs"];
        assert_redacted(&prose, &prose);
    }

    #[test]
    fn program_names_and_plain_commands_are_left_alone() {
        // argv[0] 原样保留：CLI 靠它判断窗格 shell 是否还在初始化。
        let shell = ["/bin/sh"];
        assert_redacted(&shell, &shell);
        let plain = [
            "cargo",
            "nextest",
            "run",
            "--profile",
            "ci",
            "-E",
            "test(auth)",
            "-",
        ];
        assert_redacted(&plain, &plain);
        assert_eq!(redact_command(None, None), (None, None));
    }

    #[test]
    fn raw_windows_command_lines_are_redacted_in_place() {
        let raw = r#""C:\Program Files\curl.exe" -H "Authorization: Bearer s3cr3t-1" --token=s3cr3t-2  https://u:s3cr3t-3@example.invalid"#;
        let expected = r#""C:\Program Files\curl.exe" -H "Authorization: Bearer [REDACTED]" --token=[REDACTED]  https://u:[REDACTED]@example.invalid"#;
        let argv = words(&[
            r"C:\Program Files\curl.exe",
            "-H",
            "Authorization: Bearer s3cr3t-1",
            "--token=s3cr3t-2",
            "https://u:s3cr3t-3@example.invalid",
        ]);
        let (redacted, cmdline) = redact_command(Some(argv), Some(raw.to_owned()));
        assert_eq!(cmdline.as_deref(), Some(expected));
        assert!(redacted.is_some_and(|argv| !argv.join(" ").contains("s3cr3t")));
        // 平台没给 argv 时同样按原始命令行处理。
        let (_, cmdline) = redact_command(None, Some(raw.to_owned()));
        assert_eq!(cmdline.as_deref(), Some(expected));

        // M1：CommandLineToArgvW 只认 `"`。程序路径里的撇号不是引号，不能把整行吞成
        // 第 0 个词（第 0 个词不打码，凭据会原样漏出）。
        let raw = r#"C:\Users\O'Brien\bin\tool.exe --token s3cr3t-4 --name "a b""#;
        let expected = r#"C:\Users\O'Brien\bin\tool.exe --token [REDACTED] --name "a b""#;
        let argv = words(&[
            r"C:\Users\O'Brien\bin\tool.exe",
            "--token",
            "s3cr3t-4",
            "--name",
            "a b",
        ]);
        let (redacted, cmdline) = redact_command(Some(argv), Some(raw.to_owned()));
        assert_eq!(cmdline.as_deref(), Some(expected));
        assert!(redacted.is_some_and(|argv| !argv.join(" ").contains("s3cr3t")));
        let (_, cmdline) = redact_command(None, Some(raw.to_owned()));
        assert_eq!(cmdline.as_deref(), Some(expected));
        // 引号没配对、整行都进了第 0 个词：同样要找出里面的参数。
        let (_, cmdline) =
            redact_command(None, Some(r#""C:\tools\x.exe --token s3cr3t-5"#.to_owned()));
        assert!(
            cmdline
                .as_deref()
                .is_some_and(|line| !line.contains("s3cr3t")),
            "{cmdline:?}"
        );
    }
}
