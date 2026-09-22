//! 「快速添加」输入解析：`user@host:port`、`ssh://user@host:port` 或整条
//! `ssh …` 命令 → 机器表单字段。纯函数，不做 I/O。
//!
//! 只认机器表单能表达的参数（`-p` `-l` `-i` `-J` `-A`/`-a` 与对应的
//! `-o Key=Value`）；其余 ssh 选项按 OpenSSH 的 getopt 表判断是否带参数后
//! 跳过，目标之后的远程命令忽略（机器表单的 RemoteCommand 与命令行命令
//! 互斥，照填会让 herdr 的桥接命令失效）。标量取值与 OpenSSH 一致：先出现
//! 的生效，`-i` 可重复累加。

// 下一条提交（单页表单）接入快速输入框后删除本 allow。
#![allow(dead_code)]

use crate::client::endpoint::StrictHostKeyChecking;

/// 解析结果：除 `host` 外，每个字段只在输入里出现时才有值。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::client::shell) struct QuickInput {
    pub(in crate::client::shell) host: String,
    pub(in crate::client::shell) user: Option<String>,
    pub(in crate::client::shell) port: Option<u16>,
    pub(in crate::client::shell) identity_files: Vec<String>,
    /// `Some(空)` = 显式 `-J none`。
    pub(in crate::client::shell) proxy_jump: Option<Vec<String>>,
    pub(in crate::client::shell) forward_agent: Option<bool>,
    pub(in crate::client::shell) identities_only: Option<bool>,
    pub(in crate::client::shell) identity_agent: Option<String>,
    pub(in crate::client::shell) strict_host_key: Option<StrictHostKeyChecking>,
    pub(in crate::client::shell) server_alive_interval: Option<u16>,
    pub(in crate::client::shell) server_alive_count_max: Option<u16>,
    pub(in crate::client::shell) control_persist: Option<String>,
}

/// 解析失败的原因。界面只显示一条通用文案，枚举留给测试区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::client::shell) enum QuickParseError {
    Empty,
    UnbalancedQuote,
    MissingDestination,
    MissingOptionValue(char),
    InvalidPort,
    InvalidDestination,
    /// `user:password@host`：密码不进目录，直接拒绝。
    EmbeddedPassword,
}

/// OpenSSH `ssh` 的 getopt 表里**带参数**的选项字母（9.x）。表外字母一律
/// 按不带参数处理：宁可多吃一个标志，也不把后面的目标误当成它的参数。
const OPTIONS_WITH_VALUE: &str = "bceilmopBDEFIJLOPQRSwW";

/// 把粘贴进来的整段文本压成快速输入框的单行文本：去掉行尾续行符
/// （`\` + 换行），其余换行变空格。编辑器本身也会把换行归一成空格，但那样
/// 续行符会变成「转义的空格」，解析就错了，所以粘贴路径先过这一步。
pub(in crate::client::shell) fn flatten_pasted_command(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' if matches!(chars.peek(), Some('\n' | '\r')) => {
                // 续行：吞掉换行（含 CRLF），补一个分隔空格。
                if chars.next() == Some('\r') && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                output.push(' ');
            }
            '\r' | '\n' | '\t' => output.push(' '),
            _ => output.push(ch),
        }
    }
    output.trim().to_owned()
}

/// 解析快速输入框的内容。
pub(in crate::client::shell) fn parse_quick_input(
    input: &str,
) -> Result<QuickInput, QuickParseError> {
    let words = split_shell_words(input)?;
    let mut words = words.as_slice();
    // 从终端里连提示符一起复制的情况：`$ ssh host`。
    if let [prompt, rest @ ..] = words {
        if matches!(prompt.as_str(), "$" | "%" | "#" | ">") {
            words = rest;
        }
    }
    let Some(first) = words.first() else {
        return Err(QuickParseError::Empty);
    };
    if is_ssh_program(first) {
        words = &words[1..];
    }
    let mut parsed = QuickInput::default();
    let mut destination = None;
    let mut index = 0;
    let mut options_done = false;
    while index < words.len() {
        let word = &words[index];
        index += 1;
        if !options_done && word == "--" {
            options_done = true;
            continue;
        }
        if !options_done && word.len() > 1 && word.starts_with('-') {
            for (offset, letter) in word[1..].char_indices() {
                if OPTIONS_WITH_VALUE.contains(letter) {
                    // 选项值：同一个词里的剩余部分（`-p2222`），否则取下一个词。
                    let rest = &word[1 + offset + letter.len_utf8()..];
                    let value = if rest.is_empty() {
                        let value = words
                            .get(index)
                            .ok_or(QuickParseError::MissingOptionValue(letter))?;
                        index += 1;
                        value.as_str()
                    } else {
                        rest
                    };
                    apply_option(&mut parsed, letter, value)?;
                    break;
                }
                match letter {
                    'A' => set_first(&mut parsed.forward_agent, true),
                    'a' => set_first(&mut parsed.forward_agent, false),
                    _ => {}
                }
            }
            continue;
        }
        if destination.is_some() {
            // 目标之后第一个非选项词起是远程命令：整段忽略。
            break;
        }
        // OpenSSH 在目标之后还会继续解析选项（`ssh host -p 22`），直到遇到
        // 下一个非选项词。
        destination = Some(parse_destination(word)?);
    }
    let Some((user, host, port)) = destination else {
        return Err(QuickParseError::MissingDestination);
    };
    // 目标里的 user / port 与 `-l` / `-p` 同为「先到先得」：OpenSSH 先收命令行
    // 选项，目标里的值只在选项没给时生效。
    if let Some(user) = user {
        set_first(&mut parsed.user, user);
    }
    if let Some(port) = port {
        set_first(&mut parsed.port, port);
    }
    parsed.host = host;
    Ok(parsed)
}

fn is_ssh_program(word: &str) -> bool {
    let name = word.rsplit(['/', '\\']).next().unwrap_or(word);
    name == "ssh" || name.eq_ignore_ascii_case("ssh.exe")
}

fn set_first<T>(slot: &mut Option<T>, value: T) {
    if slot.is_none() {
        *slot = Some(value);
    }
}

fn parse_port(value: &str) -> Result<u16, QuickParseError> {
    match value.trim().parse::<u16>() {
        Ok(port) if port > 0 => Ok(port),
        _ => Err(QuickParseError::InvalidPort),
    }
}

fn parse_yes_no(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "yes" | "true" => Some(true),
        "no" | "false" => Some(false),
        _ => None,
    }
}

fn push_hops(parsed: &mut QuickInput, value: &str) {
    // ProxyJump 同样先到先得；`none` 表示显式不跳，也占住这个位置。
    if parsed.proxy_jump.is_some() {
        return;
    }
    let hops = if value.trim().eq_ignore_ascii_case("none") {
        Vec::new()
    } else {
        value
            .split(',')
            .map(str::trim)
            .filter(|hop| !hop.is_empty())
            .map(str::to_owned)
            .collect()
    };
    parsed.proxy_jump = Some(hops);
}

fn apply_option(parsed: &mut QuickInput, letter: char, value: &str) -> Result<(), QuickParseError> {
    match letter {
        'p' => {
            let port = parse_port(value)?;
            set_first(&mut parsed.port, port);
        }
        'l' if !value.is_empty() => set_first(&mut parsed.user, value.to_owned()),
        'i' if !value.is_empty() => parsed.identity_files.push(value.to_owned()),
        'J' => push_hops(parsed, value),
        'o' => apply_config_option(parsed, value)?,
        // 其余带参数的选项（-L / -D / -F / -E …）表单表达不了，跳过。
        _ => {}
    }
    Ok(())
}

/// `-o Key=Value`、`-o "Key Value"`、`-o "Key = Value"` 三种写法；键不分大小写。
fn apply_config_option(parsed: &mut QuickInput, option: &str) -> Result<(), QuickParseError> {
    let option = option.trim();
    let split = option
        .find(|ch: char| ch == '=' || ch.is_whitespace())
        .unwrap_or(option.len());
    let key = option[..split].to_ascii_lowercase();
    let value = option[split..]
        .trim_start_matches(|ch: char| ch == '=' || ch.is_whitespace())
        .trim();
    if value.is_empty() {
        return Ok(());
    }
    match key.as_str() {
        "port" => {
            let port = parse_port(value)?;
            set_first(&mut parsed.port, port);
        }
        "user" => set_first(&mut parsed.user, value.to_owned()),
        "identityfile" => parsed.identity_files.push(value.to_owned()),
        "identitiesonly" => {
            if let Some(flag) = parse_yes_no(value) {
                set_first(&mut parsed.identities_only, flag);
            }
        }
        "identityagent" => set_first(&mut parsed.identity_agent, value.to_owned()),
        "stricthostkeychecking" => {
            // herdr 只保存 ask / accept-new / yes；no / off 是关掉校验，不接。
            let checking = match value.to_ascii_lowercase().as_str() {
                "ask" => Some(StrictHostKeyChecking::Ask),
                "accept-new" => Some(StrictHostKeyChecking::AcceptNew),
                "yes" | "true" => Some(StrictHostKeyChecking::Yes),
                _ => None,
            };
            if let Some(checking) = checking {
                set_first(&mut parsed.strict_host_key, checking);
            }
        }
        "proxyjump" => push_hops(parsed, value),
        "forwardagent" => {
            if let Some(flag) = parse_yes_no(value) {
                set_first(&mut parsed.forward_agent, flag);
            }
        }
        "serveraliveinterval" => {
            if let Ok(interval) = value.parse::<u16>() {
                set_first(&mut parsed.server_alive_interval, interval);
            }
        }
        "serveralivecountmax" => {
            if let Ok(count) = value.parse::<u16>() {
                set_first(&mut parsed.server_alive_count_max, count);
            }
        }
        "controlpersist" => set_first(&mut parsed.control_persist, value.to_owned()),
        _ => {}
    }
    Ok(())
}

/// 解析出的目标：`(user, host, port)`。
type Destination = (Option<String>, String, Option<u16>);

/// `[user@]host[:port]`、`ssh://[user@]host[:port][/]`、`[v6]:port`。裸 IPv6
/// （多个冒号、无方括号）整体当主机。
fn parse_destination(word: &str) -> Result<Destination, QuickParseError> {
    let authority = match word.get(..6) {
        Some(scheme) if scheme.eq_ignore_ascii_case("ssh://") => &word[6..],
        _ => word,
    };
    let authority = authority.trim_end_matches('/');
    let (user, host_port) = match authority.rsplit_once('@') {
        Some((userinfo, _)) if userinfo.contains(':') => {
            return Err(QuickParseError::EmbeddedPassword);
        }
        Some((userinfo, host_port)) => (Some(userinfo.to_owned()), host_port),
        None => (None, authority),
    };
    let (host, port) = if let Some(rest) = host_port.strip_prefix('[') {
        let close = rest.find(']').ok_or(QuickParseError::InvalidDestination)?;
        let port = match &rest[close + 1..] {
            "" => None,
            tail => Some(parse_port(
                tail.strip_prefix(':')
                    .ok_or(QuickParseError::InvalidDestination)?,
            )?),
        };
        (&rest[..close], port)
    } else if host_port.matches(':').count() > 1 {
        (host_port, None)
    } else if let Some((host, port)) = host_port.split_once(':') {
        (host, Some(parse_port(port)?))
    } else {
        (host_port, None)
    };
    let valid_text = |text: &str| {
        !text.is_empty()
            && !text.starts_with('-')
            && !text
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control() || ch == '@')
    };
    if !valid_text(host) || user.as_deref().is_some_and(|user| !valid_text(user)) {
        return Err(QuickParseError::InvalidDestination);
    }
    Ok((user, host.to_owned(), port))
}

/// 按 POSIX shell 的规则切词：单引号原样、双引号内只转义 `" \ $ \``、引号
/// 外反斜杠转义下一个字符。引号不配对报错。
fn split_shell_words(input: &str) -> Result<Vec<String>, QuickParseError> {
    let mut words = Vec::new();
    let mut current: Option<String> = None;
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            ch if ch.is_whitespace() => {
                if let Some(word) = current.take() {
                    words.push(word);
                }
            }
            '\'' => {
                let word = current.get_or_insert_with(String::new);
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => word.push(ch),
                        None => return Err(QuickParseError::UnbalancedQuote),
                    }
                }
            }
            '"' => {
                let word = current.get_or_insert_with(String::new);
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.peek() {
                            Some(&next @ ('"' | '\\' | '$' | '`')) => {
                                chars.next();
                                word.push(next);
                            }
                            _ => word.push('\\'),
                        },
                        Some(ch) => word.push(ch),
                        None => return Err(QuickParseError::UnbalancedQuote),
                    }
                }
            }
            '\\' => {
                let word = current.get_or_insert_with(String::new);
                word.push(chars.next().unwrap_or('\\'));
            }
            ch => current.get_or_insert_with(String::new).push(ch),
        }
    }
    if let Some(word) = current {
        words.push(word);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &str) -> QuickInput {
        parse_quick_input(input).unwrap_or_else(|error| panic!("{input:?}: {error:?}"))
    }

    #[test]
    fn plain_destinations_split_user_host_and_port() {
        assert_eq!(parse("build.example").host, "build.example");
        let parsed = parse("dev@build.example:2222");
        assert_eq!(parsed.host, "build.example");
        assert_eq!(parsed.user.as_deref(), Some("dev"));
        assert_eq!(parsed.port, Some(2222));
        let parsed = parse("ssh://ops@10.0.0.5:2200/");
        assert_eq!(
            (parsed.user.as_deref(), parsed.host.as_str(), parsed.port),
            (Some("ops"), "10.0.0.5", Some(2200))
        );
        let parsed = parse("root@[fe80::1]:22");
        assert_eq!((parsed.host.as_str(), parsed.port), ("fe80::1", Some(22)));
        // 裸 IPv6 整体是主机。
        let parsed = parse("fe80::1");
        assert_eq!((parsed.host.as_str(), parsed.port), ("fe80::1", None));
        // 前后空白与终端提示符。
        assert_eq!(parse("  $ ssh  box  ").host, "box");
    }

    #[test]
    fn ssh_commands_fill_every_supported_option() {
        let parsed = parse(
            "ssh -p 2222 -i ~/.ssh/k -i '/keys/with space' -J jump1,jump2 -A \
             -o IdentitiesOnly=yes -o 'StrictHostKeyChecking accept-new' \
             -oServerAliveInterval=30 -o ServerAliveCountMax=4 -o ControlPersist=10m \
             -o IdentityAgent=/run/agent.sock dev@build.example",
        );
        assert_eq!(parsed.host, "build.example");
        assert_eq!(parsed.user.as_deref(), Some("dev"));
        assert_eq!(parsed.port, Some(2222));
        assert_eq!(parsed.identity_files, vec!["~/.ssh/k", "/keys/with space"]);
        assert_eq!(
            parsed.proxy_jump,
            Some(vec!["jump1".to_owned(), "jump2".to_owned()])
        );
        assert_eq!(parsed.forward_agent, Some(true));
        assert_eq!(parsed.identities_only, Some(true));
        assert_eq!(
            parsed.strict_host_key,
            Some(StrictHostKeyChecking::AcceptNew)
        );
        assert_eq!(parsed.server_alive_interval, Some(30));
        assert_eq!(parsed.server_alive_count_max, Some(4));
        assert_eq!(parsed.control_persist.as_deref(), Some("10m"));
        assert_eq!(parsed.identity_agent.as_deref(), Some("/run/agent.sock"));
    }

    #[test]
    fn getopt_forms_and_unrelated_options_are_handled() {
        // 值粘在字母后、多个标志合并、带参数的无关选项被跳过。
        let parsed = parse("/usr/bin/ssh -tt -CAp2222 -L 8080:localhost:80 -lops -F cfg host");
        assert_eq!(parsed.host, "host");
        assert_eq!(parsed.port, Some(2222));
        assert_eq!(parsed.user.as_deref(), Some("ops"));
        assert_eq!(parsed.forward_agent, Some(true));
        // 目标之后的选项照样生效，第一个非选项词起是远程命令、整段忽略。
        let parsed = parse("ssh host -p 2200 -i key tmux attach -p 1");
        assert_eq!(parsed.port, Some(2200));
        assert_eq!(parsed.identity_files, vec!["key"]);
        // `--` 之后不再看选项；没有 `ssh` 前缀的一串参数同样按 ssh 参数解析。
        assert_eq!(parse("ssh -- host").host, "host");
        assert_eq!(parse("-p 2201 box").port, Some(2201));
        assert_eq!(parse("ssh.EXE box").host, "box");
    }

    #[test]
    fn first_value_wins_like_openssh() {
        let parsed = parse("ssh -p 22 -o Port=2222 -l a b@host:2200");
        assert_eq!(parsed.port, Some(22));
        assert_eq!(parsed.user.as_deref(), Some("a"));
        let parsed = parse("ssh b@host:2200");
        assert_eq!(parsed.port, Some(2200));
        assert_eq!(parsed.user.as_deref(), Some("b"));
        // herdr 不支持的取值不填：StrictHostKeyChecking=no 是关掉校验。
        let parsed = parse("ssh -o StrictHostKeyChecking=no -o ForwardAgent=/tmp/s -a host");
        assert_eq!(parsed.strict_host_key, None);
        assert_eq!(parsed.forward_agent, Some(false));
        let parsed = parse("ssh -J none -o ProxyJump=j host");
        assert_eq!(
            parsed.proxy_jump,
            Some(Vec::new()),
            "-J none 占位且先到先得"
        );
    }

    #[test]
    fn malformed_input_reports_why() {
        use QuickParseError as E;
        let cases: &[(&str, QuickParseError)] = &[
            ("", E::Empty),
            ("   ", E::Empty),
            ("$", E::Empty),
            ("ssh", E::MissingDestination),
            ("ssh -p 22", E::MissingDestination),
            ("ssh host -p", E::MissingOptionValue('p')),
            ("ssh 'host", E::UnbalancedQuote),
            ("ssh \"host", E::UnbalancedQuote),
            ("host:99999", E::InvalidPort),
            ("host:0", E::InvalidPort),
            ("host:ssh", E::InvalidPort),
            ("ssh -p abc host", E::InvalidPort),
            ("ssh -o Port=x host", E::InvalidPort),
            ("user:secret@host", E::EmbeddedPassword),
            ("@host", E::InvalidDestination),
            ("user@", E::InvalidDestination),
            ("a@b@host", E::InvalidDestination),
            ("[fe80::1", E::InvalidDestination),
            ("[fe80::1]x", E::InvalidDestination),
            (":22", E::InvalidDestination),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_quick_input(input), Err(*expected), "{input:?}");
        }
    }

    #[test]
    fn pasted_multi_line_commands_flatten_before_parsing() {
        let pasted = "ssh -p 2222 \\\n  -i ~/.ssh/k \\\r\n  dev@build.example\n";
        let flat = flatten_pasted_command(pasted);
        assert_eq!(flat, "ssh -p 2222    -i ~/.ssh/k    dev@build.example");
        let parsed = parse(&flat);
        assert_eq!(parsed.port, Some(2222));
        assert_eq!(parsed.identity_files, vec!["~/.ssh/k"]);
        assert_eq!(parsed.host, "build.example");
        // 双引号内的转义与引号外的反斜杠。
        let parsed = parse(r#"ssh -i "/a \"b\" c" -i /d\ e host"#);
        assert_eq!(parsed.identity_files, vec![r#"/a "b" c"#, "/d e"]);
    }
}
