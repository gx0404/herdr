//! 机器相关页面的手动视觉画廊：机器仪表盘、窄屏列表、空列表、详情、添加 / 编辑表单、
//! 导入向导、端口转发、连接恢复对话框、远程文件与删除确认，深浅两套主题、窄宽两档
//! 尺寸。`just ui-gallery` 输出到 `target/ui-gallery/machines*.html`。

use super::*;
use crate::client::endpoint::{
    ClientEndpointStatus, PortForwardKind, PortForwardRule, ProfileId, SavedSshEndpoint,
};
use crate::remote::{ConnectionErrorKind, RemoteDirEntry, RemoteEntryKind};

fn machine(label: &str, target: &str, seed: &str) -> SavedSshEndpoint {
    let hex = format!("{seed:0>32}");
    SavedSshEndpoint {
        id: ProfileId::parse(hex).expect("hex profile id"),
        label: label.into(),
        target: target.into(),
        ..SavedSshEndpoint::new(label, target, "default").expect("valid profile")
    }
}

/// 一组状态各异的机器：在线（带版本）、需要处理（认证失败）、连接中、停用。
fn fleet() -> Vec<SavedSshEndpoint> {
    let mut build = machine("build-server", "deploy@build.internal", "a1");
    build.group = Some("prod".into());
    build.port_forwards = vec![
        PortForwardRule {
            kind: PortForwardKind::Local,
            bind_address: None,
            listen_port: 8080,
            target_host: Some("127.0.0.1".into()),
            target_port: Some(80),
        },
        PortForwardRule {
            kind: PortForwardKind::Local,
            bind_address: Some("0.0.0.0".into()),
            listen_port: 5432,
            target_host: Some("db.internal".into()),
            target_port: Some(5432),
        },
        PortForwardRule {
            kind: PortForwardKind::Dynamic,
            bind_address: None,
            listen_port: 1080,
            target_host: None,
            target_port: None,
        },
    ];
    let mut staging = machine("staging", "ops@staging.example.com", "b2");
    staging.group = Some("prod".into());
    let gpu = machine("gpu-box", "me@10.0.0.42", "c3");
    let mut laptop = machine("old-laptop", "me@laptop.local", "d4");
    laptop.enabled = false;
    let edge = machine("edge-01", "root@edge-01.cdn.example", "e5");
    vec![build, staging, gpu, laptop, edge]
}

fn endpoint(profile: &SavedSshEndpoint) -> ClientEndpointId {
    ClientEndpointId::Ssh(profile.id.clone())
}

/// 写入机器目录与各自的连接状态（不落盘）。
fn seed_fleet(state: &mut ClientShellState) -> Vec<SavedSshEndpoint> {
    let fleet = fleet();
    state.set_endpoint_catalog(&fleet);
    state.set_endpoint_status(&endpoint(&fleet[0]), ClientEndpointStatus::Online);
    state.set_endpoint_server_version(&endpoint(&fleet[0]), Some("0.9.5".into()));
    state.set_endpoint_port_forward_status(
        &endpoint(&fleet[0]),
        vec![
            crate::remote::PortForwardStatus {
                rule: fleet[0].port_forwards[0].clone(),
                phase: crate::remote::PortForwardPhase::Active,
                detail: None,
            },
            crate::remote::PortForwardStatus {
                rule: fleet[0].port_forwards[1].clone(),
                phase: crate::remote::PortForwardPhase::Failed,
                detail: Some("address already in use".into()),
            },
        ],
    );
    state.set_endpoint_status(&endpoint(&fleet[1]), ClientEndpointStatus::Attention);
    state.set_endpoint_status_detail(
        &endpoint(&fleet[1]),
        Some("Permission denied (publickey,password)".into()),
    );
    state.set_endpoint_connection_error_kind(
        &endpoint(&fleet[1]),
        Some(ConnectionErrorKind::AuthRequired {
            methods: vec!["publickey".into(), "password".into()],
            identity_file: Some("~/.ssh/id_ed25519".into()),
        }),
    );
    state.set_endpoint_status(&endpoint(&fleet[4]), ClientEndpointStatus::Online);
    state.set_endpoint_server_version(&endpoint(&fleet[4]), Some("0.9.4".into()));
    fleet
}

fn press(state: &mut ClientShellState, code: KeyCode, modifiers: KeyModifiers) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    let key = crate::input::TerminalKey::new(code, modifiers);
    if !state.route_machines_key(&key, &mut outcome) {
        state.route_machine_files_key(&key, &mut outcome);
    }
    outcome
}

fn plain(state: &mut ClientShellState, code: KeyCode) -> ClientShellInput {
    press(state, code, KeyModifiers::NONE)
}

fn machine_form(
    state: &mut ClientShellState,
) -> &mut super::super::machines_overlay::ClientMachineForm {
    match state.overlay.as_mut() {
        Some(ClientShellOverlay::Machines(overlay)) => match &mut overlay.view {
            super::super::machines_overlay::ClientMachinesView::Form(form) => form,
            _ => panic!("机器表单"),
        },
        _ => panic!("机器页"),
    }
}

const SSH_CONFIG: &str = "\
Host bastion
    HostName bastion.internal
    User ops

Host web
    HostName web.internal
    User deploy
    Port 2201
    ProxyJump bastion
    StrictHostKeyChecking no

Host db-primary
    HostName 10.0.0.12
    IdentityFile ~/.ssh/db_ed25519

Host *.wild
    User nobody
";

/// 导入向导：假主目录里放一份 SSH 配置后打开向导。`dirs` 在场景里只活到这一步，
/// 发现结果已读进内存。
fn open_import(state: &mut ClientShellState) {
    let dirs = isolated_state_home("gallery-machines-import");
    let home = dirs.state_dir().with_file_name("ssh-home");
    std::fs::create_dir_all(home.join(".ssh")).expect("temp home");
    std::fs::write(home.join(".ssh").join("config"), SSH_CONFIG).expect("ssh config");
    crate::config::test_dirs::set_home_dir(home);
    state.open_machine_import_wizard();
}

fn files_entries() -> Vec<RemoteDirEntry> {
    let entry = |name: &str, kind, size, modified: &str| RemoteDirEntry {
        name: name.into(),
        kind,
        size,
        mode: Some(0o644),
        link_target: None,
        modified: modified.into(),
    };
    vec![
        entry("src", RemoteEntryKind::Directory, 4096, "Oct 21 09:12"),
        entry("target", RemoteEntryKind::Directory, 4096, "Oct 21 10:40"),
        entry("Cargo.toml", RemoteEntryKind::File, 2_314, "Oct 20 18:03"),
        entry("README.md", RemoteEntryKind::File, 11_902, "Oct 19 11:27"),
        entry("deploy.sh", RemoteEntryKind::File, 845, "Sep 30 08:55"),
        entry("current", RemoteEntryKind::Symlink, 24, "Sep 28 16:01"),
    ]
}

/// 打开第一台机器的远程文件，并用 `entries` 应答首个列目录请求。
fn open_files(state: &mut ClientShellState, entries: Vec<RemoteDirEntry>) {
    let fleet = seed_fleet(state);
    let mut outcome = ClientShellInput::default();
    state.open_machine_files(&fleet[0].id, &mut outcome);
    let ticket = outcome
        .actions
        .iter()
        .find_map(|action| match action {
            ClientShellAction::MachineFsOp { ticket, .. } => Some(*ticket),
            _ => None,
        })
        .expect("列目录请求");
    state.handle_machine_fs_result(
        ticket,
        Ok(super::super::state::MachineFsOutcome::Entries { entries }),
        &mut ClientShellInput::default(),
    );
}

/// 一个画廊场景：名称、准备动作与要截的尺寸。
type Scene = (&'static str, Prepare, &'static [(u16, u16)]);

#[test]
#[ignore = "manual visual gallery; run `just ui-gallery`"]
fn ui_gallery_machines() {
    let _lang = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    let dark = || ClientShellConfig::from_config(&Config::default());
    let light = || {
        let mut config = Config::default();
        config.theme.name = Some("graphite-light".into());
        ClientShellConfig::from_config(&config)
    };

    let dashboard: Prepare = |state| {
        seed_fleet(state);
        state.open_machines_overlay();
    };
    let list_hover: Prepare = |state| {
        seed_fleet(state);
        state.open_machines_overlay();
        plain(state, KeyCode::Down);
        let _ = state.compose(80, 24);
        if let Some((rect, _)) = state.hits.machines_rows.get(3).cloned() {
            state.handle_mouse(
                crossterm::event::MouseEvent {
                    kind: crossterm::event::MouseEventKind::Moved,
                    column: rect.x + 4,
                    row: rect.y,
                    modifiers: KeyModifiers::NONE,
                },
                &mut ClientShellInput::default(),
            );
        }
    };
    let empty: Prepare = |state| state.open_machines_overlay();
    let no_match: Prepare = |state| {
        seed_fleet(state);
        state.open_machines_overlay();
        plain(state, KeyCode::Char('/'));
        state.insert_overlay_text("zzz");
    };
    let detail: Prepare = |state| {
        let fleet = seed_fleet(state);
        state.open_machines_overlay_for(&fleet[1].id);
    };
    let add_form: Prepare = |state| {
        state.open_machine_add_form();
        let form = machine_form(state);
        form.target = TextEditor::new("deploy@new-host.example:2222", false);
        form.label = TextEditor::new("new-host", false);
        form.focused = 1;
        press(state, KeyCode::Char('t'), KeyModifiers::CONTROL);
        let outcome = plain(state, KeyCode::Enter);
        if let Some(ticket) = outcome.actions.iter().find_map(|action| match action {
            ClientShellAction::BootstrapMachine { ticket, .. } => Some(*ticket),
            _ => None,
        }) {
            state.handle_machine_bootstrap_update(
                ticket,
                MachineBootstrapUpdate::Step(crate::remote::SavedSshBootstrapStep::StartServer),
            );
        }
    };
    let edit_form: Prepare = |state| {
        let fleet = seed_fleet(state);
        state.open_machine_edit_form(&fleet[0].id);
    };
    let import_discover: Prepare = open_import;
    let import_select: Prepare = |state| {
        open_import(state);
        plain(state, KeyCode::Enter);
        plain(state, KeyCode::Down);
    };
    let import_done: Prepare = |state| {
        let dirs = isolated_state_home("gallery-machines-import-done");
        open_import(state);
        plain(state, KeyCode::Enter);
        plain(state, KeyCode::Enter);
        drop(dirs);
    };
    let forwards: Prepare = |state| {
        let fleet = seed_fleet(state);
        state.open_machines_overlay_for(&fleet[0].id);
        state.open_machine_forwards(&fleet[0].id);
        plain(state, KeyCode::Down);
    };
    let forwards_remove: Prepare = |state| {
        let fleet = seed_fleet(state);
        state.open_machines_overlay_for(&fleet[0].id);
        state.open_machine_forwards(&fleet[0].id);
        plain(state, KeyCode::Down);
        plain(state, KeyCode::Char('x'));
    };
    let forwards_add: Prepare = |state| {
        let fleet = seed_fleet(state);
        state.open_machines_overlay_for(&fleet[0].id);
        state.open_machine_forwards(&fleet[0].id);
        plain(state, KeyCode::Char('a'));
    };
    let auth_guide: Prepare = |state| {
        let fleet = seed_fleet(state);
        state.open_machine_auth_for_endpoint(&fleet[1].id, &mut ClientShellInput::default());
    };
    let host_key_unknown: Prepare = |state| {
        let fleet = seed_fleet(state);
        let fingerprint = crate::remote::HostKeyFingerprint {
            key_type: "ssh-ed25519".into(),
            fingerprint: "SHA256:Qm9yZWFsaXMgaG9zdCBrZXkgZm9yIGdhbGxlcnk".into(),
        };
        state.set_endpoint_connection_error_kind(
            &endpoint(&fleet[2]),
            Some(ConnectionErrorKind::HostKeyUnknown {
                fingerprint: Some(fingerprint.clone()),
            }),
        );
        let mut outcome = ClientShellInput::default();
        state.open_machine_auth_for_endpoint(&fleet[2].id, &mut outcome);
        let ticket = outcome.actions.iter().find_map(|action| match action {
            ClientShellAction::MachineHostKeyOp { ticket, .. } => Some(*ticket),
            _ => None,
        });
        if let Some(ticket) = ticket {
            state.handle_machine_auth_update(
                MachineAuthUpdate::HostKeyOpFinished {
                    ticket,
                    op: MachineHostKeyOp::Scan,
                    result: Ok(MachineHostKeyOutcome::Scanned(
                        crate::remote::HostKeyReview {
                            target: crate::remote::EffectiveHostKeyTarget {
                                host: "10.0.0.42".into(),
                                port: 22,
                                lookup: "10.0.0.42".into(),
                                files: vec![std::path::PathBuf::from("~/.ssh/known_hosts")],
                                proxied: false,
                            },
                            keys: vec![crate::remote::KnownHostKey {
                                key_type: "ssh-ed25519".into(),
                                key_line: "ssh-ed25519 AAAAC3Nza".into(),
                                fingerprint,
                            }],
                        },
                    )),
                },
                &mut ClientShellInput::default(),
            );
        }
    };
    let host_key_changed: Prepare = |state| {
        let fleet = seed_fleet(state);
        state.set_endpoint_connection_error_kind(
            &endpoint(&fleet[4]),
            Some(ConnectionErrorKind::HostKeyChanged),
        );
        state.open_machine_auth_for_endpoint(&fleet[4].id, &mut ClientShellInput::default());
    };
    let password: Prepare = |state| {
        let fleet = seed_fleet(state);
        state.set_endpoint_connection_error_kind(
            &endpoint(&fleet[1]),
            Some(ConnectionErrorKind::AuthRequired {
                methods: vec!["password".into()],
                identity_file: None,
            }),
        );
        state.open_machine_auth_for_endpoint(&fleet[1].id, &mut ClientShellInput::default());
        state.activate_machine_auth_button(
            super::super::machine_auth_overlay::MachineAuthButton::InteractiveAuth,
            &mut ClientShellInput::default(),
        );
        for ticket in 1..=8 {
            if state.show_machine_auth_prompt(ticket, "ops@staging.example.com's password:") {
                break;
            }
        }
        state.insert_machine_auth_text("hunter22");
    };
    let files: Prepare = |state| open_files(state, files_entries());
    let files_empty: Prepare = |state| open_files(state, Vec::new());
    let files_loading: Prepare = |state| {
        let fleet = seed_fleet(state);
        state.open_machine_files(&fleet[0].id, &mut ClientShellInput::default());
    };
    let files_error: Prepare = |state| {
        open_files(state, files_entries());
        let outcome = plain(state, KeyCode::Char('r'));
        let ticket = outcome
            .actions
            .iter()
            .find_map(|action| match action {
                ClientShellAction::MachineFsOp { ticket, .. } => Some(*ticket),
                _ => None,
            })
            .expect("refresh request");
        state.handle_machine_fs_result(
            ticket,
            Err("Permission denied".into()),
            &mut ClientShellInput::default(),
        );
    };
    let files_no_match: Prepare = |state| {
        open_files(state, files_entries());
        plain(state, KeyCode::Char('/'));
        state.insert_overlay_text("zzz");
    };
    let remove: Prepare = |state| {
        let fleet = seed_fleet(state);
        state.open_machine_remove_confirm(&fleet[1].id);
    };

    let scenes: Vec<Scene> = vec![
        ("dashboard", dashboard, &[(160, 45), (120, 40)]),
        ("list", list_hover, &[(80, 24)]),
        ("empty", empty, &[(80, 24), (120, 40)]),
        ("filter no match", no_match, &[(80, 24)]),
        ("detail", detail, &[(120, 40), (80, 24)]),
        ("add form testing", add_form, &[(160, 45), (80, 24)]),
        ("edit form", edit_form, &[(120, 40)]),
        ("import discover", import_discover, &[(120, 40)]),
        ("import select", import_select, &[(160, 45), (80, 24)]),
        ("import done", import_done, &[(120, 40)]),
        ("forwards", forwards, &[(120, 40)]),
        ("forwards remove", forwards_remove, &[(80, 24)]),
        ("forwards add", forwards_add, &[(120, 40)]),
        ("auth guide", auth_guide, &[(120, 40)]),
        ("host key unknown", host_key_unknown, &[(120, 40)]),
        ("host key changed", host_key_changed, &[(80, 24)]),
        ("password", password, &[(80, 24)]),
        ("files", files, &[(120, 40), (80, 24)]),
        ("files empty", files_empty, &[(80, 24)]),
        ("files loading", files_loading, &[(80, 24)]),
        ("files error", files_error, &[(80, 24)]),
        ("files no match", files_no_match, &[(120, 40)]),
        ("remove confirm", remove, &[(80, 24)]),
    ];
    let mut shots = Vec::new();
    let mut step = 0;
    for (name, prepare, sizes) in scenes {
        for &size in sizes {
            for (theme, host) in [("dark", DARK_HOST), ("light", LIGHT_HOST)] {
                let config = if theme == "light" { light() } else { dark() };
                let frame = shoot_scene(config, size, prepare, &mut step);
                shots.push(Shot {
                    title: format!("machines {name} {theme} {}x{}", size.0, size.1),
                    frame,
                    host,
                });
            }
        }
    }
    write_shots("machines", &shots);
}
