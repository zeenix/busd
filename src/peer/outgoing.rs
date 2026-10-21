use std::{
    pin::pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use anyhow::{bail, Result};
use futures_util::future::{select, Either};
use zbus::{names::OwnedUniqueName, Connection, Message};
use zruntime::{mpmc, spawn, Event, EventListener};

use crate::{debug, warn};

/// Where messages for a peer go.
#[derive(Debug, Clone)]
pub enum Recipient {
    /// A client, whose messages go through its queue.
    Client(Outgoing),
    /// The bus itself.
    ///
    /// The bus never waits on a client, as everything it sends to clients goes through their
    /// queues, so a message for it can wait for room on its connection. That holds back a client
    /// that sends to the bus faster than the bus can keep up.
    Bus(Connection),
}

impl Recipient {
    /// Queue `msg` for a client, or, for the bus itself, hand back its connection.
    ///
    /// Queuing never waits, so it can be done while holding any lock, but a message for the bus
    /// waits for room. Send it on the connection that this returns once no lock that the bus might
    /// need to make room is held.
    pub fn queue(&self, msg: &Message) -> Result<Option<Connection>> {
        match self {
            Self::Client(outgoing) => outgoing.queue(msg.clone()).map(|()| None),
            Self::Bus(conn) => Ok(Some(conn.clone())),
        }
    }
}

/// The queue of the messages on their way to a client.
///
/// A task of the queue's own writes them to the client's socket, so that forwarding a message to
/// a client never waits for that client to read, and a client that reads slowly, or not at all,
/// holds up no one but itself. A queue always takes one message, however big, but a client whose
/// queue would hold more than [`MAX_QUEUED_BYTES`] gets disconnected.
#[derive(Debug, Clone)]
pub struct Outgoing {
    sender: mpmc::Sender<Message>,
    /// The bytes of the messages in the queue, and of the one being written.
    queued_bytes: Arc<AtomicUsize>,
    /// Notified when the queue would hold too much, for the task to disconnect the client.
    overflowed: Arc<Event>,
}

impl Outgoing {
    /// Create the queue of the client on `conn`, and spawn the task that writes it.
    pub fn new(conn: Connection, unique_name: OwnedUniqueName) -> Self {
        let (sender, receiver) = mpmc::unbounded();
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let overflowed = Arc::new(Event::new());
        // Listen before anything can be queued, so that no overflow comes before the listener.
        let overflow = overflowed.listen();
        spawn(write(
            conn,
            unique_name,
            receiver,
            queued_bytes.clone(),
            overflow,
        ))
        .detach();

        Self {
            sender,
            queued_bytes,
            overflowed,
        }
    }

    /// Queue `msg` for the client.
    ///
    /// Fails if the client is gone, or if its queue would hold more than [`MAX_QUEUED_BYTES`], in
    /// which case the client gets disconnected.
    pub fn queue(&self, msg: Message) -> Result<()> {
        let len = msg.data().len();
        let queued = self.queued_bytes.fetch_add(len, Ordering::Relaxed);
        if queued > 0 && queued + len > MAX_QUEUED_BYTES {
            self.queued_bytes.fetch_sub(len, Ordering::Relaxed);
            self.overflowed.notify(usize::MAX);
            bail!("the client's queue would hold more than {MAX_QUEUED_BYTES} bytes");
        }
        if self.sender.try_send(msg).is_err() {
            self.queued_bytes.fetch_sub(len, Ordering::Relaxed);
            bail!("the client is gone");
        }

        Ok(())
    }
}

/// How many bytes of messages a client's queue may hold: as many as dbus-daemon's default
/// `max_outgoing_bytes`.
pub const MAX_QUEUED_BYTES: usize = 133_169_152;

/// How many file descriptors a message may carry: as many as Linux passes in one write
/// (`SCM_MAX_FD`), which is how zbus passes those of a message.
///
/// The bus couldn't pass a message with more on, so it disconnects a client that sends one.
pub const MAX_MESSAGE_FDS: usize = 253;

/// Write the queue to the client's socket, until the client is gone or gets disconnected.
///
/// The client gets disconnected when writing to its socket fails, and when its queue overflows,
/// even while a message is on its way, as the client may never read it. A message with file
/// descriptors, for a client that didn't negotiate passing them, is dropped, as the client can't
/// take it.
async fn write(
    conn: Connection,
    unique_name: OwnedUniqueName,
    receiver: mpmc::Receiver<Message>,
    queued_bytes: Arc<AtomicUsize>,
    mut overflow: EventListener,
) {
    let overflowed = loop {
        let msg = match select(receiver.recv(), overflow).await {
            Either::Left((Ok(msg), listener)) => {
                overflow = listener;

                msg
            }
            // The queue is closed and empty: the client is gone.
            Either::Left((Err(_), _)) => return,
            Either::Right(_) => break true,
        };
        let sent = {
            let send = pin!(conn.send(&msg));
            match select(send, overflow).await {
                Either::Left((result, listener)) => Some((result, listener)),
                Either::Right(_) => None,
            }
        };
        let Some((result, listener)) = sent else {
            break true;
        };
        match result {
            Ok(()) => (),
            // The message has file descriptors, and the client didn't negotiate passing them.
            // zbus finds that before it writes anything, so the next messages can still go.
            Err(zbus::Error::Unsupported) => {
                debug!(
                    "Dropping a message with file descriptors for `{unique_name}`, which takes none"
                );
            }
            Err(e) => {
                debug!("Failed to send a message to `{unique_name}`: {e}");

                break false;
            }
        }
        overflow = listener;
        queued_bytes.fetch_sub(msg.data().len(), Ordering::Relaxed);
    };

    if overflowed {
        warn!(
            "Disconnecting `{unique_name}`, as its queue would hold more than {MAX_QUEUED_BYTES} \
             bytes"
        );
    }
    // Have the messages that come for the client fail at once, rather than pile up, and close its
    // connection, for the bus to remove the client, along with its names.
    receiver.close();
    if let Err(e) = conn.close().await {
        debug!("Failed to close the connection of `{unique_name}`: {e}");
    }
}
