use std::{
    io,
    pin::pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use zbus::{names::OwnedUniqueName, Connection, Message};
use zruntime::{lock::Mutex, SharedRuntime};

use crate::{debug, warn};

/// Where the bus sends the messages for a peer, or a monitor.
///
/// It is cheap to clone, so that the bus can pick the recipients of a message while it holds its
/// locks, and send the message to them once it has let go of those. The clones share the state of
/// the peer's connection, which a peer hands on to the monitor that it becomes.
///
/// The bus keeps no queue of messages for a client: it writes each message to the client's socket
/// as it forwards it, and waits for room there if need be. A client that keeps a message waiting
/// for longer than its time limit, [`SEND_TIMEOUT`] by default, gets disconnected. So what is
/// limited is how long a client that doesn't read holds up the messages for it, rather than how
/// many bytes of them may pile up for it, as with the quotas of dbus-broker. dbus-broker
/// disconnects a client that can't take a reply or a signal that it expects, as the bus does, but
/// it rejects a method call that the callee can't take, with `LimitsExceeded`, and keeps the
/// callee. The bus disconnects the callee then too, as a send that times out may leave part of its
/// message on the socket.
#[derive(Debug, Clone)]
pub struct Recipient(Kind);

impl Recipient {
    /// A client on `conn`, which the bus gives `timeout` to take each message.
    pub fn client(
        conn: Connection,
        unique_name: OwnedUniqueName,
        timeout: Duration,
    ) -> io::Result<Self> {
        let client = Client {
            conn,
            unique_name,
            runtime: SharedRuntime::current()?,
            timeout,
            sending: Mutex::new(()),
            disconnecting: AtomicBool::new(false),
        };

        Ok(Self(Kind::Client(Arc::new(client))))
    }

    /// The bus itself, on `conn`.
    ///
    /// A message for the bus waits for room on its connection for as long as it takes, which
    /// holds back a client that sends to the bus faster than the bus can keep up. That wait has
    /// no time limit of its own, as the bus never gets disconnected, but it is bounded all the
    /// same: the bus only ever waits on a client within the time limit of that client.
    pub fn bus(conn: Connection) -> Self {
        Self(Kind::Bus(conn))
    }

    /// Send `msg`.
    ///
    /// A client that a send to fails, or doesn't take the message within its time limit, gets
    /// disconnected: the end of its connection has the bus remove it, along with its names. A
    /// message with file descriptors, for a client that didn't negotiate passing them, is dropped
    /// instead, as the client can still take the messages after it.
    pub async fn send(&self, msg: &Message) {
        match &self.0 {
            Kind::Client(client) => client.send(msg).await,
            Kind::Bus(conn) => {
                if let Err(e) = conn.send(msg).await {
                    warn!("Error sending message to the bus: {}", e);
                }
            }
        }
    }
}

/// How long the bus waits for a client to take a message, by default, before it disconnects the
/// client.
///
/// The wait counts from when the bus starts to send the message, so it covers the wait for the
/// other messages on their way to the client, as well as the write of this one.
///
/// It is long enough for a client that reads its socket to take even the largest message, of
/// 128 MiB, as that only takes reading at 128 MiB/s. And it is short next to the 25 s that a method
/// call waits for its reply by default, with libdbus, GDBus and sd-bus: the bus forwards the
/// messages of each sender one after the other, so a client that stops reading while it is
/// subscribed to the signals of the bus holds up every message of the bus, its replies included,
/// for this long before it gets disconnected. Several such clients that stop taking messages at the
/// same one hold it up for this long all together, as the bus sends a message to all of its
/// recipients at once.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
enum Kind {
    Client(Arc<Client>),
    Bus(Connection),
}

/// The connection of a client, as the bus sends messages on it.
#[derive(Debug)]
struct Client {
    conn: Connection,
    unique_name: OwnedUniqueName,
    runtime: SharedRuntime,
    timeout: Duration,
    /// Held by each send, from its wait for the connection to the end of its write, so that the
    /// sends to the client go one at a time.
    sending: Mutex<()>,
    /// Whether the client is being disconnected, as a send to it failed or timed out.
    ///
    /// Such a send may have written part of its message, so nothing more is written to the
    /// client. A send checks this once its turn comes, which is only once the send that failed
    /// has let go of the connection.
    disconnecting: AtomicBool,
}

impl Client {
    async fn send(&self, msg: &Message) {
        if self.disconnecting.load(Ordering::SeqCst) {
            return;
        }
        let (failure, first) = {
            let send = pin!(self.runtime.timeout(self.timeout, self.send_in_turn(msg)));
            let failure = match send.await {
                Ok(Ok(())) => return,
                // The message has file descriptors, and the client didn't negotiate passing
                // them. zbus finds that before it writes anything.
                Ok(Err(zbus::Error::Unsupported)) => {
                    debug!(
                        "Dropping a message with file descriptors for `{}`, which takes none",
                        self.unique_name
                    );

                    return;
                }
                Ok(Err(e)) => Failure::Error(e),
                Err(_) => Failure::Timeout,
            };
            // A send that timed out is still there, with whatever part of its message it wrote,
            // and it may hold the connection until the end of this block. Flag the client before
            // it lets go, so that no other send writes after that part.
            let first = !self.disconnecting.swap(true, Ordering::SeqCst);

            (failure, first)
        };
        // Another send got here first, and disconnects the client.
        if !first {
            return;
        }
        match failure {
            Failure::Timeout => warn!(
                "Disconnecting `{}`, as it didn't take a message within {:?}",
                self.unique_name, self.timeout
            ),
            Failure::Error(e) => debug!(
                "Disconnecting `{}`, as sending a message to it failed: {e}",
                self.unique_name
            ),
        }
        // Closing the connection ends the stream of the messages from the client, and the bus
        // removes the client then. It waits for the send that holds the connection, if any, which
        // its own time limit bounds.
        if let Err(e) = self.conn.clone().close().await {
            debug!(
                "Failed to close the connection of `{}`: {e}",
                self.unique_name
            );
        }
    }

    /// Send `msg` once no other send to the client is under way, unless the client is being
    /// disconnected by then.
    async fn send_in_turn(&self, msg: &Message) -> zbus::Result<()> {
        let _turn = self.sending.lock().await;
        if self.disconnecting.load(Ordering::SeqCst) {
            return Ok(());
        }

        self.conn.send(msg).await
    }
}

/// Why a send to a client failed.
enum Failure {
    Timeout,
    Error(zbus::Error),
}
