//! One-off remote command execution for `herdr machine exec`.
//!
//! Unlike the managed background channels (endpoint bridge, sftp batch) an
//! exec call is a single foreground `ssh` invocation: the same managed ssh
//! config and non-interactive option family apply, but stdio is inherited so
//! the remote command's output streams straight to the operator's terminal
//! and ssh's exit status (the remote command's status, or 255 on a transport
//! failure) becomes the CLI's. There is deliberately no timeout: the command
//! is operator-driven and may legitimately run for a long time.

use std::io;
use std::process::{Command, Stdio};

use super::attach::{
    apply_managed_channel_options, apply_noninteractive_ssh_options, write_managed_ssh_config,
};
use crate::client::endpoint::SavedSshEndpoint;

/// Runs `command` on the profile's target through one ssh invocation.
/// `-T` disables pseudo-terminal allocation: exec is a batch channel like
/// the sftp path, not an interactive shell.
pub(crate) fn exec_saved_ssh(profile: &SavedSshEndpoint, command: &[String]) -> io::Result<i32> {
    let profile_options = super::saved::saved_profile_ssh_options(profile)?;
    let mut config = write_managed_ssh_config(profile_options.as_ref())?;
    // 前台一次性命令不建控制主连接：受管配置按调用新建，ControlPath 每次不同，
    // 开着只会留下后台常驻 master（HERDR-MACH-004）。
    config.options.control_path = None;
    let mut ssh = Command::new("ssh");
    apply_managed_channel_options(&mut ssh, Some(&config.options));
    apply_noninteractive_ssh_options(
        &mut ssh,
        config.options.server_alive_interval,
        config.options.server_alive_count_max,
        config.options.strict_host_key_checking,
    );
    ssh.arg("-T")
        .arg("--")
        .arg(&profile.target)
        .args(command)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = ssh.status().map_err(|error| {
        super::error::classify_and_wrap(
            io::Error::new(
                error.kind(),
                format!("failed to start the ssh client: {error}"),
            ),
            &profile.target,
            profile.identity_file.first().cloned(),
        )
    })?;
    Ok(status.code().unwrap_or(255))
}
