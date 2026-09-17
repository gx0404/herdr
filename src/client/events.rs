use super::*;

/// Internal events for the client event loop.
pub(super) enum ClientLoopEvent {
    #[cfg(unix)]
    StdinInput(Vec<u8>),
    #[cfg(unix)]
    PixelMouse(Vec<u8>, crate::input::mouse::HostGeometry),
    #[cfg(unix)]
    DirectGraphicsResponse(direct_graphics::Response),
    #[cfg(windows)]
    StdinEvents(Vec<crate::protocol::ClientInputEvent>),
    Resize(u16, u16, u32, u32, bool),
    TerminalUnavailable(io::Error),
    ServerMessage {
        endpoint_id: endpoint::ClientEndpointId,
        generation: u64,
        message: Box<ServerMessage>,
    },
    ServerDisconnected {
        endpoint_id: endpoint::ClientEndpointId,
        generation: u64,
    },
    EndpointSupervisor(endpoint::EndpointSupervisorEvent),
    EndpointCatalog(Result<Vec<endpoint::SavedSshEndpoint>, String>),
    /// The broadcast target-set file changed on disk (CLI edits). Carries no
    /// payload on purpose: the loop re-reads the file at handling time, so a
    /// stale notification can never clobber a fresher local write.
    BroadcastSetChanged,
    /// Manual reconnect request from the machines UI.
    ReconnectEndpoint {
        endpoint_id: endpoint::ClientEndpointId,
    },
    /// Reconnect with an in-memory `accept-new` host-key override (the
    /// "this time only" trust choice; never written to the catalog).
    ConnectEndpointTrustOnce {
        endpoint_id: endpoint::ClientEndpointId,
    },
    /// Progress of one wizard-driven remote bootstrap worker.
    MachineBootstrap {
        ticket: u64,
        update: shell::MachineBootstrapUpdate,
    },
    /// Result of one machine file browser operation (worker thread).
    MachineFs {
        ticket: u64,
        result: Result<shell::MachineFsOutcome, String>,
    },
    /// Progress of one approved machine recovery worker (known_hosts ops and
    /// interactive auth outcomes).
    MachineAuth {
        update: shell::MachineAuthUpdate,
    },
    /// An askpass prompt arriving from an approved interactive auth worker.
    /// The prompt object carries its responder; the loop parks it until the
    /// TUI answers. Prompt answers are secrets: they flow to the responder
    /// only and are never logged.
    MachineAuthPrompt {
        ticket: u64,
        prompt: Box<crate::remote::SshAskpassPrompt>,
    },
    /// The TUI's answer (or decline) for a parked askpass prompt.
    MachineAuthAnswer {
        ticket: u64,
        answer: Option<String>,
    },
    /// Cancel a running interactive auth attempt.
    MachineAuthCancel {
        ticket: u64,
    },
    ActivateEndpoint {
        endpoint_id: endpoint::ClientEndpointId,
        target: Option<shell::ClientEndpointFocusTarget>,
        /// A superseded handoff deliberately starts a fresh target-on epoch even when source and
        /// latest target have the same identity after restoration.
        force: bool,
    },
    Timer,
}
