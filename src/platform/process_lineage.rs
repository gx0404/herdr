//! 进程父链快照：共用的类型与上溯逻辑。
//!
//! 平台层提供父链、进程表和本地 socket / 命名管道对端 PID 的读取。
//! 共用的上溯、成环与深度上限、后代判定及 tmux/screen 客户端启发式在这里实现，
//! 平台无关、可直接单测。

use std::collections::HashSet;

/// 上溯父链的深度上限：超过就按「查不清」处理，不再往上走。
pub(crate) const PROCESS_LINEAGE_DEPTH_LIMIT: usize = 64;

/// 进程表里的一项：pid、父 pid 与可执行文件名（Linux `/proc/<pid>/stat` 的 `comm`、
/// macOS `pbi_comm`、Windows `szExeFile`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessParentEntry {
    pub(crate) pid: u32,
    pub(crate) parent_pid: u32,
    pub(crate) name: String,
}

/// 从某个进程沿父链上溯得到的快照，自身在首位。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessLineage {
    pub(crate) processes: Vec<ProcessParentEntry>,
    /// 走到了没有父进程的根（父 pid 为 0：Linux / macOS 的 pid 1、Windows 的 System）
    /// 才为 true；中途读不到父进程、成环或超过深度上限都是 false。
    pub(crate) complete: bool,
}

impl ProcessLineage {
    pub(crate) fn contains(&self, pid: u32) -> bool {
        self.processes.iter().any(|process| process.pid == pid)
    }

    /// 链上有 `ancestor`（自身也算）→ `Some(true)`；链完整走到根仍不见 → `Some(false)`；
    /// 链不完整（读不到、成环、超深度）→ `None`，调用方按「查不清」处理。
    pub(crate) fn descends_from(&self, ancestor: u32) -> Option<bool> {
        if ancestor != 0 && self.contains(ancestor) {
            return Some(true);
        }
        self.complete.then_some(false)
    }
}

/// 共用的上溯：`lookup` 读一个进程的父 pid 与名字，读不到返回 `None`。
///
/// 起点读不到时返回 `None`（进程已退出或无权读取）；之后任一环读不到、成环或超过
/// [`PROCESS_LINEAGE_DEPTH_LIMIT`] 时返回已走到的部分，`complete` 为 false。
pub(crate) fn walk_process_lineage(
    pid: u32,
    mut lookup: impl FnMut(u32) -> Option<ProcessParentEntry>,
) -> Option<ProcessLineage> {
    if pid == 0 {
        return None;
    }
    let mut entry = lookup(pid)?;
    let mut lineage = ProcessLineage {
        processes: Vec::new(),
        complete: false,
    };
    let mut visited = HashSet::new();
    loop {
        visited.insert(entry.pid);
        let parent_pid = entry.parent_pid;
        lineage.processes.push(entry);
        if parent_pid == 0 {
            lineage.complete = true;
            return Some(lineage);
        }
        if lineage.processes.len() >= PROCESS_LINEAGE_DEPTH_LIMIT || visited.contains(&parent_pid) {
            return Some(lineage);
        }
        match lookup(parent_pid) {
            Some(parent) => entry = parent,
            None => return Some(lineage),
        }
    }
}

/// Names are exact matches: a custom `tmux-wrapper` is not a multiplexer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Multiplexer {
    Tmux,
    Screen,
}

fn multiplexer_server(name: &str) -> Option<Multiplexer> {
    match name {
        "tmux" | "tmux: server" => Some(Multiplexer::Tmux),
        "screen" | "SCREEN" => Some(Multiplexer::Screen),
        _ => None,
    }
}

fn multiplexer_client(name: &str) -> Option<Multiplexer> {
    match name {
        "tmux" | "tmux: client" => Some(Multiplexer::Tmux),
        "screen" => Some(Multiplexer::Screen),
        _ => None,
    }
}

/// Snapshot client ancestors only when the peer has a tmux/screen server ancestor.
/// The pane PID is resolved later on the event loop, which performs no process I/O.
/// This intentionally only proves a same-kind client, not the exact server/session.
pub(crate) fn multiplexer_client_lineages(peer: &ProcessLineage) -> Option<Vec<ProcessLineage>> {
    multiplexer_client_lineages_with(peer, super::process_parent_entries)
}

fn multiplexer_client_lineages_with(
    peer: &ProcessLineage,
    snapshot: impl FnOnce() -> Option<Vec<ProcessParentEntry>>,
) -> Option<Vec<ProcessLineage>> {
    let servers: Vec<_> = peer
        .processes
        .iter()
        .skip(1)
        .filter_map(|process| multiplexer_server(&process.name))
        .collect();
    if servers.is_empty() {
        return Some(Vec::new());
    }
    let entries = snapshot()?;
    let by_pid: std::collections::HashMap<_, _> =
        entries.iter().map(|entry| (entry.pid, entry)).collect();
    Some(
        entries
            .iter()
            .filter(|entry| {
                // Do not mistake the peer's own server for an attached pane client.
                !peer.contains(entry.pid)
                    && multiplexer_client(&entry.name).is_some_and(|kind| servers.contains(&kind))
            })
            .filter_map(|entry| {
                walk_process_lineage(entry.pid, |pid| {
                    by_pid.get(&pid).map(|entry| (*entry).clone())
                })
            })
            .collect(),
    )
}

/// `pid` 是否在 `ancestor_pid` 的进程树里（`pid == ancestor_pid` 也算）。
///
/// 生产路径不直接调它：API 连接线程先把对端父链快照成 [`ProcessLineage`]，事件循环再
/// 拿窗格根进程用 [`ProcessLineage::descends_from`] 判定，进程查询不进事件循环。这个
/// 包装给平台层的真进程测试（Linux / macOS）用。
///
/// `Some(true)`：父链上有它；`Some(false)`：父链完整走到根也没有；`None`：查不清（任一
/// pid 为 0、起点读不到、链中途断开或超过深度上限）。Windows 不会把孤儿进程重新挂到
/// 别的父进程下，父进程退出后链就断了，这类情形同样是 `None`。
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
pub(crate) fn is_descendant_of(pid: u32, ancestor_pid: u32) -> Option<bool> {
    if pid == 0 || ancestor_pid == 0 {
        return None;
    }
    if pid == ancestor_pid {
        return Some(true);
    }
    super::process_lineage(pid)?.descends_from(ancestor_pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pid: u32, parent_pid: u32, name: &str) -> ProcessParentEntry {
        ProcessParentEntry {
            pid,
            parent_pid,
            name: name.into(),
        }
    }

    fn table_lookup(
        table: &[ProcessParentEntry],
    ) -> impl FnMut(u32) -> Option<ProcessParentEntry> + '_ {
        move |pid| table.iter().find(|entry| entry.pid == pid).cloned()
    }

    #[test]
    fn lineage_walks_to_the_root_and_is_complete() {
        let table = [
            entry(1, 0, "systemd"),
            entry(10, 1, "herdr"),
            entry(20, 10, "zsh"),
            entry(30, 20, "claude"),
            entry(40, 30, "python3"),
        ];
        let lineage = walk_process_lineage(40, table_lookup(&table)).expect("起点可读");
        assert!(lineage.complete);
        assert_eq!(
            lineage
                .processes
                .iter()
                .map(|process| process.pid)
                .collect::<Vec<_>>(),
            vec![40, 30, 20, 10, 1]
        );
        assert_eq!(lineage.descends_from(20), Some(true), "窗格 shell 在链上");
        assert_eq!(lineage.descends_from(40), Some(true), "自身也算");
        assert_eq!(lineage.descends_from(99), Some(false), "完整链上没有就是否");
        assert_eq!(lineage.descends_from(0), Some(false), "pid 0 不当祖先");
    }

    #[test]
    fn detached_process_is_not_a_descendant_of_the_pane_shell() {
        // 取证里的后台会话：bg-pty-host → systemd --user → 1，不经过任何窗格 shell。
        let table = [
            entry(1, 0, "systemd"),
            entry(900, 1, "systemd"),
            entry(910, 900, "bg-pty-host"),
            entry(920, 910, "claude"),
            entry(930, 920, "python3"),
            entry(20, 1, "zsh"),
        ];
        let lineage = walk_process_lineage(930, table_lookup(&table)).expect("起点可读");
        assert_eq!(lineage.descends_from(20), Some(false));
    }

    #[test]
    fn unreadable_start_is_none_and_broken_chain_is_unknown() {
        let table = [entry(30, 20, "claude"), entry(40, 30, "python3")];
        assert_eq!(walk_process_lineage(99, table_lookup(&table)), None);
        assert_eq!(walk_process_lineage(0, table_lookup(&table)), None);
        let lineage = walk_process_lineage(40, table_lookup(&table)).expect("起点可读");
        assert!(!lineage.complete, "父进程 20 读不到，链不完整");
        assert_eq!(
            lineage.descends_from(30),
            Some(true),
            "已走到的部分仍可证明是后代"
        );
        assert_eq!(lineage.descends_from(10), None, "断链上找不到只能是查不清");
    }

    #[test]
    fn ordinary_reports_do_not_enumerate_the_process_table() {
        let peer =
            walk_process_lineage(2, table_lookup(&[entry(2, 1, "hook"), entry(1, 0, "init")]))
                .unwrap();
        assert_eq!(
            multiplexer_client_lineages_with(&peer, || panic!("no table scan")),
            Some(Vec::new())
        );
    }

    #[test]
    fn multiplexer_clients_match_exact_kind_and_exclude_server_ancestors() {
        for (server, client, other) in [
            ("tmux: server", "tmux: client", "screen"),
            ("SCREEN", "screen", "tmux"),
        ] {
            let table = [
                entry(1, 0, "init"),
                entry(10, 1, "pane-shell"),
                entry(20, 10, client),
                entry(30, 1, server),
                entry(40, 30, "hook"),
                entry(50, 10, other),
                entry(60, 10, "tmux-wrapper"),
            ];
            let peer = walk_process_lineage(40, table_lookup(&table)).unwrap();
            let clients = multiplexer_client_lineages_with(&peer, || Some(table.to_vec())).unwrap();
            assert_eq!(clients.len(), 1);
            assert_eq!(clients[0].processes[0].pid, 20);
            assert_eq!(clients[0].descends_from(10), Some(true));
            assert_eq!(clients[0].descends_from(99), Some(false));
            assert_eq!(multiplexer_client_lineages_with(&peer, || None), None);
        }
        // On macOS a server may be named plain tmux; it must not count as a client.
        let table = [
            entry(1, 0, "init"),
            entry(30, 1, "tmux"),
            entry(40, 30, "hook"),
        ];
        let peer = walk_process_lineage(40, table_lookup(&table)).unwrap();
        assert_eq!(
            multiplexer_client_lineages_with(&peer, || Some(table.to_vec())),
            Some(Vec::new())
        );
    }

    #[test]
    fn cycles_and_depth_limit_stop_as_incomplete() {
        let cycle = [entry(5, 6, "a"), entry(6, 5, "b")];
        let lineage = walk_process_lineage(5, table_lookup(&cycle)).expect("起点可读");
        assert!(!lineage.complete);
        assert_eq!(lineage.processes.len(), 2);

        let deep: Vec<ProcessParentEntry> = (1..=200u32)
            .map(|pid| entry(pid, if pid == 1 { 0 } else { pid - 1 }, "sh"))
            .collect();
        let lineage = walk_process_lineage(200, table_lookup(&deep)).expect("起点可读");
        assert!(!lineage.complete, "超过深度上限按查不清处理");
        assert_eq!(lineage.processes.len(), PROCESS_LINEAGE_DEPTH_LIMIT);
        assert_eq!(lineage.descends_from(1), None);
    }
}

/// 真进程验证：Linux 读 `/proc`、macOS 走 `proc_pidinfo` / `LOCAL_PEERPID`。
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod live_tests {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use super::is_descendant_of;

    /// 读子进程打印的一行 pid。
    fn read_pid_line(child: &mut std::process::Child) -> u32 {
        let stdout = child.stdout.take().expect("子进程 stdout 已接管道");
        let mut line = String::new();
        BufReader::new(stdout)
            .read_line(&mut line)
            .expect("读到孙进程 pid");
        line.trim().parse().expect("孙进程 pid 是数字")
    }

    fn kill(pid: u32) {
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
    }

    #[test]
    fn own_lineage_is_complete_and_starts_with_self() {
        let own = std::process::id();
        let lineage = crate::platform::process_lineage(own).expect("读得到自身");
        assert_eq!(
            lineage.processes.first().map(|process| process.pid),
            Some(own)
        );
        assert!(lineage.complete, "自身的父链应能走到根: {lineage:?}");
        assert!(lineage.contains(std::os::unix::process::parent_id()));
        assert_eq!(crate::platform::process_lineage(0), None);
    }

    #[test]
    fn spawned_grandchild_descends_from_its_ancestors_only() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "sleep 30 & echo $!; wait"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("拉起 sh");
        let grandchild = read_pid_line(&mut child);
        let own = std::process::id();
        let results = (
            is_descendant_of(grandchild, child.id()),
            is_descendant_of(grandchild, own),
            is_descendant_of(own, child.id()),
            is_descendant_of(grandchild, grandchild),
        );
        kill(grandchild);
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(results.0, Some(true), "孙进程在 sh 的树里");
        assert_eq!(results.1, Some(true), "孙进程在测试进程的树里");
        assert_eq!(results.2, Some(false), "测试进程不在 sh 的树里");
        assert_eq!(results.3, Some(true), "自身算在树里");
    }

    #[test]
    fn orphaned_process_leaves_the_tree_it_was_started_from() {
        // The orphan reads a pipe held only by this test. Closing it (including SIGKILL
        // of the test process) makes the orphan exit; cleanup does not rely on Drop.
        // No detached server, filesystem sandbox or persistent process is created.
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exec 3<&0; cat <&3 3<&- >/dev/null & echo $!"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("start disposable pipe reader");
        let orphan = read_pid_line(&mut child);
        let root = child.id();
        let owner_pipe = child.stdin.take().expect("test owns pipe writer");
        let _ = child.wait();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut verdict = is_descendant_of(orphan, root);
        while verdict != Some(false) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            verdict = is_descendant_of(orphan, root);
        }
        drop(owner_pipe);
        // Also remove the exact known process immediately on ordinary completion.
        kill(orphan);
        assert_eq!(
            verdict,
            Some(false),
            "orphan no longer descends from its departed parent"
        );
    }

    #[test]
    fn peer_process_id_reports_the_connecting_process() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("hpeer-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let path = dir.join("peer.sock");
        let listener = crate::ipc::bind_local_listener(&path).expect("绑定本地 socket");
        let client = crate::ipc::connect_local_stream(&path).expect("连接本地 socket");
        let accepted = {
            use interprocess::local_socket::traits::ListenerExt as _;
            listener
                .incoming()
                .next()
                .expect("有一个连接")
                .expect("accept 成功")
        };
        let peer = crate::platform::peer_process_id(&accepted);
        drop(client);
        drop(accepted);
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(peer, Some(std::process::id()));
    }
}
