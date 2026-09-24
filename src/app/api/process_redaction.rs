//! `pane.process_info` 输出层的凭据打码（RL12）。
//!
//! 能连上 socket 的调用方都能查任何窗格的前台进程，而命令行里常带认证头、token、
//! 口令。进程采集（`platform` / `detect`）保持原样——检测要看完整 argv；这里只在
//! 把结果交给调用方之前，把疑似凭据的值换成 `[REDACTED]`：程序名、选项名与参数
//! 位置原样保留，调用方仍看得出跑的是什么。覆盖：
//!
//! - 凭据类选项的值：`--token abc`、`--token=abc`、`--api-key abc`、`-password=abc`、
//!   `--oauth2-bearer abc`；
//! - 「名:口令」：curl 的 `--user` / `--proxy-user` / `-u` / `-U`，只打冒号后；
//! - 敏感请求头：`-H 'Authorization: …'`、`--header Authorization: …`、
//!   `-HX-Api-Key: …`，以及 httpie / xh 的位置参数 `Authorization:Bearer …`；
//!   保留 `Bearer` / `Basic` 等认证方案名；
//! - 独立出现的 `Bearer <token>`；
//! - 凭据类变量赋值：`OPENAI_API_KEY=…`、`GITHUB_TOKEN=…`、PowerShell 的
//!   `$env:NAME=…` 与 `-Headers @{Authorization=…}`、npm 的 `…/:_authToken=…`；
//! - PowerShell 冒号参数 `-Token:…` 与 Windows 风格开关 `/token:…`、`/p:Password=…`；
//! - URL 里的口令与凭据类查询参数：`https://user:pass@host`、`?api_key=…`；http(s)
//!   URL 的 userinfo 只有令牌时整段（`https://TOKEN@host`，`ssh://git@` 保留）；
//! - 整段命令行形态的参数（`sh -c '…'`）按以上规则递归处理，`;`、`&&`、`|` 两侧
//!   不带空格也分得开。
//!
//! 只认形态、不认程序：`-p<口令>` 这类短选项的含义随程序而异，不在覆盖范围内。

/// 打码后的占位值。
const REDACTED: &str = "[REDACTED]";

/// 参数里套命令行（`sh -c 'sh -c …'`）最多递归处理的层数。
const MAX_NESTING: usize = 3;

/// 名字以这些词结尾即视为凭据（不分大小写，`-` 与 `_` 等价）。请求头名也用这张
/// 表判断：`Authorization`、`Proxy-Authorization`、`X-Auth-Token`、`Cookie` 命中，
/// `X-Author` 不命中。
const CREDENTIAL_SUFFIXES: [&str; 15] = [
    "token",
    "secret",
    "password",
    "passwd",
    "passphrase",
    "passcode",
    "apikey",
    "credential",
    "credentials",
    "cookie",
    "signature",
    "authorization",
    "authentication",
    "bearer",
    "auth_header",
];

/// 这些词要独立成词（整个名字，或前面是 `_`）才算凭据：`--api-key`、`DB_PASS`、
/// `--auth` 算，`MONKEY`、`--bypass` 不算。
const CREDENTIAL_WORDS: [&str; 8] = [
    "key", "pass", "auth", "sig", "pw", "jwt", "passin", "passout",
];

/// 这些词只在前面是 `_` 时才算：`MYSQL_PWD` 算，当前目录 `PWD`、`OLDPWD` 不算。
const CREDENTIAL_TAILS: [&str; 1] = ["pwd"];

/// 敏感请求头里保留、不打码的认证方案名。
const AUTH_SCHEMES: [&str; 6] = ["bearer", "basic", "token", "digest", "negotiate", "ntlm"];

/// curl 取「名:口令」的选项：冒号后是口令。
const USER_PASSWORD_OPTIONS: [&str; 4] = ["--user", "--proxy-user", "-u", "-U"];

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
    /// 上一个词是 `--user` / `-u` 一类：当前词是 `名:口令`。
    UserPassword,
    /// 上一个词是 PowerShell 的 `$env:NAME`（NAME 像凭据）：当前词若是 `=`，再下一个
    /// 词就是值（`$env:GH_TOKEN = '…'`）。
    Assignment,
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
    // 值的位置上出现选项，说明前一个词只是开关：约定作废，按选项处理。等着凭据时，
    // 以 `-` 开头的随机串（`--token -Xk9…`）仍是值。
    let awaits_secret = matches!(pending, Pending::Value | Pending::Credential);
    if looks_like_option(word) && !(awaits_secret && looks_like_dash_secret(word)) {
        return redact_option(word, depth);
    }
    match pending {
        Pending::Value => return (Some(redacted_secret(word)), Pending::None),
        Pending::Header => return redact_header(word, depth),
        Pending::UserPassword => return (redact_user_password(word), Pending::None),
        Pending::Credential if is_auth_scheme(word) => return (None, Pending::Credential),
        Pending::Credential => return (Some(redacted_secret(word)), Pending::None),
        Pending::Assignment if word == "=" => return (None, Pending::Value),
        Pending::Assignment | Pending::None => {}
    }
    // 带空白的参数是一段命令行（`sh -c 'TOKEN=… codex'`、`pwsh -Command '…'`），不能
    // 整个当成一个赋值或请求头：逐词再过一遍。
    if word.contains(char::is_whitespace) {
        return (redact_text(word, depth), Pending::None);
    }
    // Windows 风格开关：`/token:…`、`/p:Password=…`。
    if let Some((name, separator, value)) = split_switch(word) {
        return (
            redact_option_value(name, separator, value, depth),
            Pending::None,
        );
    }
    if let Some((name, key, value)) = split_assignment(word) {
        return redact_assignment(name, key, value, depth);
    }
    // PowerShell 带空格的赋值：`$env:GH_TOKEN = '…'`。
    if word
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("$env:"))
        && is_credential_name(&word[5..])
    {
        return (None, Pending::Assignment);
    }
    // httpie / xh 的请求头是位置参数：`Authorization:Bearer …`、`x-api-key:…`。
    if is_positional_header(word) {
        return redact_header(word, depth);
    }
    if word.eq_ignore_ascii_case("bearer") || ends_with_sensitive_header_name(word) {
        return (None, Pending::Credential);
    }
    (redact_text(word, depth), Pending::None)
}

/// 等着凭据时出现的 `-…`：不是 `--长选项`，也不是全小写字母的开关（`-v`、`-debug`），
/// 那多半就是以 `-` 开头的随机串。
fn looks_like_dash_secret(word: &str) -> bool {
    !word.starts_with("--")
        && !word[1..]
            .chars()
            .all(|c| c.is_ascii_lowercase() || c == '-')
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
    if USER_PASSWORD_OPTIONS.contains(&word) {
        return (None, Pending::UserPassword);
    }
    if !word.starts_with("--") {
        // curl 允许 `-H` 紧贴值：`-HAuthorization: …`。
        if let Some(header) = word.strip_prefix("-H") {
            let (redacted, next) = redact_header(header, depth);
            return (redacted.map(|header| format!("-H{header}")), next);
        }
        // `-uadmin:pw` / `-Uproxy:pw`；带 `=` 的（Go 风格的 `-url=…`）不是这种写法。
        for short in ["-u", "-U"] {
            if let Some(value) = word.strip_prefix(short) {
                if value.contains(':') && !value.contains('=') {
                    let redacted = redact_user_password(value);
                    return (
                        redacted.map(|value| format!("{short}{value}")),
                        Pending::None,
                    );
                }
            }
        }
    }
    // `--name=value`，以及 PowerShell 的冒号参数 `-Token:value`。
    match word.find(['=', ':']) {
        Some(index) => {
            let (name, rest) = word.split_at(index);
            let (separator, value) = rest.split_at(1);
            if name == "--header" && separator == "=" {
                let (redacted, next) = redact_header(value, depth);
                return (redacted.map(|header| format!("--header={header}")), next);
            }
            (
                redact_option_value(name, separator, value, depth),
                Pending::None,
            )
        }
        None if is_credential_name(word) => (None, Pending::Value),
        None => (None, Pending::None),
    }
}

/// 选项或开关与值写在一起（`--token=…`、`-Token:…`、`/p:Password=…`）：凭据类
/// 名字整个值打码，`--user=` 一类只打冒号后，其余的值再按赋值 / 普通文本看一遍。
fn redact_option_value(name: &str, separator: &str, value: &str, depth: usize) -> Option<String> {
    let redacted = if USER_PASSWORD_OPTIONS.contains(&name) {
        redact_user_password(value)?
    } else if is_credential_name(name.trim_start_matches('/')) {
        if value.is_empty() {
            return None;
        }
        redacted_secret(value)
    } else {
        redact_embedded(value, depth)?
    };
    Some(format!("{name}{separator}{redacted}"))
}

/// 选项值里再套一层 `名字=值`（`-p:Password=…`、`--env=API_KEY=…`）或普通文本。
fn redact_embedded(value: &str, depth: usize) -> Option<String> {
    match split_assignment(value) {
        Some((name, key, value)) => redact_assignment(name, key, value, depth).0,
        None => redact_text(value, depth),
    }
}

/// Windows 风格开关 `/名字:值`、`/名字=值`；Unix 路径（`/usr/bin/env`）不算。
fn split_switch(word: &str) -> Option<(&str, &str, &str)> {
    let rest = word.strip_prefix('/')?;
    let index = rest.find([':', '='])?;
    let name = &rest[..index];
    let mut chars = name.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    valid.then(|| {
        let (switch, rest) = word.split_at(index + 1);
        let (separator, value) = rest.split_at(1);
        (switch, separator, value)
    })
}

/// `名字: 值` 形式的请求头：敏感头的值打码（保留认证方案名），其它头当普通文本。
fn redact_header(header: &str, depth: usize) -> (Option<String>, Pending) {
    let Some((name, rest)) = header.split_once(':') else {
        return (redact_text(header, depth), Pending::None);
    };
    if !is_credential_name(name.trim()) {
        return (redact_text(header, depth), Pending::None);
    }
    // httpie 的 `名字:=值`（原样 JSON）连同 `=` 一起保留在值前面。
    let value = rest.trim_start_matches('=').trim_start();
    // 值被 shell 拆到了后面的词里（`--header Authorization: Bearer x` 没加引号）。
    if value.is_empty() || is_auth_scheme(value) {
        return (None, Pending::Credential);
    }
    let lead = &rest[..rest.len() - value.len()];
    (
        Some(format!("{name}:{lead}{}", redacted_secret(value))),
        Pending::None,
    )
}

/// 凭据值打码：`Bearer xxx` 这类「认证方案 + 凭据」保留方案名。
fn redacted_secret(value: &str) -> String {
    match value.split_once(char::is_whitespace) {
        Some((scheme, secret)) if is_auth_scheme(scheme) && !secret.trim().is_empty() => {
            format!("{scheme} {REDACTED}")
        }
        _ => REDACTED.to_owned(),
    }
}

/// `名:口令` 只打冒号后；只有用户名时不动。
fn redact_user_password(value: &str) -> Option<String> {
    let (user, password) = value.split_once(':')?;
    (!password.is_empty()).then(|| format!("{user}:{REDACTED}"))
}

/// httpie / xh 位置参数形式的敏感请求头：`名字:值`，名字是请求头记号且像凭据。
fn is_positional_header(word: &str) -> bool {
    word.split_once(':').is_some_and(|(name, _)| {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            && is_credential_name(name)
    })
}

/// `键=值`：shell 变量赋值（`NAME=…`），以及键尾是个名字的写法——PowerShell 的
/// `$env:NAME=…` 与哈希表 `@{Authorization=…`、npm 的 `//registry/:_authToken=…`。
/// 返回（用来判断的名字，原样的键，值）；URL 查询串这类键尾不是名字的不算。
fn split_assignment(word: &str) -> Option<(&str, &str, &str)> {
    let (key, value) = word.split_once('=')?;
    let name = key.rsplit([':', '/', '.', '{', '$', '@']).next()?;
    let mut chars = name.chars();
    let first = chars.next()?;
    ((first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-')))
    .then_some((name, key, value))
}

fn redact_assignment(
    name: &str,
    key: &str,
    value: &str,
    depth: usize,
) -> (Option<String>, Pending) {
    if !is_credential_name(name) {
        return (
            redact_text(value, depth).map(|value| format!("{key}={value}")),
            Pending::None,
        );
    }
    // httpie 的 `名字==值`（查询参数）保留第二个 `=`；PowerShell 哈希表收尾的
    // `}`、`;` 留在值后面。
    let secret = value.trim_start_matches('=');
    let eq = &value[..value.len() - secret.len()];
    let secret_end = secret.trim_end_matches(['}', ';', ',', ')']);
    let tail = &secret[secret_end.len()..];
    if secret_end.is_empty() {
        return (None, Pending::None);
    }
    // 值只剩认证方案名：凭据被切到了下一个词（`AUTH="Bearer x"` 在命令行里切开后）。
    if is_auth_scheme(secret_end) {
        return (None, Pending::Credential);
    }
    (
        Some(format!("{key}={eq}{}{tail}", redacted_secret(secret_end))),
        Pending::None,
    )
}

/// 值被拆到后面词里的敏感头名：`Authorization:`、`http.extraHeader=Authorization:`。
fn ends_with_sensitive_header_name(word: &str) -> bool {
    word.strip_suffix(':').is_some_and(|head| {
        let name = head.rsplit('=').next().unwrap_or(head);
        !name.is_empty() && is_credential_name(name)
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

/// URL 里的口令（`scheme://user:pass@host` → `user:[REDACTED]@`；http(s) 的 userinfo
/// 只有令牌时整段打码）与凭据类查询参数。
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
        let scheme_start = rest[..index]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
            .map_or(0, |position| position + 1);
        let http = is_http_scheme(&rest[scheme_start..index]);
        let (head, tail) = rest.split_at(index + 3);
        redacted.push_str(head);
        let authority_len = tail
            .find(|c: char| matches!(c, '/' | '?' | '#') || c.is_whitespace())
            .unwrap_or(tail.len());
        let (authority, remainder) = tail.split_at(authority_len);
        match authority.rsplit_once('@') {
            Some((userinfo, host)) => match userinfo.split_once(':') {
                Some((user, password)) if !password.is_empty() => {
                    redacted.push_str(user);
                    redacted.push(':');
                    redacted.push_str(REDACTED);
                    redacted.push('@');
                    redacted.push_str(host);
                    changed = true;
                }
                // http(s) 的 userinfo 只有一段：多半是令牌（`https://TOKEN@github.com`）。
                None if http && !userinfo.is_empty() => {
                    redacted.push_str(REDACTED);
                    redacted.push('@');
                    redacted.push_str(host);
                    changed = true;
                }
                _ => redacted.push_str(authority),
            },
            None => redacted.push_str(authority),
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

/// 名字像凭据：选项名（去掉前导 `-`）、环境变量名、URL 查询参数名、请求头名。
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
    let separated = |word: &str| {
        name.strip_suffix(word)
            .is_some_and(|head| head.ends_with('_'))
    };
    CREDENTIAL_SUFFIXES
        .iter()
        .any(|suffix| name.ends_with(suffix))
        || CREDENTIAL_WORDS
            .iter()
            .any(|word| name == *word || separated(word))
        || CREDENTIAL_TAILS.iter().any(|word| separated(word))
}

/// `http`、`https` 以及 `git+https` 这类套在 http(s) 上的协议。
fn is_http_scheme(scheme: &str) -> bool {
    let scheme = scheme.to_ascii_lowercase();
    ["http", "https"]
        .iter()
        .any(|http| scheme == *http || scheme.ends_with(&format!("+{http}")))
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

    /// 引号外把一条命令与下一条分开的字符：shell 里的 `;`、`&`、`|` 与换行
    /// （`&&`、`||` 就是连着两个）。Windows 原始命令行交给 CommandLineToArgvW，
    /// 这些都是普通字符。
    fn separates_commands(self, ch: char) -> bool {
        match self {
            Quotes::Windows => false,
            Quotes::Shell => matches!(ch, ';' | '&' | '|' | '\n'),
        }
    }
}

/// 一段命令行里的一个词：原文范围、去掉引号后的内容、最先出现的引号、是否一条
/// 命令的第一个词。
struct Word {
    span: std::ops::Range<usize>,
    text: String,
    /// `text` 里每个字符的（在 `text` 里的字节位置，在原文里的字节位置）。
    offsets: Vec<(usize, usize)>,
    quote: Option<char>,
    starts_command: bool,
}

/// 按空白切词：引号里的空白不切、引号本身去掉；反斜杠按字面处理（Windows 路径里
/// 到处是反斜杠）。只用来找出要打码的词，不求还原 shell 语义。
fn split_words(line: &str, quotes: Quotes) -> Vec<Word> {
    let mut words = Vec::new();
    let mut current: Option<Word> = None;
    let mut open_quote: Option<char> = None;
    let mut next_starts_command = true;
    for (index, ch) in line.char_indices() {
        if open_quote.is_none() {
            // 命令分隔符不进任何词：原文里留在词与词之间，打码时原样保留。
            if quotes.separates_commands(ch) {
                words.extend(current.take());
                next_starts_command = true;
                continue;
            }
            if ch.is_whitespace() {
                words.extend(current.take());
                continue;
            }
        }
        let word = current.get_or_insert_with(|| Word {
            span: index..index,
            text: String::new(),
            offsets: Vec::new(),
            quote: None,
            starts_command: std::mem::take(&mut next_starts_command),
        });
        word.span.end = index + ch.len_utf8();
        match open_quote {
            Some(quote) if ch == quote => open_quote = None,
            None if quotes.opens(ch) => {
                open_quote = Some(ch);
                word.quote.get_or_insert(ch);
            }
            Some(_) | None => {
                word.offsets.push((word.text.len(), index));
                word.text.push(ch);
            }
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
    // 每条命令单独处理：上一条命令里等着的值不会落到下一条命令的第一个词上。
    let mut start = 0;
    for end in 1..=words.len() {
        if end == words.len() || words[end].starts_command {
            redact_words(
                &mut texts[start..end],
                if start == 0 { first } else { 0 },
                depth,
            );
            start = end;
        }
    }
    let mut redacted = String::with_capacity(line.len());
    let mut copied = 0;
    for (word, text) in words.iter().zip(&texts) {
        if *text == word.text {
            continue;
        }
        redacted.push_str(&line[copied..word.span.start]);
        splice_word(&mut redacted, line, word, text);
        copied = word.span.end;
    }
    redacted.push_str(&line[copied..]);
    redacted
}

/// 把改过的词写回原文：只换掉改动的那一段，引号写法原样保留
/// （`$env:X='…'` → `$env:X='[REDACTED]'`）；改动跨过引号时整词重写。
fn splice_word(out: &mut String, line: &str, word: &Word, text: &str) {
    let old = word.text.as_str();
    let prefix: usize = old
        .chars()
        .zip(text.chars())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum();
    let suffix: usize = old[prefix..]
        .chars()
        .rev()
        .zip(text[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum();
    let old_mid = prefix..old.len() - suffix;
    let raw_start = word
        .offsets
        .iter()
        .find(|(text_at, _)| *text_at == old_mid.start)
        .map(|(_, raw_at)| *raw_at);
    let raw_end = word
        .offsets
        .iter()
        .take_while(|(text_at, _)| *text_at < old_mid.end)
        .last()
        .and_then(|(text_at, raw_at)| {
            let len = old[*text_at..].chars().next()?.len_utf8();
            Some(raw_at + len)
        });
    if let (false, Some(start), Some(end)) = (old_mid.is_empty(), raw_start, raw_end) {
        if line.get(start..end) == Some(&old[old_mid.clone()]) {
            out.push_str(&line[word.span.start..start]);
            out.push_str(&text[prefix..text.len() - suffix]);
            out.push_str(&line[end..word.span.end]);
            return;
        }
    }
    match word
        .quote
        .or_else(|| text.contains(char::is_whitespace).then_some('"'))
    {
        Some(quote) => {
            out.push(quote);
            out.push_str(text);
            out.push(quote);
        }
        None => out.push_str(text),
    }
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

    /// 复审 M2：curl 的 `--user` / `--proxy-user` / `-u` / `-U` 取「名:口令」，冒号后
    /// 打码；只给用户名（curl 会再问口令）与 `sudo -u root` 这类不动。
    #[test]
    fn user_password_options_keep_the_user_name() {
        assert_redacted(
            &[
                "curl",
                "--user",
                "admin:s3cr3t-1",
                "https://example.invalid",
            ],
            &[
                "curl",
                "--user",
                "admin:[REDACTED]",
                "https://example.invalid",
            ],
        );
        assert_redacted(
            &["curl", "--user=admin:s3cr3t-2"],
            &["curl", "--user=admin:[REDACTED]"],
        );
        assert_redacted(
            &[
                "curl",
                "--proxy-user",
                "proxy:s3cr3t-3",
                "-x",
                "http://proxy.invalid:3128",
            ],
            &[
                "curl",
                "--proxy-user",
                "proxy:[REDACTED]",
                "-x",
                "http://proxy.invalid:3128",
            ],
        );
        assert_redacted(
            &["curl", "-u", "admin:s3cr3t-4"],
            &["curl", "-u", "admin:[REDACTED]"],
        );
        assert_redacted(
            &["curl", "-uadmin:s3cr3t-5", "-Uproxy:s3cr3t-6"],
            &["curl", "-uadmin:[REDACTED]", "-Uproxy:[REDACTED]"],
        );
        let user_only = ["curl", "--user", "admin", "https://example.invalid"];
        assert_redacted(&user_only, &user_only);
        let sudo = ["sudo", "-u", "root", "ls"];
        assert_redacted(&sudo, &sudo);
    }

    /// 复审 M2：选项名是 authorization / bearer / auth-header 一类的，值整个是凭据
    /// （带认证方案名的保留方案名）。
    #[test]
    fn bearer_and_authorization_options_are_redacted() {
        assert_redacted(
            &["curl", "--oauth2-bearer", "s3cr3t-1"],
            &["curl", "--oauth2-bearer", "[REDACTED]"],
        );
        assert_redacted(
            &[
                "tool",
                "--authorization",
                "s3cr3t-2",
                "--auth-header",
                "Basic s3cr3t-3",
                "--bearer",
                "s3cr3t-4",
            ],
            &[
                "tool",
                "--authorization",
                "[REDACTED]",
                "--auth-header",
                "Basic [REDACTED]",
                "--bearer",
                "[REDACTED]",
            ],
        );
    }

    /// 复审 M2：httpie / xh 的请求头是位置参数 `名字:值`，冒号后常不带空格、前面也
    /// 没有 `-H`。名字只是含 auth 字样的（`X-Author`）与普通头不动。
    #[test]
    fn positional_request_headers_are_redacted() {
        assert_redacted(
            &[
                "http",
                "POST",
                "api.example.invalid/v1",
                "Authorization:Bearer s3cr3t-1",
                "x-api-key:s3cr3t-2",
                "Content-Type:application/json",
                "X-Author:Jane",
                "name=value",
            ],
            &[
                "http",
                "POST",
                "api.example.invalid/v1",
                "Authorization:Bearer [REDACTED]",
                "x-api-key:[REDACTED]",
                "Content-Type:application/json",
                "X-Author:Jane",
                "name=value",
            ],
        );
    }

    /// 复审 M2：http(s) URL 的 userinfo 只有令牌（没有冒号）时整段打码；`ssh://git@`
    /// 这类其它协议的用户名保留。
    #[test]
    fn token_only_userinfo_in_http_urls_is_redacted() {
        assert_redacted(
            &[
                "git",
                "clone",
                "https://s3cr3t-1@github.com/o/r.git",
                "git+https://s3cr3t-2@example.invalid/x",
                "ssh://git@github.com/o/r.git",
                "ftp://anonymous@ftp.example.invalid/pub",
            ],
            &[
                "git",
                "clone",
                "https://[REDACTED]@github.com/o/r.git",
                "git+https://[REDACTED]@example.invalid/x",
                "ssh://git@github.com/o/r.git",
                "ftp://anonymous@ftp.example.invalid/pub",
            ],
        );
    }

    /// 复审 L1 / L2：参数里套的 shell 命令行按 `;`、`&&`、`||`、`|`、换行分成几条
    /// 命令：运算符两侧没有空格也照常打码，打码不吞运算符，上一条命令里等着的值
    /// 不会落到下一条命令的第一个词上。
    #[test]
    fn shell_operators_split_commands_even_without_spaces() {
        assert_redacted(
            &["sh", "-c", "cd /x&&TOKEN=s3cr3t-1 codex"],
            &["sh", "-c", "cd /x&&TOKEN=[REDACTED] codex"],
        );
        assert_redacted(
            &[
                "sh",
                "-c",
                "foo;API_KEY=s3cr3t-2 bar|curl --token s3cr3t-3||true",
            ],
            &[
                "sh",
                "-c",
                "foo;API_KEY=[REDACTED] bar|curl --token [REDACTED]||true",
            ],
        );
        assert_redacted(
            &["sh", "-c", "export API_KEY=s3cr3t-4; codex"],
            &["sh", "-c", "export API_KEY=[REDACTED]; codex"],
        );
        let plain = ["sh", "-c", "tool --api-key; ls -la\nbearer-check done"];
        assert_redacted(&plain, &plain);
    }

    /// 复审 L1：PowerShell 的 `$env:NAME=…`（含带空格的 `$env:NAME = …`）、
    /// `-Headers @{Authorization='Bearer …'}`、冒号参数 `-Token:…`，以及 Windows 风格
    /// 开关 `/token:…`、`/p:Password=…`。引号写法原样保留，只换掉值本身。
    #[test]
    fn powershell_and_windows_switch_spellings_are_redacted() {
        assert_redacted(
            &[
                "pwsh",
                "-Command",
                "$env:OPENAI_API_KEY='s3cr3t-1'; $env:GH_TOKEN = \"s3cr3t-2\"; codex",
            ],
            &[
                "pwsh",
                "-Command",
                "$env:OPENAI_API_KEY='[REDACTED]'; $env:GH_TOKEN = \"[REDACTED]\"; codex",
            ],
        );
        assert_redacted(
            &[
                "pwsh",
                "-c",
                "Invoke-RestMethod -Headers @{Authorization='Bearer s3cr3t-3'; 'X-Api-Key'='s3cr3t-4'} https://example.invalid",
            ],
            &[
                "pwsh",
                "-c",
                "Invoke-RestMethod -Headers @{Authorization='Bearer [REDACTED]'; 'X-Api-Key'='[REDACTED]'} https://example.invalid",
            ],
        );
        assert_redacted(
            &[
                "tool.exe",
                "-Token:s3cr3t-5",
                "/token:s3cr3t-6",
                "/p:Password=s3cr3t-7",
                "/p:Configuration=Release",
                "/usr/bin/env",
            ],
            &[
                "tool.exe",
                "-Token:[REDACTED]",
                "/token:[REDACTED]",
                "/p:Password=[REDACTED]",
                "/p:Configuration=Release",
                "/usr/bin/env",
            ],
        );
    }

    /// 带空白的参数按一段命令行逐词处理：开头的赋值不会把后面的命令一起吞掉；
    /// 值被切开时（`"AUTH_HEADER=Bearer …"`）方案名留着、凭据照打。
    #[test]
    fn assignments_inside_command_lines_keep_the_rest_of_the_command() {
        assert_redacted(
            &["sh", "-c", "TOKEN=s3cr3t-1 codex --model opus"],
            &["sh", "-c", "TOKEN=[REDACTED] codex --model opus"],
        );
        assert_redacted(
            &["env", "AUTH_HEADER=Bearer s3cr3t-2", "tool"],
            &["env", "AUTH_HEADER=Bearer [REDACTED]", "tool"],
        );
    }

    /// 复审 L1：词表补上 MYSQL_PWD（`PWD` / `OLDPWD` 是目录，不动）、`--pw`、
    /// `--passcode`、`--jwt`，以及 npm 的 `//registry/:_authToken=`。
    #[test]
    fn extra_credential_names_are_recognised() {
        assert_redacted(
            &[
                "env",
                "MYSQL_PWD=s3cr3t-1",
                "PWD=/home/me",
                "OLDPWD=/tmp",
                "tool",
                "--pw",
                "s3cr3t-2",
                "--passcode=s3cr3t-3",
                "--jwt",
                "s3cr3t-4",
                "//registry.npmjs.org/:_authToken=s3cr3t-5",
            ],
            &[
                "env",
                "MYSQL_PWD=[REDACTED]",
                "PWD=/home/me",
                "OLDPWD=/tmp",
                "tool",
                "--pw",
                "[REDACTED]",
                "--passcode=[REDACTED]",
                "--jwt",
                "[REDACTED]",
                "//registry.npmjs.org/:_authToken=[REDACTED]",
            ],
        );
    }

    /// 复审 L1：跟在凭据选项或 `Bearer` 后面、以 `-` 开头的随机串是凭据本身；
    /// 像选项的（`--verbose`、全小写的 `-debug`）仍按选项处理。
    #[test]
    fn dash_leading_secrets_after_credential_options_are_redacted() {
        assert_redacted(
            &[
                "tool",
                "--token",
                "-s3cr3tXk9",
                "Bearer",
                "-s3cr3tQ2",
                "--api-key",
                "--verbose",
                "--token",
                "-debug",
            ],
            &[
                "tool",
                "--token",
                "[REDACTED]",
                "Bearer",
                "[REDACTED]",
                "--api-key",
                "--verbose",
                "--token",
                "-debug",
            ],
        );
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
