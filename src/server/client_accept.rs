use std::io;
use std::sync::{atomic::AtomicBool, atomic::Ordering, Arc};

use interprocess::local_socket::traits::{Listener as _, Stream as _};
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use crate::ipc::LocalListener;
use crate::server::client_transport::{self, ClientHandshakeLimiter, ServerEvent};

/// Accepts pending thin-client connections and starts their handshake readers.
///
/// Admission is bounded by the shared handshake limiter: a connection without a
/// complete hello consumes one of at most 32 handshake slots until it closes,
/// reaches the 4-second absolute deadline, and delivers its registered welcome.
/// The first framed hello is independently capped at `MAX_FRAME_SIZE` (2 MiB);
/// connections that cannot acquire a slot are closed without another thread.
#[cfg(unix)]
pub(crate) fn accept_pending_client_connections(
    listener: &LocalListener,
    next_client_id: &mut u64,
    should_quit: &Arc<AtomicBool>,
    server_event_tx: &mpsc::Sender<ServerEvent>,
    handshake_limiter: &Arc<ClientHandshakeLimiter>,
) -> io::Result<()> {
    loop {
        if should_quit.load(Ordering::Acquire) {
            break;
        }
        match listener.accept() {
            Ok(stream) => {
                let client_id = *next_client_id;
                *next_client_id = next_client_id.saturating_add(1);
                let Some(handshake_permit) = handshake_limiter.try_acquire() else {
                    debug!(client_id, "client handshake concurrency limit reached");
                    continue;
                };

                if let Err(err) = stream.set_nonblocking(true) {
                    warn!(err = %err, "failed to set client stream nonblocking");
                    drop(handshake_permit);
                    continue;
                }

                let should_quit = should_quit.clone();
                let server_event_tx = server_event_tx.clone();
                let spawned = crate::thread_spawn::spawn_named("herdr-client-conn", move || {
                    if let Err(err) = client_transport::handle_client_handshake_with_permit(
                        stream,
                        client_id,
                        &server_event_tx,
                        &should_quit,
                        Some(handshake_permit),
                    ) {
                        debug!(client_id, err = %err, "client handshake failed");
                    }
                });
                if let Err(err) = spawned {
                    warn!(client_id, err = %err, "failed to spawn client connection thread; dropping connection");
                }
            }
            Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(err) => {
                error!(err = %err, "client listener accept failed");
                break;
            }
        }
    }

    Ok(())
}

#[cfg(windows)]
pub(crate) fn spawn_windows_client_accept_thread(
    listener: LocalListener,
    should_quit: Arc<AtomicBool>,
    server_event_tx: mpsc::Sender<ServerEvent>,
    handshake_limiter: Arc<ClientHandshakeLimiter>,
) -> io::Result<std::thread::JoinHandle<()>> {
    crate::thread_spawn::spawn_named("herdr-client-accept", move || {
        let mut next_client_id = 1_u64;
        loop {
            if should_quit.load(Ordering::Acquire) {
                break;
            }
            let stream = match listener.accept() {
                Ok(stream) => stream,
                Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    continue;
                }
                Err(err) => {
                    if should_quit.load(Ordering::Acquire) {
                        break;
                    }
                    error!(err = %err, "client listener accept failed");
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    continue;
                }
            };

            let client_id = next_client_id;
            next_client_id = next_client_id.saturating_add(1);
            let Some(handshake_permit) = handshake_limiter.try_acquire() else {
                debug!(client_id, "client handshake concurrency limit reached");
                continue;
            };

            if let Err(err) = stream.set_nonblocking(true) {
                warn!(err = %err, "failed to set client stream nonblocking");
                drop(handshake_permit);
                continue;
            }

            let should_quit = should_quit.clone();
            let server_event_tx = server_event_tx.clone();
            let spawned = crate::thread_spawn::spawn_named("herdr-client-conn", move || {
                if let Err(err) = client_transport::handle_client_handshake_with_permit(
                    stream,
                    client_id,
                    &server_event_tx,
                    &should_quit,
                    Some(handshake_permit),
                ) {
                    debug!(client_id, err = %err, "client handshake failed");
                }
            });
            if let Err(err) = spawned {
                warn!(client_id, err = %err, "failed to spawn client connection thread; dropping connection");
            }
        }
    })
}

/// Drains pending thin-client connections without starting handshakes.
///
/// During live handoff the old server must not let clients sit in the Unix
/// listener backlog waiting for a welcome frame that will never be sent.
#[cfg(unix)]
pub(crate) fn reject_pending_client_connections(listener: &LocalListener) -> io::Result<()> {
    loop {
        match listener.accept() {
            Ok(_stream) => {}
            Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(err) => {
                error!(err = %err, "client listener reject failed");
                break;
            }
        }
    }

    Ok(())
}
