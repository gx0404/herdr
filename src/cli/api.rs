const API_SCHEMA_JSON: &str = include_str!("../../docs/next/api/herdr-api.schema.json");

use crate::api::schema::{AgentActivityReadParams, AgentTarget, EmptyParams, Method, Request};

pub(super) fn run_api_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(String::as_str) else {
        print_api_help();
        return Ok(2);
    };

    match subcommand {
        "schema" => api_schema(&args[1..]),
        "snapshot" => api_snapshot(&args[1..]),
        "activity-read" => api_activity_read(&args[1..]),
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

/// `herdr api activity-read` 的取值参数（`--flag=value` 形态经 `expand_equals_args` 拆开）。
const ACTIVITY_READ_VALUE_FLAGS: &[&str] = &[
    "--agent",
    "--external-id",
    "--node-id",
    "--cursor",
    "--max-bytes",
];

/// `herdr api activity-read` 读谁：pane 里的 agent（agent 名或 pane id，先经
/// `agent.get` 解析成 pane id），或外部来源条目。
#[derive(Debug, PartialEq, Eq)]
enum ActivityReadTarget {
    Agent(String),
    External(String),
}

/// `herdr api activity-read` 的参数，与 `agent.activity.read` 一一对应（`follow` 除外：
/// 一次性读取不跟随）。
#[derive(Debug, PartialEq, Eq)]
struct ActivityReadArgs {
    target: ActivityReadTarget,
    node_id: Option<String>,
    cursor: Option<String>,
    max_bytes: Option<u32>,
}

/// 解析 `herdr api activity-read` 的参数；用法错误给出要打印的一行说明。帮助由 clap
/// 接管（`spec::print_requested_help` 在派发前拦下 `--help` / `-h`）。
fn parse_activity_read_args(args: &[String]) -> Result<ActivityReadArgs, String> {
    let t = &crate::i18n::texts().cli_errors;
    let args = super::expand_equals_args(args, ACTIVITY_READ_VALUE_FLAGS);
    let mut agent = None;
    let mut external = None;
    let mut node_id = None;
    let mut cursor = None;
    let mut max_bytes = None;
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        if !ACTIVITY_READ_VALUE_FLAGS.contains(&flag) {
            return Err(if flag.starts_with('-') {
                crate::i18n::fill(t.unknown_option_fmt, &[("option", flag)])
            } else {
                crate::i18n::fill(t.unexpected_argument_fmt, &[("argument", flag)])
            });
        }
        let Some(value) = args.get(index + 1) else {
            return Err(crate::i18n::fill(
                t.missing_value_for_fmt,
                &[("flag", flag)],
            ));
        };
        match flag {
            "--agent" => agent = Some(value.clone()),
            "--external-id" => external = Some(value.clone()),
            "--node-id" => node_id = Some(value.clone()),
            "--cursor" => cursor = Some(value.clone()),
            _ => {
                max_bytes =
                    Some(super::parse_u32_flag(flag, value).map_err(|error| error.to_string())?)
            }
        }
        index += 2;
    }
    let target = match (agent, external) {
        (Some(agent), None) => ActivityReadTarget::Agent(agent),
        (None, Some(external)) => ActivityReadTarget::External(external),
        _ => return Err(t.api_activity_read_usage.to_owned()),
    };
    Ok(ActivityReadArgs {
        target,
        node_id,
        cursor,
        max_bytes,
    })
}

/// 读取请求：`pane_id` / `external_id` 恰给其一（由调用方按目标种类给出）。
fn activity_read_request(
    pane_id: Option<String>,
    external_id: Option<String>,
    args: ActivityReadArgs,
) -> Request {
    Request {
        id: "cli:api:activity-read".into(),
        method: Method::AgentActivityRead(AgentActivityReadParams {
            pane_id,
            external_id,
            node_id: args.node_id,
            cursor: args.cursor,
            max_bytes: args.max_bytes,
            follow: false,
        }),
    }
}

/// `herdr api activity-read`：`agent.activity.read` 的 CLI 入口，应答整份以 JSON 输出
/// （错误应答打到 stderr、退出码 1，与其它 `herdr api` 子命令一致）。
fn api_activity_read(args: &[String]) -> std::io::Result<i32> {
    let args = match parse_activity_read_args(args) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };
    let (pane_id, external_id) = match &args.target {
        ActivityReadTarget::External(external_id) => (None, Some(external_id.clone())),
        ActivityReadTarget::Agent(target) => {
            let response = super::send_request(&Request {
                id: "cli:api:activity-read:resolve".into(),
                method: Method::AgentGet(AgentTarget {
                    target: target.clone(),
                }),
            })?;
            if response.get("error").is_some() {
                return super::print_response(&response);
            }
            let Some(pane_id) = response["result"]["agent"]["pane_id"].as_str() else {
                eprintln!(
                    "{}",
                    crate::i18n::texts().cli_errors.api_activity_read_no_pane_id
                );
                return Ok(1);
            };
            (Some(pane_id.to_owned()), None)
        }
    };
    super::print_response(&super::send_request(&activity_read_request(
        pane_id,
        external_id,
        args,
    ))?)
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
    eprintln!(
        "  herdr api activity-read (--agent TARGET | --external-id ID) [--node-id ID] [--cursor CURSOR] [--max-bytes N]"
    );
}

fn print_api_schema_help() {
    eprintln!("{}", crate::i18n::texts().cli_errors.api_schema_usage);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    /// 交接 T8 RL11：`herdr api activity-read` 的参数与 `agent.activity.read` 对应；
    /// `--flag=value` 形态同样接受；目标恰给其一，其余按用法错误报出（英文界面下是英文）。
    #[test]
    fn activity_read_args_mirror_the_method_params() {
        let _guard = crate::i18n::lang_guard(crate::i18n::Lang::En);
        let parsed = parse_activity_read_args(&args(&[
            "--external-id",
            "zcode:sess_1",
            "--node-id=sess_subagent_a",
            "--cursor",
            "t:10",
            "--max-bytes=4096",
        ]))
        .expect("合法参数");
        assert_eq!(
            parsed.target,
            ActivityReadTarget::External("zcode:sess_1".into())
        );
        let request = serde_json::to_value(activity_read_request(
            None,
            Some("zcode:sess_1".into()),
            parsed,
        ))
        .expect("可序列化");
        assert_eq!(request["method"], "agent.activity.read");
        assert_eq!(
            request["params"],
            serde_json::json!({
                "external_id": "zcode:sess_1",
                "node_id": "sess_subagent_a",
                "cursor": "t:10",
                "max_bytes": 4096,
            })
        );
        assert_eq!(
            parse_activity_read_args(&args(&["--agent", "claude-main"]))
                .expect("只给 agent")
                .target,
            ActivityReadTarget::Agent("claude-main".into())
        );

        let usage = crate::i18n::texts().cli_errors.api_activity_read_usage;
        for (list, expected) in [
            (&[][..], usage.to_owned()),
            (
                &["--agent", "a", "--external-id", "zcode:s"][..],
                usage.to_owned(),
            ),
            (&["--node-id", "n"][..], usage.to_owned()),
            (&["--agent"][..], "missing value for --agent".to_owned()),
            (
                &["--agent", "a", "--follow"][..],
                "unknown option: --follow".to_owned(),
            ),
            (
                &["--agent", "a", "stray"][..],
                "unexpected argument: stray".to_owned(),
            ),
            (
                &["--agent", "a", "--max-bytes", "lots"][..],
                "invalid value for --max-bytes: lots".to_owned(),
            ),
        ] {
            assert_eq!(
                parse_activity_read_args(&args(list)),
                Err(expected),
                "{list:?}"
            );
        }
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
