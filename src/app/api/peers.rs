use crate::api::schema::{PeerSystemSummary, PeerWorkspaceSummary, ResponseResult};
use crate::app::App;

use super::responses::encode_success;

impl App {
    /// Serve this server's federated summary: one entry per workspace with
    /// project identity + attention-leading agent status. Peers poll this
    /// over SSH to fold our workspaces into their sidebars.
    pub(super) fn handle_peers_summary(&mut self, id: String) -> String {
        let workspaces = self
            .state
            .workspaces
            .iter()
            .map(|ws| workspace_peer_summary(ws, &self.state.terminals))
            .collect();
        encode_success(
            id,
            ResponseResult::PeersSummary {
                host: short_host_name(),
                version: Some(crate::build_info::version()),
                system: self.state.system_stats.as_ref().map(system_summary),
                workspaces,
            },
        )
    }
}

/// Map the local status-line stats sampler onto the federated summary shape.
fn system_summary(stats: &crate::system_stats::SystemStats) -> PeerSystemSummary {
    PeerSystemSummary {
        cpu_percent: stats
            .cpu_percent
            .map(|cpu| cpu.round().clamp(0.0, 100.0) as u8),
        mem_used: stats.mem_used,
        mem_total: stats.mem_total,
        disk_free: stats.disk_free,
    }
}

/// A resolved server switch ready to send to the foreground client:
/// the next attach target plus the fleet snapshot that leg carries.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PreparedServerSwitch {
    pub(crate) ssh_target: String,
    pub(crate) label: String,
    pub(crate) fleet: Option<crate::protocol::FleetSnapshot>,
}

impl App {
    /// Resolve a server-switch request from the sidebar or the switch_home
    /// keybind into the SwitchServer payload. Returns None when the request
    /// no longer resolves (rows changed) — or for Home without an origin.
    pub(crate) fn prepare_switch_server(
        &mut self,
        request: crate::app::state::PeerSwitchRequest,
    ) -> Option<PreparedServerSwitch> {
        use crate::app::state::PeerSwitchRequest;
        match request {
            PeerSwitchRequest::ConfigPeer { peer_idx, ws_idx } => {
                let (ssh_target, label) = self.prepare_peer_switch(peer_idx, ws_idx)?;
                let fleet = Some(self.outgoing_fleet_snapshot(&ssh_target));
                Some(PreparedServerSwitch {
                    ssh_target,
                    label,
                    fleet,
                })
            }
            PeerSwitchRequest::SnapshotPeer { entry_idx } => {
                let entry = self.state.fleet_snapshot.as_ref()?.peers.get(entry_idx)?;
                let ssh_target = entry.ssh_target.clone();
                let label = entry.host.clone().unwrap_or_else(|| entry.peer.clone());
                let fleet = Some(self.outgoing_fleet_snapshot(&ssh_target));
                Some(PreparedServerSwitch {
                    ssh_target,
                    label,
                    fleet,
                })
            }
            PeerSwitchRequest::Home => {
                let origin = self.state.fleet_snapshot.as_ref()?.origin.clone();
                Some(PreparedServerSwitch {
                    ssh_target: crate::protocol::HOME_SWITCH_TARGET.to_string(),
                    label: format!("{origin} (home)"),
                    fleet: None,
                })
            }
        }
    }

    /// The fleet snapshot the next attach leg carries. Pass-through, never
    /// re-stamp: a server that itself received a snapshot forwards it with
    /// the ORIGINAL origin (nested leaps keep the real home); only a server
    /// the client reached directly (the hub) stamps a fresh snapshot from
    /// its own identity and polled peer summaries. The hop target is
    /// excluded — it becomes the self row on the receiving end.
    fn outgoing_fleet_snapshot(&self, exclude_ssh_target: &str) -> crate::protocol::FleetSnapshot {
        match self.state.fleet_snapshot.as_ref() {
            Some(snapshot) => snapshot.to_wire(exclude_ssh_target),
            None => crate::protocol::FleetSnapshot {
                origin: short_host_name(),
                peers: self
                    .state
                    .peer_summaries
                    .iter()
                    .filter(|peer| peer.ssh_target != exclude_ssh_target)
                    .map(crate::peers::peer_to_wire)
                    .collect(),
            },
        }
    }

    /// Resolve a requested peer switch: returns the SSH target for the
    /// client's next attach leg and a display label, and best-effort
    /// pre-focuses the chosen workspace on the peer (off-thread).
    pub(crate) fn prepare_peer_switch(
        &mut self,
        peer_idx: usize,
        ws_idx: usize,
    ) -> Option<(String, String)> {
        let peer = self.state.peer_summaries.get(peer_idx)?;
        let ssh_target = peer.ssh_target.clone();
        let label = peer.host.clone().unwrap_or_else(|| peer.peer.clone());
        if let Some(remote_ws) = peer.workspaces.get(ws_idx) {
            let label = format!("{label}:{}", remote_ws.workspace);
            // Workspace ids are server-assigned ("ws_3"); refuse anything
            // that could escape the remote shell command.
            let id = remote_ws.id.clone();
            if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                let target = ssh_target.clone();
                std::thread::spawn(move || {
                    let _ = std::process::Command::new("ssh")
                        .args([
                            "-o",
                            "BatchMode=yes",
                            "-o",
                            "ConnectTimeout=5",
                            &target,
                            &format!("sh -lc 'herdr workspace focus --workspace {id}'"),
                        ])
                        .stdin(std::process::Stdio::null())
                        .output();
                });
            }
            return Some((ssh_target, label));
        }
        Some((ssh_target, label))
    }
}

/// Short, stable hostname for the status line and peer identity. Cached for the
/// session. On macOS this prefers the user-set `LocalHostName` over the network
/// hostname, which on corp/campus DHCP (e.g. ETH `staff-net-*.intern.ethz.ch`)
/// is an unstable name nobody recognizes.
pub(crate) fn short_host_name() -> String {
    use std::sync::OnceLock;
    static CACHED: OnceLock<String> = OnceLock::new();
    CACHED.get_or_init(compute_short_host_name).clone()
}

fn compute_short_host_name() -> String {
    #[cfg(target_os = "macos")]
    if let Some(name) = macos_local_host_name() {
        return name;
    }
    sysinfo::System::host_name()
        .map(|h| h.split('.').next().unwrap_or(&h).to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(target_os = "macos")]
fn macos_local_host_name() -> Option<String> {
    let out = std::process::Command::new("/usr/sbin/scutil")
        .args(["--get", "LocalHostName"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!name.is_empty()).then_some(name)
}

fn workspace_peer_summary(
    ws: &crate::workspace::Workspace,
    terminals: &std::collections::HashMap<
        crate::terminal::TerminalId,
        crate::terminal::TerminalState,
    >,
) -> PeerWorkspaceSummary {
    let (state, seen) = ws.aggregate_state(terminals);
    // The attention-leading pane: highest priority, oldest transition first —
    // mirrors the local focus_attention ordering. Panes without a transition
    // timestamp sort as newest.
    let now = std::time::Instant::now();
    let leading = ws
        .pane_details(terminals)
        .into_iter()
        .filter(|detail| (detail.state, detail.seen) == (state, seen))
        .min_by_key(|detail| detail.state_changed_at.unwrap_or(now));
    let (agent, status_age_secs, activity) = leading
        .map(|detail| {
            (
                Some(crate::detect::short_agent_label(&detail.agent_label).to_string()),
                detail
                    .state_changed_at
                    .map(|changed| changed.elapsed().as_secs()),
                detail.live_activity,
            )
        })
        .unwrap_or((None, None, None));

    // The git-space cache is populated by the periodic async refresh, so a
    // freshly-created workspace may not have it yet. Derive the project
    // identity live from the checkout in that cold-start window so the peer
    // row can still fold by project.
    let derived_space = ws
        .git_space()
        .is_none()
        .then(|| ws.resolved_identity_cwd())
        .flatten()
        .and_then(|cwd| crate::workspace::git_space_metadata(&cwd));
    let project_key = ws.project_key().map(str::to_string).or_else(|| {
        derived_space
            .as_ref()
            .map(|space| space.project_key.clone())
    });
    let project_label = ws
        .git_space()
        .map(|space| space.label.clone())
        .or_else(|| derived_space.as_ref().map(|space| space.label.clone()))
        .or_else(|| ws.worktree_space().map(|space| space.label.clone()));

    PeerWorkspaceSummary {
        id: ws.id.clone(),
        workspace: ws.display_name(),
        project_key,
        project_label,
        branch: ws.branch(),
        is_linked_worktree: ws
            .git_space()
            .map(|space| space.is_linked_worktree)
            .or_else(|| ws.worktree_space().map(|space| space.is_linked_worktree))
            .unwrap_or(false),
        agent,
        status: super::super::api_helpers::pane_agent_status(state, seen),
        status_age_secs,
        activity,
    }
}

#[cfg(test)]
mod tests {
    use crate::app::state::PeerSwitchRequest;
    use crate::app::App;

    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    fn summary(name: &str, ssh_target: &str) -> crate::peers::PeerSummaryState {
        crate::peers::PeerSummaryState {
            peer: name.to_string(),
            ssh_target: ssh_target.to_string(),
            host: Some(name.to_string()),
            version: None,
            system: None,
            latency_ms: Some(10),
            // Deliberately empty: prepare_peer_switch must not spawn the
            // remote pre-focus ssh in tests.
            workspaces: Vec::new(),
            last_ok: Some(std::time::Instant::now()),
            error: None,
        }
    }

    fn carried_snapshot() -> crate::peers::FleetSnapshotState {
        crate::peers::FleetSnapshotState {
            origin: "mba22".to_string(),
            peers: vec![summary("anvil", "lars@anvil"), summary("ksb", "lars@ksb")],
            received_at: std::time::Instant::now(),
        }
    }

    #[tokio::test]
    async fn home_request_resolves_to_reserved_target_without_fleet() {
        let mut app = test_app();
        app.state.fleet_snapshot = Some(carried_snapshot());

        let prepared = app
            .prepare_switch_server(PeerSwitchRequest::Home)
            .expect("home resolves when an origin was carried");
        assert_eq!(prepared.ssh_target, crate::protocol::HOME_SWITCH_TARGET);
        assert!(prepared.label.contains("mba22"));
        // Going home carries nothing: the local server needs no snapshot.
        assert!(prepared.fleet.is_none());
    }

    #[tokio::test]
    async fn home_request_without_origin_resolves_to_none() {
        let mut app = test_app();
        assert!(app.prepare_switch_server(PeerSwitchRequest::Home).is_none());
    }

    #[tokio::test]
    async fn snapshot_row_switch_passes_snapshot_through_with_original_origin() {
        let mut app = test_app();
        app.state.fleet_snapshot = Some(carried_snapshot());

        let prepared = app
            .prepare_switch_server(PeerSwitchRequest::SnapshotPeer { entry_idx: 0 })
            .expect("snapshot row resolves");
        assert_eq!(prepared.ssh_target, "lars@anvil");
        let fleet = prepared.fleet.expect("nested leap carries the snapshot");
        // Pass-through, not re-stamp: the ORIGINAL origin survives, and the
        // hop target drops out (it becomes the self row over there).
        assert_eq!(fleet.origin, "mba22");
        assert_eq!(fleet.peers.len(), 1);
        assert_eq!(fleet.peers[0].ssh_target, "lars@ksb");
    }

    #[tokio::test]
    async fn config_peer_switch_from_hub_stamps_own_origin_and_peers() {
        let mut app = test_app();
        app.state.peer_summaries =
            vec![summary("anvil", "lars@anvil"), summary("sage", "lars@sage")];

        let prepared = app
            .prepare_switch_server(PeerSwitchRequest::ConfigPeer {
                peer_idx: 1,
                ws_idx: 0,
            })
            .expect("config peer resolves");
        assert_eq!(prepared.ssh_target, "lars@sage");
        let fleet = prepared.fleet.expect("hub leap stamps a fresh snapshot");
        assert_eq!(fleet.origin, crate::app::short_host_name());
        // The hop target is excluded from its own snapshot.
        assert_eq!(fleet.peers.len(), 1);
        assert_eq!(fleet.peers[0].ssh_target, "lars@anvil");
    }

    #[tokio::test]
    async fn stale_snapshot_row_index_resolves_to_none() {
        let mut app = test_app();
        app.state.fleet_snapshot = Some(carried_snapshot());
        assert!(app
            .prepare_switch_server(PeerSwitchRequest::SnapshotPeer { entry_idx: 99 })
            .is_none());
    }
}
