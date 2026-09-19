const API_SCHEMA_JSON: &str = include_str!("../../docs/next/api/herdr-api.schema.json");

use crate::api::schema::{EmptyParams, Method, Request};

pub(super) fn run_api_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(String::as_str) else {
        print_api_help();
        return Ok(2);
    };

    match subcommand {
        "schema" => api_schema(&args[1..]),
        "snapshot" => api_snapshot(&args[1..]),
        "usage-report" => usage_report(&args[1..]),
        "help" | "--help" | "-h" => {
            print_api_help();
            Ok(0)
        }
        _ => {
            print_api_help();
            Ok(2)
        }
    }
}

/// 官方用量报文的大小上限；超出的 passthrough 输入只透传不上报。
const USAGE_REPORT_MAX_BYTES: usize = 1024 * 1024;
/// passthrough 上报的 socket 写超时与整体等待上限：stdout 已经写完，这两个值只约束本进程
/// 还持有管道写端（原渲染器等 EOF）的时间；本机 Unix socket 连接加写入通常远低于 1 ms。
const USAGE_REPORT_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);
const USAGE_REPORT_SEND_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

/// `herdr api usage-report … --passthrough` 是 statusline 每次刷新都会跑的热路径：`main`
/// 用它跳过语言初始化等与本命令无关的启动开销。判定复用真正的参数解析器：只有解析结果是
/// 合法的 passthrough 调用才走热路径；`--help`、`--session`、未知参数等一律落回完整 CLI。
pub(crate) fn is_usage_report_passthrough(args: &[String]) -> bool {
    args.get(1).map(String::as_str) == Some("api")
        && args.get(2).map(String::as_str) == Some("usage-report")
        && matches!(
            parse_usage_report_args(&args[3..]),
            UsageReportArgs::Run {
                passthrough: true,
                ..
            }
        )
}

/// `--help` / `-h` 不在这里处理：`cli::maybe_run` 派发前已由 clap（`spec::print_requested_help`）
/// 接管，这里遇到它只可能是 `--` 之后的形态，按无效参数处理。
enum UsageReportArgs {
    Run {
        agent: Option<String>,
        account_id: String,
        passthrough: bool,
    },
    /// 参数无效；`passthrough` 记录是否出现过 `--passthrough`，无效参数也不能吞掉原渲染器的输入。
    Invalid { passthrough: bool },
}

fn parse_usage_report_args(args: &[String]) -> UsageReportArgs {
    let mut agent = None;
    let mut account_id = String::new();
    let mut passthrough = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--agent" | "--account" if index + 1 < args.len() => {
                if args[index] == "--agent" {
                    agent = Some(args[index + 1].clone());
                } else {
                    account_id = args[index + 1].clone();
                }
                index += 2;
            }
            "--passthrough" => {
                passthrough = true;
                index += 1;
            }
            _ => {
                return UsageReportArgs::Invalid {
                    passthrough: passthrough || args.iter().any(|arg| arg == "--passthrough"),
                }
            }
        }
    }
    UsageReportArgs::Run {
        agent,
        account_id,
        passthrough,
    }
}

fn usage_report(args: &[String]) -> std::io::Result<i32> {
    let pane_id = std::env::var("HERDR_PANE_ID").ok();
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    usage_report_with_io(
        args,
        &mut stdin.lock(),
        &mut stdout.lock(),
        pane_id,
        |request, detached| {
            if detached {
                // stdout 已回放并 flush：先让出 fd 1（管道写端），原渲染器立刻读到 EOF，再去投递——
                // 否则 server 忙（connect 阻塞）时渲染器要陪着等满 USAGE_REPORT_SEND_WAIT。让不出去
                // 只是退回到「等上报结束才 EOF」，不算失败。
                if let Err(error) = crate::platform::detach_stdout() {
                    tracing::debug!(%error, "让出 stdout 失败，原渲染器将等到上报结束");
                }
            }
            deliver_usage_report(request, detached)
        },
    )
}

/// 上报的投递方式由调用形态决定：passthrough（statusline 回调）下 fire-and-forget——只写
/// 不读、短超时，连不上或超时都静默；显式调用下同步等响应，把 server 的拒绝原因报给用户。
fn deliver_usage_report(request: Request, detached: bool) -> std::io::Result<()> {
    if detached {
        let Ok(client) = super::target::api_client() else {
            return Ok(());
        };
        let (sent, completed) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = client.send_request_without_reply(&request, USAGE_REPORT_SEND_TIMEOUT);
            let _ = sent.try_send(());
        });
        // 只等到写完为止（含连接阶段）；进程退出会关闭未完成的 socket，不拖住原渲染器读 EOF。
        let _ = completed.recv_timeout(USAGE_REPORT_SEND_WAIT);
        return Ok(());
    }
    super::target::api_client()?
        .request_value_with_timeout(&request, std::time::Duration::from_secs(2))
        .and_then(crate::api::client::parse_response_value)
        .map_err(super::api_client_error_to_io)?;
    Ok(())
}

/// `usage_report` 的可测试主体：stdin / stdout / pane 与投递方式都由调用方注入。passthrough
/// 下先把 stdin 原样交给 stdout（字节保真、含 >1 MiB 的流式部分），再决定是否上报；下游
/// 渲染器提前关管道（`BrokenPipe`）不是错误，也不影响上报。
fn usage_report_with_io(
    args: &[String],
    input: &mut impl std::io::Read,
    output: &mut impl std::io::Write,
    pane_id: Option<String>,
    deliver: impl FnOnce(Request, bool) -> std::io::Result<()>,
) -> std::io::Result<i32> {
    use std::io::Read;
    let (agent, account_id, passthrough) = match parse_usage_report_args(args) {
        UsageReportArgs::Invalid { passthrough: true } => {
            tolerate_closed_reader(std::io::copy(input, output).and_then(|_| output.flush()))?;
            return Ok(2);
        }
        UsageReportArgs::Invalid { passthrough: false } => {
            eprintln!(
                "usage: herdr api usage-report --agent <AGENT> [--account <ID>] [--passthrough]"
            );
            return Ok(2);
        }
        UsageReportArgs::Run {
            agent,
            account_id,
            passthrough,
        } => (agent, account_id, passthrough),
    };
    let mut bytes = Vec::new();
    input
        .by_ref()
        .take(USAGE_REPORT_MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    let oversized = bytes.len() > USAGE_REPORT_MAX_BYTES;
    if passthrough {
        let replay = output.write_all(&bytes).and_then(|()| {
            if oversized {
                std::io::copy(input, output)?;
            }
            output.flush()
        });
        tolerate_closed_reader(replay)?;
    }
    if oversized {
        return if passthrough {
            Ok(0)
        } else {
            Err(std::io::Error::other("官方用量报告超过大小限制"))
        };
    }
    let payload = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) if passthrough => return Ok(0),
        Err(_) => return Err(std::io::Error::other("需要官方 JSON 用量报文")),
    };
    if passthrough && pane_id.is_none() && account_id.is_empty() {
        return Ok(0);
    }
    let request = Request {
        id: "usage-report".into(),
        method: Method::AccountUsageReport(crate::api::schema::UsageReportParams {
            account_id,
            agent,
            pane_id,
            official_payload: Some(payload),
            snapshot: Default::default(),
        }),
    };
    deliver(request, passthrough)?;
    Ok(0)
}

/// 下游渲染器提前关掉管道读端时的写失败：透传已尽力，静默即可。
fn tolerate_closed_reader(result: std::io::Result<()>) -> std::io::Result<()> {
    match result {
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

fn api_schema(args: &[String]) -> std::io::Result<i32> {
    match args {
        [] => {
            print!("{}", schema_summary_text()?);
        }
        [flag] if flag == "--json" => {
            print!("{API_SCHEMA_JSON}");
        }
        [flag, path] if flag == "--output" => {
            write_schema_file(std::path::Path::new(path))?;
            println!(
                "{}",
                crate::i18n::fill(
                    crate::i18n::texts().cli_output.api_schema_written_fmt,
                    &[("path", path.as_str())]
                )
            );
        }
        [flag] if flag == "--output" => {
            eprintln!(
                "{}",
                crate::i18n::fill(
                    crate::i18n::texts().cli_errors.missing_value_for_fmt,
                    &[("flag", "--output")]
                )
            );
            return Ok(2);
        }
        [flag] if matches!(flag.as_str(), "help" | "--help" | "-h") => {
            print_api_schema_help();
        }
        [other] if other.starts_with('-') => {
            eprintln!(
                "{}",
                crate::i18n::fill(
                    crate::i18n::texts().cli_errors.unknown_option_fmt,
                    &[("option", other.as_str())]
                )
            );
            return Ok(2);
        }
        _ => {
            print_api_schema_help();
            return Ok(2);
        }
    }
    Ok(0)
}

fn api_snapshot(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("{}", crate::i18n::texts().cli_errors.api_snapshot_usage);
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:api:snapshot".into(),
        method: Method::SessionSnapshot(EmptyParams::default()),
    })?)
}

fn write_schema_file(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::write(path, API_SCHEMA_JSON)
}

fn schema_summary_text() -> std::io::Result<String> {
    let value: serde_json::Value = serde_json::from_str(API_SCHEMA_JSON)?;
    let protocol = value
        .get("protocol")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| std::io::Error::other("API schema is missing protocol"))?;
    let schema_version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| std::io::Error::other("API schema is missing schema_version"))?;
    let mut schemas: Vec<&str> = value
        .get("schemas")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| std::io::Error::other("API schema is missing schemas"))?
        .keys()
        .map(String::as_str)
        .collect();
    schemas.sort();

    Ok(crate::i18n::fill(
        crate::i18n::texts().cli_output.api_schema_summary_fmt,
        &[
            ("protocol", &protocol.to_string()),
            ("schema_version", &schema_version.to_string()),
            ("schemas", &schemas.join(", ")),
        ],
    ))
}

fn print_api_help() {
    eprintln!("herdr api commands:");
    eprintln!("  herdr api snapshot");
    eprintln!("  herdr api schema [--json | --output PATH]");
}

fn print_api_schema_help() {
    eprintln!("{}", crate::i18n::texts().cli_errors.api_schema_usage);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    /// 跑一次 `usage_report_with_io`，返回（退出码、stdout 字节、投递到的请求与是否 detached）。
    fn run(
        list: &[&str],
        input: &[u8],
        pane_id: Option<&str>,
        deliver_result: std::io::Result<()>,
    ) -> (
        std::io::Result<i32>,
        Vec<u8>,
        Option<(serde_json::Value, bool)>,
    ) {
        let delivered = RefCell::new(None);
        let mut output = Vec::new();
        let code = usage_report_with_io(
            &args(list),
            &mut std::io::Cursor::new(input.to_vec()),
            &mut output,
            pane_id.map(str::to_owned),
            |request, detached| {
                *delivered.borrow_mut() = Some((serde_json::to_value(&request).unwrap(), detached));
                deliver_result
            },
        );
        (code, output, delivered.into_inner())
    }

    #[test]
    fn usage_report_passthrough_replays_bytes_exactly_then_reports_detached() {
        // 无尾随换行、含非 ASCII 的报文：stdout 必须逐字节等于 stdin。
        let input = "{\"model\":{\"display_name\":\"Opus\"},\"cwd\":\"/tmp/路径\"}".as_bytes();
        let (code, output, delivered) = run(
            &["--agent", "claude", "--passthrough"],
            input,
            Some("w1:p1"),
            Ok(()),
        );
        assert_eq!(code.unwrap(), 0);
        assert_eq!(output, input);
        let (request, detached) = delivered.expect("有 pane 时上报");
        assert!(detached, "passthrough 走 fire-and-forget");
        assert_eq!(request["method"], "account.usage.report");
        assert_eq!(request["params"]["agent"], "claude");
        assert_eq!(request["params"]["pane_id"], "w1:p1");
        assert_eq!(request["params"]["official_payload"]["cwd"], "/tmp/路径");
        assert_eq!(request["params"]["account_id"], "");
    }

    #[test]
    fn usage_report_passthrough_streams_oversized_input_without_reporting() {
        let mut input = b"{\"pad\":\"".to_vec();
        input.resize(USAGE_REPORT_MAX_BYTES + 4096, b'x');
        input.extend_from_slice(b"\"}");
        let (code, output, delivered) = run(
            &["--agent", "claude", "--passthrough"],
            &input,
            Some("w1:p1"),
            Ok(()),
        );
        assert_eq!(code.unwrap(), 0);
        assert_eq!(output, input, ">1 MiB 也要完整透传");
        assert!(delivered.is_none(), "超限报文不上报");
        // 显式调用则是错误。
        let (code, output, delivered) = run(&["--agent", "claude"], &input, Some("w1:p1"), Ok(()));
        assert!(code.is_err());
        assert!(output.is_empty());
        assert!(delivered.is_none());
    }

    #[test]
    fn usage_report_passthrough_skips_report_without_pane_or_account() {
        let input = b"{\"model\":{}}";
        let (code, output, delivered) =
            run(&["--agent", "claude", "--passthrough"], input, None, Ok(()));
        assert_eq!(code.unwrap(), 0);
        assert_eq!(output, input);
        assert!(delivered.is_none(), "不在 pane 里、也没指定账号：只透传");
        let (code, _, delivered) = run(
            &[
                "--agent",
                "claude",
                "--account",
                "claude:work",
                "--passthrough",
            ],
            input,
            None,
            Ok(()),
        );
        assert_eq!(code.unwrap(), 0);
        assert_eq!(
            delivered.unwrap().0["params"]["account_id"],
            "claude:work",
            "显式账号不依赖 pane"
        );
    }

    #[test]
    fn usage_report_passthrough_replays_invalid_json_and_stays_quiet() {
        let input = b"not json \xff\xfe";
        let (code, output, delivered) = run(
            &["--agent", "claude", "--passthrough"],
            input,
            Some("w1:p1"),
            Ok(()),
        );
        assert_eq!(code.unwrap(), 0);
        assert_eq!(output, input, "坏 JSON 也逐字节透传");
        assert!(delivered.is_none());
        let (code, output, _) = run(&["--agent", "claude"], input, Some("w1:p1"), Ok(()));
        assert!(code.is_err(), "显式调用要求 JSON");
        assert!(output.is_empty());
    }

    #[test]
    fn usage_report_invalid_option_still_replays_stdin() {
        let input = b"{\"model\":{}}";
        let (code, output, delivered) = run(
            &["--agent", "claude", "--passthrough", "--bogus"],
            input,
            Some("w1:p1"),
            Ok(()),
        );
        assert_eq!(code.unwrap(), 2, "参数错误按用法错误退出");
        assert_eq!(output, input, "但原渲染器的输入不能被吞掉");
        assert!(delivered.is_none());
        // `--bogus` 出现在 `--passthrough` 之前同样透传。
        let (code, output, _) = run(&["--bogus", "--passthrough"], input, Some("w1:p1"), Ok(()));
        assert_eq!(code.unwrap(), 2);
        assert_eq!(output, input);
    }

    #[test]
    fn usage_report_passthrough_tolerates_renderer_closing_the_pipe() {
        struct ClosedPipe;
        impl std::io::Write for ClosedPipe {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let delivered = RefCell::new(false);
        let code = usage_report_with_io(
            &args(&["--agent", "claude", "--passthrough"]),
            &mut std::io::Cursor::new(b"{\"model\":{}}".to_vec()),
            &mut ClosedPipe,
            Some("w1:p1".into()),
            |_, _| {
                *delivered.borrow_mut() = true;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(code, 0, "下游提前关管道不是错误");
        assert!(delivered.into_inner(), "报文仍然上报");
    }

    #[test]
    fn usage_report_explicit_call_waits_for_reply_and_surfaces_errors() {
        let input = b"{\"model\":{}}";
        let (code, output, delivered) = run(
            &["--agent", "claude", "--account", "claude:work"],
            input,
            None,
            Err(std::io::Error::other("usage_binding_required")),
        );
        assert_eq!(code.unwrap_err().to_string(), "usage_binding_required");
        assert!(output.is_empty(), "非 passthrough 不回放 stdin");
        assert!(!delivered.unwrap().1, "显式调用同步等响应");
    }

    #[test]
    fn usage_report_passthrough_fast_path_matches_only_the_statusline_shape() {
        assert!(is_usage_report_passthrough(&args(&[
            "herdr",
            "api",
            "usage-report",
            "--agent",
            "claude",
            "--passthrough"
        ])));
        assert!(is_usage_report_passthrough(&args(&[
            "herdr",
            "api",
            "usage-report",
            "--passthrough",
            "--account",
            "claude:work"
        ])));
        let slow_paths: [&[&str]; 7] = [
            &["herdr", "api", "usage-report", "--agent", "claude"],
            &["herdr", "api", "usage-report", "--passthrough", "--help"],
            &["herdr", "api", "usage-report", "--passthrough", "--", "-h"],
            &[
                "herdr",
                "--session",
                "x",
                "api",
                "usage-report",
                "--passthrough",
            ],
            // `--session` 写在后面：交给完整路径的会话参数解析，不能被当成未知参数吞掉。
            &[
                "herdr",
                "api",
                "usage-report",
                "--passthrough",
                "--session",
                "x",
            ],
            // 把 flag 当成 --agent 的值：解析结果不是 passthrough，判定与解析器一致。
            &["herdr", "api", "usage-report", "--agent", "--passthrough"],
            &["herdr", "api", "usage-report", "--passthrough", "--bogus"],
        ];
        for shape in slow_paths {
            assert!(!is_usage_report_passthrough(&args(shape)), "{shape:?}");
        }
        assert!(!is_usage_report_passthrough(&args(&["herdr", "api"])));
    }

    /// 帮助由 clap 接管（`spec::print_requested_help` 在派发前拦下 `--help` / `-h`）；本模块
    /// 没有第二份帮助文案，`--` 之后的 `-h` 按无效参数处理。
    #[test]
    fn usage_report_help_belongs_to_clap() {
        assert!(matches!(
            parse_usage_report_args(&args(&["--help"])),
            UsageReportArgs::Invalid { passthrough: false }
        ));
        let (code, output, delivered) = run(&["--", "-h"], b"{}", Some("w1:p1"), Ok(()));
        assert_eq!(code.unwrap(), 2);
        assert!(output.is_empty());
        assert!(delivered.is_none());
    }

    #[test]
    fn schema_summary_text_stays_human_sized() {
        let _guard = crate::i18n::lang_guard(crate::i18n::Lang::En);
        let text = super::schema_summary_text().unwrap();
        assert!(text.contains("Herdr API schema"));
        assert!(text.contains("Use `herdr api schema --json`"));
        assert!(text.len() < 400);
    }
}
