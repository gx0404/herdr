//! `herdr machine fs <profile> <op>`: remote filesystem operations through
//! the profile's managed `sftp -b -` channel (see `remote::fs`). The profile
//! selector follows the same label-or-id resolution as the global
//! `--machine` prefix. Downloads are written atomically with private
//! permissions; uploads are capped at the small-file limit.

use serde::Serialize;

use crate::client::endpoint::EndpointCatalog;
use crate::remote::{RemoteFileStat, RemoteFs};

fn usage_text() -> &'static str {
    crate::i18n::texts().cli_errors.machine_fs_usage
}

#[derive(Serialize)]
struct FsListRow<'a> {
    name: &'a str,
    kind: &'static str,
    size: u64,
    mode: Option<String>,
    modified: &'a str,
    link_target: Option<&'a str>,
}

#[derive(Serialize)]
struct FsStatRow {
    kind: &'static str,
    size: u64,
    mode: Option<String>,
    modified: String,
    link_target: Option<String>,
}

fn mode_text(mode: Option<u32>) -> Option<String> {
    mode.map(|mode| format!("{:o}", mode & 0o7777))
}

pub(super) fn run_fs_command(args: &[String]) -> std::io::Result<i32> {
    let Some(selector) = args.first() else {
        eprintln!("{}", usage_text());
        return Ok(2);
    };
    if matches!(selector.as_str(), "help" | "--help" | "-h") {
        println!("{}", usage_text());
        return Ok(0);
    }
    let profiles = EndpointCatalog::load_profiles().map_err(std::io::Error::other)?;
    let profile = match super::target::resolve_machine(&profiles, selector) {
        Ok(profile) => profile.clone(),
        Err(error) => {
            eprintln!("{}{error}", crate::i18n::texts().cli_errors.error_prefix);
            return Ok(2);
        }
    };
    let Some(operation) = args.get(1).map(String::as_str) else {
        eprintln!("{}", usage_text());
        return Ok(2);
    };
    let rest = &args[2..];
    match operation {
        "ls" => with_fs(&profile, |fs| list(fs, rest)),
        "stat" => with_fs(&profile, |fs| stat(fs, rest)),
        "cat" => with_fs(&profile, |fs| cat(fs, rest)),
        "get" => with_fs(&profile, |fs| get(fs, rest)),
        "put" => with_fs(&profile, |fs| put(fs, rest)),
        "mkdir" => with_fs(&profile, |fs| mkdir(fs, rest)),
        "mv" => with_fs(&profile, |fs| mv(fs, rest)),
        "rm" => with_fs(&profile, |fs| rm(fs, rest)),
        _ => {
            eprintln!("{}", usage_text());
            Ok(2)
        }
    }
}

/// Connects on demand: parse/usage errors never open an SSH connection.
fn with_fs(
    profile: &crate::client::endpoint::SavedSshEndpoint,
    run: impl FnOnce(&RemoteFs) -> std::io::Result<i32>,
) -> std::io::Result<i32> {
    match RemoteFs::connect(profile) {
        Ok(fs) => run(&fs),
        Err(error) => {
            eprintln!("{}{error}", crate::i18n::texts().cli_errors.error_prefix);
            Ok(1)
        }
    }
}

fn fail(error: std::io::Error) -> std::io::Result<i32> {
    eprintln!("{}{error}", crate::i18n::texts().cli_errors.error_prefix);
    Ok(1)
}

fn usage() -> std::io::Result<i32> {
    eprintln!("{}", usage_text());
    Ok(2)
}

fn parse_path_and_json(args: &[String]) -> Option<(&str, bool)> {
    match args {
        [path] => Some((path, false)),
        [path, flag] if flag == "--json" => Some((path, true)),
        _ => None,
    }
}

fn list(fs: &RemoteFs, args: &[String]) -> std::io::Result<i32> {
    let Some((path, json)) = parse_path_and_json(args) else {
        return usage();
    };
    let entries = match fs.list_dir(path) {
        Ok(entries) => entries,
        Err(error) => return fail(error),
    };
    let rows: Vec<FsListRow> = entries
        .iter()
        .map(|entry| FsListRow {
            name: &entry.name,
            kind: entry.kind.as_str(),
            size: entry.size,
            mode: mode_text(entry.mode),
            modified: &entry.modified,
            link_target: entry.link_target.as_deref(),
        })
        .collect();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).map_err(std::io::Error::other)?
        );
        return Ok(0);
    }
    for row in rows {
        let target = row.link_target.unwrap_or("");
        println!(
            "{}\t{}\t{}\t{}\t{}{}",
            row.kind,
            row.size,
            row.mode.unwrap_or_default(),
            row.modified,
            row.name,
            target,
        );
    }
    Ok(0)
}

fn stat(fs: &RemoteFs, args: &[String]) -> std::io::Result<i32> {
    let Some((path, json)) = parse_path_and_json(args) else {
        return usage();
    };
    let stat = match fs.stat(path) {
        Ok(stat) => stat,
        Err(error) => return fail(error),
    };
    let row = stat_row(&stat);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&row).map_err(std::io::Error::other)?
        );
        return Ok(0);
    }
    println!("{}\t{}\t{}", row.kind, row.size, row.modified);
    Ok(0)
}

fn stat_row(stat: &RemoteFileStat) -> FsStatRow {
    FsStatRow {
        kind: stat.kind.as_str(),
        size: stat.size,
        mode: mode_text(stat.mode),
        modified: stat.modified.clone(),
        link_target: stat.link_target.clone(),
    }
}

fn cat(fs: &RemoteFs, args: &[String]) -> std::io::Result<i32> {
    let [path] = args else {
        return usage();
    };
    let data = match fs.read_small_file(path) {
        Ok(data) => data,
        Err(error) => return fail(error),
    };
    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&data)?;
    stdout.flush()?;
    Ok(0)
}

fn get(fs: &RemoteFs, args: &[String]) -> std::io::Result<i32> {
    let [remote, local] = args else {
        return usage();
    };
    let data = match fs.read_small_file(remote) {
        Ok(data) => data,
        Err(error) => return fail(error),
    };
    // Atomic with private permissions; refuses to replace through symlinks.
    if let Err(error) = crate::client::endpoint::store_private_json(
        std::path::Path::new(local),
        &data,
        "downloaded file",
    ) {
        return fail(std::io::Error::other(error));
    }
    Ok(0)
}

fn put(fs: &RemoteFs, args: &[String]) -> std::io::Result<i32> {
    let [local, remote] = args else {
        return usage();
    };
    let metadata = match std::fs::metadata(local) {
        Ok(metadata) => metadata,
        Err(error) => return fail(error),
    };
    if !metadata.is_file() {
        return fail(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            crate::i18n::fill(
                crate::i18n::texts()
                    .cli_errors
                    .machine_fs_local_not_file_fmt,
                &[("path", local)],
            ),
        ));
    }
    if metadata.len() > crate::remote::MAX_SMALL_FILE_BYTES {
        return fail(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            crate::i18n::fill(
                crate::i18n::texts().cli_errors.machine_fs_too_large_fmt,
                &[
                    ("size", &metadata.len().to_string()),
                    ("limit", &crate::remote::MAX_SMALL_FILE_BYTES.to_string()),
                ],
            ),
        ));
    }
    let data = match std::fs::read(local) {
        Ok(data) => data,
        Err(error) => return fail(error),
    };
    match fs.write_small_file(remote, &data) {
        Ok(()) => Ok(0),
        Err(error) => fail(error),
    }
}

fn mkdir(fs: &RemoteFs, args: &[String]) -> std::io::Result<i32> {
    let (path, parents) = match args {
        [path] => (path.as_str(), false),
        [path, flag] if flag == "--parents" || flag == "-p" => (path.as_str(), true),
        _ => return usage(),
    };
    match fs.mkdir(path, parents) {
        Ok(()) => Ok(0),
        Err(error) => fail(error),
    }
}

fn mv(fs: &RemoteFs, args: &[String]) -> std::io::Result<i32> {
    let [from, to] = args else {
        return usage();
    };
    match fs.rename(from, to) {
        Ok(()) => Ok(0),
        Err(error) => fail(error),
    }
}

fn rm(fs: &RemoteFs, args: &[String]) -> std::io::Result<i32> {
    let (path, recursive) = match args {
        [path] => (path.as_str(), false),
        [path, flag] if flag == "--recursive" || flag == "-r" => (path.as_str(), true),
        _ => return usage(),
    };
    match fs.delete(path, recursive) {
        Ok(()) => Ok(0),
        Err(error) => fail(error),
    }
}
