//! Saved-profile SSH connection options and their ssh-config rendering.

use crate::client::endpoint::{ProxyJumpHop, SavedSshEndpoint, StrictHostKeyChecking};

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

/// Connection-relevant fields of a saved SSH profile, resolved and ready to
/// render into a managed ssh config. Presentation metadata (group, tags,
/// color) never reaches SSH.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProfileSshOptions {
    pub(crate) port: Option<u16>,
    pub(crate) user: Option<String>,
    pub(crate) identity_file: Vec<String>,
    pub(crate) identities_only: Option<bool>,
    pub(crate) identity_agent: Option<String>,
    pub(crate) strict_host_key_checking: Option<StrictHostKeyChecking>,
    pub(crate) proxy_jump: Vec<String>,
    pub(crate) forward_agent: Option<bool>,
    pub(crate) server_alive_interval: Option<u16>,
    pub(crate) server_alive_count_max: Option<u16>,
    pub(crate) control_persist: Option<String>,
    pub(crate) remote_command: Option<String>,
}

impl ProfileSshOptions {
    /// Resolves a profile's connection options. ProxyJump hops that reference
    /// another saved profile are substituted with that profile's target;
    /// references are resolved one level deep so hop chains never recurse.
    /// Returns `None` when the profile carries no connection options at all.
    pub(crate) fn from_profile(
        profile: &SavedSshEndpoint,
        profiles: &[SavedSshEndpoint],
    ) -> Result<Option<Self>, String> {
        if !profile.has_connection_options() {
            return Ok(None);
        }
        let mut proxy_jump = Vec::with_capacity(profile.proxy_jump.len());
        for hop in &profile.proxy_jump {
            match hop {
                ProxyJumpHop::Target(target) => proxy_jump.push(target.clone()),
                ProxyJumpHop::Profile(id) => {
                    let referenced = profiles
                        .iter()
                        .find(|candidate| &candidate.id == id)
                        .ok_or_else(|| {
                            format!("SSH proxy jump references unknown endpoint profile {id}")
                        })?;
                    proxy_jump.push(referenced.target.clone());
                }
            }
        }
        Ok(Some(Self {
            port: profile.port,
            user: profile.user.clone(),
            identity_file: profile.identity_file.clone(),
            identities_only: profile.identities_only,
            identity_agent: profile.identity_agent.clone(),
            strict_host_key_checking: profile.strict_host_key_checking,
            proxy_jump,
            forward_agent: profile.forward_agent,
            server_alive_interval: profile.server_alive_interval,
            server_alive_count_max: profile.server_alive_count_max,
            control_persist: profile.control_persist.clone(),
            remote_command: profile.remote_command.clone(),
        }))
    }

    /// ssh-config directives for the managed `Host *` fallback block. Values
    /// containing spaces or `~` are quoted; catalog validation has already
    /// rejected control characters and double quotes in rendered fields.
    pub(super) fn config_directives(&self) -> Vec<String> {
        let mut directives = Vec::new();
        if let Some(port) = self.port {
            directives.push(format!("Port {port}"));
        }
        if let Some(user) = &self.user {
            directives.push(format!("User {user}"));
        }
        for identity_file in &self.identity_file {
            directives.push(format!(
                "IdentityFile {}",
                super::attach::ssh_config_quote(identity_file)
            ));
        }
        if let Some(identities_only) = self.identities_only {
            directives.push(format!("IdentitiesOnly {}", yes_no(identities_only)));
        }
        if let Some(identity_agent) = &self.identity_agent {
            directives.push(format!(
                "IdentityAgent {}",
                super::attach::ssh_config_quote(identity_agent)
            ));
        }
        if let Some(strict_host_key_checking) = self.strict_host_key_checking {
            directives.push(format!(
                "StrictHostKeyChecking {}",
                strict_host_key_checking.as_ssh_value()
            ));
        }
        if !self.proxy_jump.is_empty() {
            directives.push(format!(
                "ProxyJump {}",
                super::attach::ssh_config_quote(&self.proxy_jump.join(","))
            ));
        }
        if let Some(forward_agent) = self.forward_agent {
            directives.push(format!("ForwardAgent {}", yes_no(forward_agent)));
        }
        if let Some(control_persist) = &self.control_persist {
            directives.push(format!("ControlPersist {control_persist}"));
        }
        if let Some(remote_command) = &self.remote_command {
            directives.push(format!(
                "RemoteCommand {}",
                super::attach::ssh_config_quote(remote_command)
            ));
            directives.push("RequestTTY yes".to_string());
        }
        directives
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::endpoint::{ProfileId, SshProfileOptions};

    fn profile_with(options: SshProfileOptions) -> SavedSshEndpoint {
        SavedSshEndpoint::with_options("label", "dev@build.example", "default", options)
            .expect("valid profile")
    }

    #[test]
    fn profile_without_connection_options_resolves_to_none() {
        let profile = profile_with(SshProfileOptions {
            group: Some("prod".into()),
            tags: vec!["ci".into()],
            color: Some("blue".into()),
            ..SshProfileOptions::default()
        });
        assert_eq!(
            ProfileSshOptions::from_profile(&profile, &[]).unwrap(),
            None,
            "presentation metadata alone must not change the connection"
        );
    }

    #[test]
    fn proxy_jump_profile_hops_resolve_against_the_catalog() {
        let jump = SavedSshEndpoint::new("jump", "jumpuser@bastion.example", "default").unwrap();
        let profile = profile_with(SshProfileOptions {
            proxy_jump: vec![
                ProxyJumpHop::Target("first.example".into()),
                ProxyJumpHop::Profile(jump.id.clone()),
            ],
            ..SshProfileOptions::default()
        });
        let resolved = ProfileSshOptions::from_profile(&profile, std::slice::from_ref(&jump))
            .unwrap()
            .expect("connection options present");
        assert_eq!(
            resolved.proxy_jump,
            vec![
                "first.example".to_string(),
                "jumpuser@bastion.example".to_string()
            ]
        );
        assert_eq!(
            resolved.config_directives(),
            vec!["ProxyJump \"first.example,jumpuser@bastion.example\"".to_string()]
        );
    }

    #[test]
    fn dangling_proxy_jump_profile_reference_is_an_error() {
        let profile = profile_with(SshProfileOptions {
            proxy_jump: vec![ProxyJumpHop::Profile(
                ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap(),
            )],
            ..SshProfileOptions::default()
        });
        let error = ProfileSshOptions::from_profile(&profile, &[]).unwrap_err();
        assert!(error.contains("unknown endpoint profile"), "{error}");
    }

    #[test]
    fn directives_cover_every_connection_field() {
        let options = ProfileSshOptions {
            port: Some(2222),
            user: Some("dev".into()),
            identity_file: vec!["~/.ssh/id build".into()],
            identities_only: Some(false),
            identity_agent: Some("~/.ssh/agent.sock".into()),
            strict_host_key_checking: Some(StrictHostKeyChecking::Ask),
            proxy_jump: vec!["bastion".into()],
            forward_agent: Some(false),
            server_alive_interval: Some(45),
            server_alive_count_max: Some(3),
            control_persist: Some("600".into()),
            remote_command: Some("tmux new -A".into()),
        };
        assert_eq!(
            options.config_directives(),
            vec![
                "Port 2222",
                "User dev",
                "IdentityFile \"~/.ssh/id build\"",
                "IdentitiesOnly no",
                "IdentityAgent \"~/.ssh/agent.sock\"",
                "StrictHostKeyChecking ask",
                "ProxyJump \"bastion\"",
                "ForwardAgent no",
                "ControlPersist 600",
                "RemoteCommand \"tmux new -A\"",
                "RequestTTY yes",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>()
        );
    }
}
