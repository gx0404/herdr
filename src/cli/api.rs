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

fn usage_report(args: &[String]) -> std::io::Result<i32> {
    use std::io::{Read, Write};
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
            "--help" | "-h" => {
                println!("herdr api usage-report --agent <AGENT> [--account <ID>] [--passthrough]\n从 stdin 接收官方用量 JSON。默认通过 HERDR_PANE_ID 选择已绑定账号；不会保存原始报文。");
                return Ok(0);
            }
            _ => return Ok(2),
        }
    }
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if passthrough {
        std::io::stdout().write_all(&bytes)?;
        if bytes.len() > 1024 * 1024 {
            std::io::copy(&mut std::io::stdin(), &mut std::io::stdout())?;
            return Ok(0);
        }
        std::io::stdout().flush()?;
    }
    if bytes.len() > 1024 * 1024 {
        return Err(std::io::Error::other("官方用量报告超过大小限制"));
    }
    let payload = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) if passthrough => return Ok(0),
        Err(_) => return Err(std::io::Error::other("需要官方 JSON 用量报文")),
    };
    let pane_id = std::env::var("HERDR_PANE_ID").ok();
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
    if passthrough {
        let Ok(client) = super::target::api_client() else {
            return Ok(0);
        };
        let (sent, completed) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ =
                client.request_value_with_timeout(&request, std::time::Duration::from_millis(500));
            let _ = sent.try_send(());
        });
        // 总时限包括连接阶段；进程退出关闭未完成的 socket，不拖住原渲染器的 EOF。
        let _ = completed.recv_timeout(std::time::Duration::from_millis(750));
        return Ok(0);
    }
    super::target::api_client()?
        .request_value_with_timeout(&request, std::time::Duration::from_secs(2))
        .and_then(crate::api::client::parse_response_value)
        .map_err(super::api_client_error_to_io)?;
    Ok(0)
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
    #[test]
    fn schema_summary_text_stays_human_sized() {
        let _guard = crate::i18n::lang_guard(crate::i18n::Lang::En);
        let text = super::schema_summary_text().unwrap();
        assert!(text.contains("Herdr API schema"));
        assert!(text.contains("Use `herdr api schema --json`"));
        assert!(text.len() < 400);
    }
}
