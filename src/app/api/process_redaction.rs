//! `pane.process_info` 输出层的凭据打码（RL12）。
//!
//! 能连上 socket 的调用方都能查任何窗格的前台进程，而命令行里常带认证头、token、
//! 口令。进程采集（`platform` / `detect`）保持原样——检测要看完整 argv；这里只在
//! 把结果交给调用方之前，把疑似凭据的值换成 `[REDACTED]`：程序名、选项名与参数
//! 位置原样保留，调用方仍看得出跑的是什么。覆盖：
//!
//! - 凭据类选项的值：`--token abc`、`--token=abc`、`--api-key abc`、`-password=abc`、
//!   `--oauth2-bearer abc`；
//! - 「名 + 分隔符 + 口令」：curl 的 `--user` / `--proxy-user` / `-u` / `-U`（`:`）、
//!   Samba 客户端的 `-U`（`%`）、lftp 的 `-u`（`,`），只打分隔符后，且只在这条命令
//!   里出现过这些程序时才算；
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
//! - curl/curlie 的明确请求体选项：表单凭据字段与嵌套 JSON；URL 查询与 fragment
//!   也接受相对 URL，OAuth `code` 只在这些 URL 参数里视为凭据。
//!
//! 按凭据形态与已知程序选项判断；`-p<口令>` 这类语义不明的短选项不在覆盖范围内。

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
const CREDENTIAL_WORDS: [&str; 9] = [
    "key", "pass", "auth", "sig", "pw", "jwt", "passin", "passout", "creds",
];

/// 这些词只在前面是 `_` 时才算：`MYSQL_PWD` 算，当前目录 `PWD`、`OLDPWD` 不算。
const CREDENTIAL_TAILS: [&str; 1] = ["pwd"];

/// 敏感请求头里保留、不打码的认证方案名。
const AUTH_SCHEMES: [&str; 6] = ["bearer", "basic", "token", "digest", "negotiate", "ntlm"];

/// 「名 + 分隔符 + 口令」写法：哪些程序的哪些选项取这种值、用什么分隔。`-u` 在别的
/// 程序里意思各不相同（`docker run -u 1000:1000` 是 uid:gid，`rsync -u` 是开关），
/// 所以只在这条命令里前面出现过这些程序时才按口令处理。
struct UserPasswordRule {
    programs: &'static [&'static str],
    options: &'static [&'static str],
    separator: char,
}

const USER_PASSWORD_RULES: [UserPasswordRule; 3] = [
    // curl，以及原样转发 curl 选项的 curlie：`-u name:pw`、`-U proxy:pw`。
    UserPasswordRule {
        programs: &["curl", "curlie"],
        options: &["-u", "-U", "--user", "--proxy-user"],
        separator: ':',
    },
    // Samba 客户端：`-U DOMAIN\name%pw`（文档写法，进程表里常见的口令泄漏）。
    UserPasswordRule {
        programs: &["smbclient", "rpcclient", "smbcacls", "smbget"],
        options: &["-U", "--user"],
        separator: '%',
    },
    // lftp：`-u name,pw`。
    UserPasswordRule {
        programs: &["lftp"],
        options: &["-u"],
        separator: ',',
    },
];

/// 通常的程序路径原样保留；setproctitle 等把整条命令写入 argv[0] 时，处理其中参数。
pub(super) fn redact_program(program: &str) -> String {
    if program.contains(char::is_whitespace) {
        redact_line(program, 1, 0, Quotes::Windows)
    } else {
        program.to_owned()
    }
}

/// 前台进程的 argv 与 cmdline 打码，包括被改写成整条命令的 argv[0]。
pub(super) fn redact_command(
    argv: Option<Vec<String>>,
    cmdline: Option<String>,
) -> (Option<Vec<String>>, Option<String>) {
    let redacted_argv = argv.as_deref().map(|argv| {
        let mut words = argv.to_vec();
        if let Some(program) = words.first_mut() {
            *program = redact_program(program);
        }
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
    /// 上一个词是 curl 的 `--user` / `-u` 一类：当前词是「名 + 分隔符 + 口令」。
    UserPassword(char),
    /// 上一个词是 PowerShell 的 `$env:NAME`（NAME 像凭据）：当前词若是 `=`，再下一个
    /// 词就是值（`$env:GH_TOKEN = '…'`）。
    Assignment,
    /// 上一个词是 `Bearer`，或值被 shell 拆到后面去的敏感头名（`Authorization:`）：
    /// 当前词若是认证方案名就保留、接着等下一个，否则它就是凭据本身。
    Credential,
    /// curl 明确指定的请求体参数；不读取 `@file` 引用。
    Body(BodyKind),
}

#[derive(Clone, Copy)]
enum BodyKind {
    Data { raw: bool },
    UrlEncoded,
    Form { literal: bool },
    Json,
}

/// 从下标 `first` 起逐词打码，原位替换。`words` 是一条命令。
fn redact_words(words: &mut [String], first: usize, depth: usize) {
    let mut pending = Pending::None;
    let mut user_password = None;
    for (index, word) in words.iter_mut().enumerate() {
        // 记下这条命令里最近出现的、取「名:口令」写法的程序（含不打码的 argv[0]，
        // 也含 `sudo curl …`、`docker run curlimages/curl …` 里靠后的程序名）。
        if let Some(rule) = user_password_rule(word) {
            user_password = Some(rule);
        }
        if index < first {
            continue;
        }
        let (redacted, next) = redact_word(word, pending, depth, user_password);
        if let Some(redacted) = redacted {
            *word = redacted;
        }
        pending = next;
    }
}

/// 词是不是取「名:口令」写法的程序：取路径最后一段，去掉 `.exe` 与命令替换的
/// 前缀（`$(curl`），不分大小写。
fn user_password_rule(word: &str) -> Option<&'static UserPasswordRule> {
    let name = word.rsplit(['/', '\\']).next()?;
    let name = name
        .trim_start_matches(['$', '(', '`'])
        .to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    USER_PASSWORD_RULES
        .iter()
        .find(|rule| rule.programs.contains(&name))
}

/// 打码一个词：返回替换值（不用打码时为 `None`）与它对下一个词的约定。
/// `user_password` 是这条命令里前面出现过的、取「名:口令」写法的程序。
fn redact_word(
    word: &str,
    pending: Pending,
    depth: usize,
    user_password: Option<&UserPasswordRule>,
) -> (Option<String>, Pending) {
    if let Pending::Body(kind) = pending {
        return (redact_body(word, kind), Pending::None);
    }
    // 值的位置上出现选项，说明前一个词只是开关：约定作废，按选项处理。等着凭据时，
    // 以 `-` 开头的随机串（`--token -Xk9…`）仍是值。
    let awaits_secret = matches!(pending, Pending::Value | Pending::Credential);
    if looks_like_option(word) && !(awaits_secret && looks_like_dash_secret(word)) {
        return redact_option(word, depth, user_password);
    }
    match pending {
        Pending::Value => return (redacted_secret_word(word), Pending::None),
        Pending::Header => return redact_header(word, depth),
        Pending::UserPassword(separator) => {
            return (redact_user_password(word, separator), Pending::None);
        }
        Pending::Credential if is_auth_scheme(word) => return (None, Pending::Credential),
        Pending::Credential => return (redacted_secret_word(word), Pending::None),
        Pending::Assignment if word == "=" => return (None, Pending::Value),
        Pending::Assignment | Pending::None | Pending::Body(_) => {}
    }
    // 带空白的参数是一段命令行（`sh -c 'TOKEN=… codex'`、`pwsh -Command '…'`），不能
    // 整个当成一个赋值或请求头：逐词再过一遍。
    if word.contains(char::is_whitespace) {
        return (redact_text(word, depth), Pending::None);
    }
    // Windows 风格开关：`/token:…`、`/p:Password=…`。
    if let Some((name, separator, value)) = split_switch(word) {
        return (
            redact_option_value(name, separator, value, depth, None),
            Pending::None,
        );
    }
    if let Some((name, key, value)) = split_assignment(word) {
        return redact_assignment(name, key, value, depth);
    }
    // httpie / xh 的请求头是位置参数：`Authorization:Bearer …`、`x-api-key:…`。
    if is_positional_header(word) {
        return redact_header(word, depth);
    }
    if word.eq_ignore_ascii_case("bearer") || ends_with_sensitive_header_name(word) {
        return (None, Pending::Credential);
    }
    // PowerShell 环境或哈希表带空格的赋值；只有后续真出现 `=` 才消费值。
    if assignment_name(word).is_some_and(is_credential_name) {
        return (None, Pending::Assignment);
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

fn redact_option(
    word: &str,
    depth: usize,
    user_password: Option<&UserPasswordRule>,
) -> (Option<String>, Pending) {
    if word == "--" {
        return (None, Pending::None);
    }
    let curl = user_password.is_some_and(|rule| rule.programs.contains(&"curl"));
    if curl {
        if let Some(kind) = body_option(word) {
            return (None, Pending::Body(kind));
        }
        for prefix in ["-d", "-F"] {
            if let Some(body) = word.strip_prefix(prefix) {
                return (
                    redact_body(
                        body,
                        if prefix == "-F" {
                            BodyKind::Form { literal: false }
                        } else {
                            BodyKind::Data { raw: false }
                        },
                    )
                    .map(|body| format!("{prefix}{body}")),
                    Pending::None,
                );
            }
        }
    }
    if word == "-H" || word == "--header" {
        return (None, Pending::Header);
    }
    if let Some(rule) = user_password.filter(|rule| rule.options.contains(&word)) {
        return (None, Pending::UserPassword(rule.separator));
    }
    if !word.starts_with("--") {
        // curl 允许 `-H` 紧贴值：`-HAuthorization: …`。
        if let Some(header) = word.strip_prefix("-H") {
            let (redacted, next) = redact_header(header, depth);
            return (redacted.map(|header| format!("-H{header}")), next);
        }
        // 短选项紧贴值：`-uadmin:pw`、`-UCORP\name%pw`；带 `=` 的（Go 风格的
        // `-url=…`）不是这种写法。
        if let Some(rule) = user_password {
            for short in rule.options.iter().filter(|option| option.len() == 2) {
                if let Some(value) = word.strip_prefix(short) {
                    if value.contains(rule.separator) && !value.contains('=') {
                        let redacted = redact_user_password(value, rule.separator);
                        return (
                            redacted.map(|value| format!("{short}{value}")),
                            Pending::None,
                        );
                    }
                }
            }
        }
    }
    // `--name=value`，以及 PowerShell 的冒号参数 `-Token:value`。
    match word.find(['=', ':']) {
        Some(index) => {
            let (name, rest) = word.split_at(index);
            let (separator, value) = rest.split_at(1);
            if curl && separator == "=" {
                if let Some(kind) = body_option(name) {
                    return (
                        redact_body(value, kind).map(|body| format!("{name}={body}")),
                        Pending::None,
                    );
                }
            }
            if name == "--header" && separator == "=" {
                let (redacted, next) = redact_header(value, depth);
                return (redacted.map(|header| format!("--header={header}")), next);
            }
            (
                redact_option_value(name, separator, value, depth, user_password),
                Pending::None,
            )
        }
        None if is_credential_name(word) => (None, Pending::Value),
        None => (None, Pending::None),
    }
}

fn body_option(option: &str) -> Option<BodyKind> {
    match option {
        "--json" => Some(BodyKind::Json),
        "--data" | "--data-ascii" | "--data-binary" | "-d" => Some(BodyKind::Data { raw: false }),
        "--data-raw" => Some(BodyKind::Data { raw: true }),
        "--data-urlencode" => Some(BodyKind::UrlEncoded),
        "--form" | "-F" => Some(BodyKind::Form { literal: false }),
        "--form-string" => Some(BodyKind::Form { literal: true }),
        _ => None,
    }
}

/// 只解析 curl 的明确请求体；超过 64 KiB、JSON 深度超过 16 或 JSON 不完整时
/// 整体遮蔽。文件引用只保留路径，绝不读取文件或尝试展开 shell。
fn redact_body(body: &str, kind: BodyKind) -> Option<String> {
    if body.is_empty() {
        return None;
    }
    // curl 官方语义：data-raw/form-string 的 @ 是字面；form 的 name=@file/<file
    // 与 urlencode 的 name@file 才是文件。单字段内容里的 & 不是字段分隔符。
    let single_field = match kind {
        BodyKind::Json | BodyKind::Data { raw: false } if body.starts_with('@') => return None,
        BodyKind::Form { literal } => {
            let (name, value) = body.split_once('=')?;
            if !literal && value.starts_with(['@', '<']) {
                return None;
            }
            Some((name, value))
        }
        BodyKind::UrlEncoded => {
            // 无 = 时是 name@file/@file 或无字段名内容；不打开文件，也不猜字段名。
            let (name, value) = body.split_once('=')?;
            Some((name, value))
        }
        _ => None,
    };
    if body.len() > 65_536 {
        return Some(REDACTED.into());
    }
    if let Some((name, value)) = single_field {
        return if is_credential_name(name) && !value.is_empty() {
            Some(format!("{name}={REDACTED}"))
        } else {
            // 非凭据字段仍可能携带重定向 URL；保留单字段的 & 语义。
            redact_urls(value).map(|value| format!("{name}={value}"))
        };
    }
    if matches!(kind, BodyKind::Json) || body.trim_start().starts_with(['{', '[']) {
        let Ok(mut value) = serde_json::from_str::<serde_json::Value>(body) else {
            return Some(REDACTED.into());
        };
        return match redact_json_value(&mut value, 0) {
            Ok(false) => None,
            Ok(true) => Some(serde_json::to_string(&value).unwrap_or_else(|_| REDACTED.into())),
            Err(()) => Some(REDACTED.into()),
        };
    }
    let params = redact_params(body, false);
    redact_urls(params.as_deref().unwrap_or(body)).or(params)
}

fn redact_json_value(value: &mut serde_json::Value, depth: usize) -> Result<bool, ()> {
    if depth > 16 {
        return Err(());
    }
    let mut changed = false;
    match value {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                if is_credential_name(key) && !value.is_null() {
                    *value = serde_json::Value::String(REDACTED.into());
                    changed = true;
                } else {
                    changed |= redact_json_value(value, depth + 1)?;
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                changed |= redact_json_value(value, depth + 1)?;
            }
        }
        serde_json::Value::String(text) => {
            if let Some(redacted) = redact_urls(text) {
                *text = redacted;
                changed = true;
            }
        }
        _ => {}
    }
    Ok(changed)
}

/// 表单与 URL 参数共用凭据键规则；OAuth code 仅在 URL 参数中视为凭据。
fn redact_params(params: &str, url: bool) -> Option<String> {
    let mut changed = false;
    let params = params
        .split('&')
        .map(|param| match param.split_once('=') {
            Some((name, value))
                if !value.is_empty()
                    && (is_credential_name(name) || (url && name.eq_ignore_ascii_case("code"))) =>
            {
                changed = true;
                let (_, tail) = split_closing(value);
                format!("{name}={REDACTED}{tail}")
            }
            _ => param.to_owned(),
        })
        .collect::<Vec<_>>();
    changed.then(|| params.join("&"))
}

/// 选项或开关与值写在一起（`--token=…`、`-Token:…`、`/p:Password=…`）：凭据类
/// 名字整个值打码，curl 的 `--user=` 一类只打分隔符后，其余的值再按赋值 / 普通文本
/// 看一遍。
fn redact_option_value(
    name: &str,
    separator: &str,
    value: &str,
    depth: usize,
    user_password: Option<&UserPasswordRule>,
) -> Option<String> {
    let redacted = if let Some(rule) = user_password.filter(|rule| rule.options.contains(&name)) {
        redact_user_password(value, rule.separator)?
    } else if is_credential_name(name.trim_start_matches('/')) {
        redacted_secret_word(value)?
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
        redacted_secret_word(value).map(|value| format!("{name}:{lead}{value}")),
        Pending::None,
    )
}

/// 值后面紧跟的收尾符号（PowerShell 哈希表的 `}`、命令替换的 `)`，以及 `;`、`,`）
/// 不属于凭据：分出来留在原处。
fn split_closing(value: &str) -> (&str, &str) {
    let core = value.trim_end_matches(['}', ')', ';', ',']);
    (core, &value[core.len()..])
}

/// 整个值是凭据时的打码：收尾符号留着；只剩收尾符号时不动。
fn redacted_secret_word(value: &str) -> Option<String> {
    let (core, tail) = split_closing(value);
    (!core.is_empty()).then(|| format!("{}{tail}", redacted_secret(core)))
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

/// 「名 + 分隔符 + 口令」只打分隔符后；只有用户名时不动。
fn redact_user_password(value: &str, separator: char) -> Option<String> {
    let (user, password) = value.split_once(separator)?;
    (!password.is_empty()).then(|| format!("{user}{separator}{REDACTED}"))
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
    Some((assignment_name(key)?, key, value))
}

fn assignment_name(key: &str) -> Option<&str> {
    let name = key.rsplit([':', '/', '.', '{', '$', '@']).next()?;
    let mut chars = name.chars();
    let first = chars.next()?;
    ((first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-')))
    .then_some(name)
}

fn redact_assignment(
    name: &str,
    key: &str,
    value: &str,
    depth: usize,
) -> (Option<String>, Pending) {
    if !is_credential_name(name) {
        if is_positional_header(value) {
            let (redacted, pending) = redact_header(value, depth);
            return (redacted.map(|value| format!("{key}={value}")), pending);
        }
        return (
            redact_text(value, depth).map(|value| format!("{key}={value}")),
            Pending::None,
        );
    }
    // httpie 的 `名字==值`（查询参数）保留第二个 `=`；收尾符号留在值后面。
    let secret = value.trim_start_matches('=');
    let eq = &value[..value.len() - secret.len()];
    // 值只剩认证方案名：凭据被切到了下一个词（`AUTH="Bearer x"` 在命令行里切开后）。
    if is_auth_scheme(split_closing(secret).0) {
        return (None, Pending::Credential);
    }
    (
        redacted_secret_word(secret).map(|secret| format!("{key}={eq}{secret}")),
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
        // 协议名是 `://` 前面连着的一串协议字符；前面紧挨的可能是多字节字符，
        // 按字符而不是按字节往回找。
        let scheme_start = rest[..index]
            .char_indices()
            .rev()
            .find(|(_, c)| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
            .map_or(0, |(position, c)| position + c.len_utf8());
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

/// 查询与 fragment 参数均可能携带 OAuth 凭据；相对 URL 也处理，不要求 scheme。
fn redact_url_query(text: &str) -> Option<String> {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    let mut changed = false;
    while let Some(marker) = rest.find(['?', '#']) {
        let fragment = rest.as_bytes()[marker] == b'#';
        output.push_str(&rest[..=marker]);
        rest = &rest[marker + 1..];
        // 只有首个 ? 引入 query；其后的 ? 属于值。fragment 里的 ?/# 也属于值。
        let end = rest
            .find(|ch: char| (!fragment && ch == '#') || ch.is_whitespace())
            .unwrap_or(rest.len());
        let params = &rest[..end];
        if let Some(redacted) = redact_params(params, true) {
            output.push_str(&redacted);
            changed = true;
        } else {
            output.push_str(params);
        }
        rest = &rest[end..];
    }
    output.push_str(rest);
    changed.then_some(output)
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

/// 按空白切词：引号里的空白不切。Shell 模式识别引号外的反斜杠转义空白，Windows
/// 原始行仍保留反斜杠（路径里到处都有）。只寻找打码词，不执行或展开 shell。
fn split_words(line: &str, quotes: Quotes) -> Vec<Word> {
    let mut words = Vec::new();
    let mut current: Option<Word> = None;
    let mut open_quote: Option<char> = None;
    let mut next_starts_command = true;
    let mut chars = line.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
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
        if ch == '\\' && matches!(quotes, Quotes::Shell) && open_quote.is_none() {
            if let Some(&(escaped_at, escaped)) = chars.peek().filter(|(_, ch)| ch.is_whitespace())
            {
                chars.next();
                word.offsets.push((word.text.len(), escaped_at));
                word.text.push(escaped);
                word.span.end = escaped_at + escaped.len_utf8();
                continue;
            }
        }
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
    fn rl12_body_fields_keep_url_redaction() {
        for option in [
            "--data",
            "--data-raw",
            "--data-urlencode",
            "--form",
            "--form-string",
        ] {
            for (body, expected) in [
                (
                    "redirect=https://example.test/?access_token=s3cr3t-url",
                    "redirect=https://example.test/?access_token=[REDACTED]",
                ),
                (
                    "redirect=https://user:s3cr3t-pass@example.test/",
                    "redirect=https://user:[REDACTED]@example.test/",
                ),
            ] {
                assert_redacted(&["curl", option, body], &["curl", option, expected]);
            }
        }
    }

    #[test]
    fn rl12_question_marks_inside_url_secrets_remain_secret() {
        for (url, expected) in [
            (
                "https://example.test/?token=first?s3cr3t-tail&keep=yes",
                "https://example.test/?token=[REDACTED]&keep=yes",
            ),
            (
                "https://example.test/#token=first?s3cr3t-tail&keep=yes",
                "https://example.test/#token=[REDACTED]&keep=yes",
            ),
            (
                "https://example.test/?token=first?s3cr3t-tail#access_token=next?s3cr3t-other",
                "https://example.test/?token=[REDACTED]#access_token=[REDACTED]",
            ),
        ] {
            assert_redacted(&["curl", url], &["curl", expected]);
        }
    }

    #[test]
    fn rl12_curl_literal_bodies_and_file_references_are_distinct() {
        assert_redacted(
            &[
                "curl",
                "--data-raw",
                "@prefix=x&password=s3cr3t-raw&name=keep",
            ],
            &[
                "curl",
                "--data-raw",
                "@prefix=x&password=[REDACTED]&name=keep",
            ],
        );
        assert_redacted(
            &[
                "curl",
                "--form-string",
                "password=@s3cr3t-literal&still-secret",
            ],
            &["curl", "--form-string", "password=[REDACTED]"],
        );
        assert_redacted(
            &[
                "curl",
                "--data-urlencode",
                "password=s3cr3t-encoded&still-secret",
            ],
            &["curl", "--data-urlencode", "password=[REDACTED]"],
        );
        for input in [
            words(&["curl", "--form", "token=@/not-a-real-file/body"]),
            words(&["curl", "-F", "token=</not-a-real-file/body"]),
            words(&["curl", "--data-urlencode", "token@/not-a-real-file/body"]),
            words(&["curl", "--json", "@/not-a-real-file/body"]),
            words(&["curl", "--data", "@/not-a-real-file/body"]),
        ] {
            assert_eq!(
                redact_command(Some(input.clone()), None),
                (Some(input), None)
            );
        }
    }

    #[test]
    fn rl12_single_field_options_do_not_split_literal_ampersands() {
        for option in ["--form-string", "--data-urlencode"] {
            let input = words(&["curl", option, "message=keep&password=is-part-of-message"]);
            assert_eq!(
                redact_command(Some(input.clone()), None),
                (Some(input), None)
            );
        }
    }

    #[test]
    fn rl12_url_query_and_fragment_forms_do_not_leak() {
        for url in [
            "example.test/path?api_key=s3cr3t-url&page=2",
            "https://example.test/#access_token=s3cr3t-fragment&state=keep",
            "https://example.test/callback?code=s3cr3t-oauth&state=keep",
            "https://example.test/?api_key=s3cr3t-one#access_token=s3cr3t-two",
        ] {
            let (argv, line) = redact_command(Some(words(&["curl", url])), None);
            let argv = argv.expect("argv");
            assert!(!argv[1].contains("s3cr3t"), "{argv:?}");
            assert!(argv[1].contains(REDACTED));
            assert!(line.is_none());
        }
        assert_redacted(
            &[
                "build",
                "--code",
                "ordinary",
                "https://example.test/#heading",
            ],
            &[
                "build",
                "--code",
                "ordinary",
                "https://example.test/#heading",
            ],
        );
    }

    #[test]
    fn rl12_explicit_form_and_json_bodies_do_not_leak() {
        for input in [
            words(&["curl", "--data", "user=keep&password=s3cr3t-form"]),
            words(&["curl", "-dpassword=s3cr3t-form&name=keep"]),
            words(&["curl", "--data=user=keep&password=s3cr3t-form"]),
            words(&[
                "curl",
                "--json",
                r#"{"nested":[{"api_key":"s3cr3t-json"}],"name":"keep"}"#,
            ]),
            words(&["curl", r#"--json={"password":"s3cr3t-json","name":"keep"}"#]),
            words(&[
                "sh",
                "-c",
                r#"curl --json '{"api_key":"s3cr3t-json","name":"keep"}'"#,
            ]),
        ] {
            let (argv, line) = redact_command(Some(input.clone()), Some(input.join(" ")));
            assert!(
                !argv.expect("argv").join(" ").contains("s3cr3t"),
                "{input:?}"
            );
            let line = line.expect("cmdline");
            assert!(!line.contains("s3cr3t"), "{input:?}");
            assert!(line.contains("keep"), "非敏感字段仍可诊断：{line}");
        }
    }

    #[test]
    fn rl12_body_limits_fail_closed_without_reading_files_or_rewriting_other_options() {
        for body in [
            r#"{"api_key":"s3cr3t-incomplete"#.to_owned(),
            format!("{{\"padding\":\"{}s3cr3t-large\"}}", "x".repeat(65_536)),
            format!("{}\"s3cr3t-deep\"{}", "[".repeat(100), "]".repeat(100)),
        ] {
            let (argv, _) = redact_command(Some(words(&["curl", "--json", &body])), None);
            assert_eq!(argv.expect("argv")[2], REDACTED);
        }
        let input = words(&[
            "curl",
            "--data",
            "@/not-a-real-file/body.json",
            "--output",
            "ordinary",
        ]);
        assert_eq!(
            redact_command(Some(input.clone()), None),
            (Some(input), None)
        );
    }

    #[test]
    fn rl12_common_argument_forms_do_not_leak() {
        for input in [
            words(&["client", "--creds", "s3cr3t-basic"]),
            words(&["sh", "-c", r"curl -H Authorization:\ Basic\ s3cr3t-header"]),
            words(&[
                "pwsh",
                "-Command",
                "Invoke-WebRequest -Headers @{Authorization = 'Basic s3cr3t-ps'}",
            ]),
            words(&[
                "git",
                "-c",
                "http.extraHeader=AUTHORIZATION:basic s3cr3t-git",
                "fetch",
            ]),
            words(&["worker --token s3cr3t-title"]),
        ] {
            let (argv, cmdline) = redact_command(Some(input.clone()), Some(input.join(" ")));
            assert!(
                !argv.expect("argv").join(" ").contains("s3cr3t"),
                "argv leak: {input:?}"
            );
            assert!(
                !cmdline.expect("cmdline").contains("s3cr3t"),
                "cmdline leak: {input:?}"
            );
        }
    }

    #[test]
    fn rl12_argument_fixes_preserve_non_secret_values_and_scheme() {
        assert_redacted(
            &[
                "git",
                "-c",
                "http.extraHeader=AUTHORIZATION:basic s3cr3t-git",
                "fetch",
            ],
            &[
                "git",
                "-c",
                "http.extraHeader=AUTHORIZATION:basic [REDACTED]",
                "fetch",
            ],
        );
        for input in [
            words(&[r"C:\Program Files\O'Brien\client.exe", "--name", "ordinary"]),
            words(&["pwsh", "-Command", "Write-Output Authorization ordinary"]),
        ] {
            assert_eq!(
                redact_command(Some(input.clone()), Some(input.join(" "))),
                (Some(input.clone()), Some(input.join(" ")))
            );
        }
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

    /// 复审轻级：`-u` / `-U` / `--user` 的「名 + 分隔符 + 口令」只对认这种写法的程序
    /// 生效（curl 一族用 `:`，Samba 客户端用 `%`，lftp 用 `,`），看这条命令里前面出现过
    /// 哪个程序；`docker run -u 1000:1000`、`rsync -u host:/src` 不是口令。
    #[test]
    fn user_password_options_only_apply_to_programs_that_take_them() {
        let docker = [
            "docker",
            "run",
            "-u",
            "1000:1000",
            "--user",
            "0:0",
            "alpine",
            "id",
        ];
        assert_redacted(&docker, &docker);
        let rsync = ["rsync", "-u", "host:/src", "/dst"];
        assert_redacted(&rsync, &rsync);
        assert_redacted(
            &[
                "sudo",
                "-u",
                "root",
                "curl",
                "-u",
                "admin:s3cr3t-1",
                "https://example.invalid",
            ],
            &[
                "sudo",
                "-u",
                "root",
                "curl",
                "-u",
                "admin:[REDACTED]",
                "https://example.invalid",
            ],
        );
        assert_redacted(
            &[
                "sh",
                "-c",
                "docker run -u 1000:1000 img && curl -u admin:s3cr3t-2 https://example.invalid",
            ],
            &[
                "sh",
                "-c",
                "docker run -u 1000:1000 img && curl -u admin:[REDACTED] https://example.invalid",
            ],
        );
        assert_redacted(
            &[r"C:\tools\curl.exe", "-u", "admin:s3cr3t-3"],
            &[r"C:\tools\curl.exe", "-u", "admin:[REDACTED]"],
        );
        assert_redacted(
            &["smbclient", "//server/share", "-U", r"CORP\admin%s3cr3t-4"],
            &[
                "smbclient",
                "//server/share",
                "-U",
                r"CORP\admin%[REDACTED]",
            ],
        );
        assert_redacted(
            &[
                "lftp",
                "-u",
                "admin,s3cr3t-5",
                "sftp://files.example.invalid",
            ],
            &[
                "lftp",
                "-u",
                "admin,[REDACTED]",
                "sftp://files.example.invalid",
            ],
        );
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

    /// 复审轻级：值后面紧跟的收尾符号（PowerShell 哈希表的 `}`、命令替换的 `)`）
    /// 不属于凭据，打码后留在原处。
    #[test]
    fn closing_punctuation_after_a_secret_is_kept() {
        assert_redacted(
            &[
                "pwsh",
                "-c",
                "Invoke-RestMethod -Headers @{Authorization='Bearer s3cr3t-1'} https://example.invalid",
            ],
            &[
                "pwsh",
                "-c",
                "Invoke-RestMethod -Headers @{Authorization='Bearer [REDACTED]'} https://example.invalid",
            ],
        );
        assert_redacted(
            &["sh", "-c", "echo $(curl --token s3cr3t-2)"],
            &["sh", "-c", "echo $(curl --token [REDACTED])"],
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

    /// URL 前面紧挨着多字节字符时照常打码，找协议名不能按字节切到字符中间。
    #[test]
    fn urls_after_multibyte_text_are_redacted_without_panicking() {
        assert_redacted(
            &[
                "echo",
                "见https://s3cr3t-1@example.invalid/x",
                "链接：https://u:s3cr3t-2@example.invalid",
            ],
            &[
                "echo",
                "见https://[REDACTED]@example.invalid/x",
                "链接：https://u:[REDACTED]@example.invalid",
            ],
        );
    }

    /// 进程命令行是任意文本：打码对任何输入都不能 panic（它跑在 server 的 API 路径上）。
    /// 用固定种子的伪随机串覆盖引号、分隔符、多字节字符与各种标点的组合。
    #[test]
    fn redaction_never_panics_on_arbitrary_text() {
        const PIECES: [&str; 32] = [
            "a",
            "Z",
            "0",
            "=",
            ":",
            "/",
            "@",
            "'",
            "\"",
            " ",
            ";",
            "&",
            "|",
            "-",
            "--",
            "见",
            "：",
            "\n",
            "?",
            "#",
            "{",
            "}",
            "$env:",
            "\\",
            "token",
            "Bearer",
            "https://",
            "-H",
            "Authorization:",
            "-u",
            "\t",
            ",",
        ];
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as usize
        };
        for _ in 0..20_000 {
            let len = 1 + next() % 12;
            let text: String = (0..len).map(|_| PIECES[next() % PIECES.len()]).collect();
            let argv = vec!["prog".to_owned(), text.clone(), text.clone()];
            let _ = redact_command(Some(argv.clone()), Some(argv.join(" ")));
            let _ = redact_command(None, Some(text.clone()));
            let _ = redact_command(Some(vec![text.clone()]), Some(format!("\"{text}")));
        }
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
