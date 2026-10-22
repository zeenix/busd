use crate::{debug, trace, warn};
use anyhow::{bail, Context, Result};
use futures_util::{
    future::{select, Either},
    stream::StreamExt,
};
use std::{
    collections::BTreeMap,
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};
use zbus::{
    connection::{self, AuthMechanism},
    message,
    names::{BusName, OwnedUniqueName, UniqueName},
    Connection, Message, Optional, OwnedGuid,
};
use zruntime::{lock::RwLock, spawn, EventListener};

use crate::{
    fdo,
    match_rules::MatchRules,
    name_registry::{NameOwnerChanged, NameRegistry},
    peer::{Monitor, Peer, Recipient, Stream, MAX_MESSAGE_FDS},
};

/// The peers of the bus, and its monitors and names.
///
/// The bus forwards the messages of each peer, including those of the bus itself, one after the
/// other, from the task that serves the peer, so that each recipient gets them in the order that
/// the peer sent them, monitors included. It picks the recipients of a message while it holds the
/// locks of the peers, monitors and names, and only sends the message once it has let go of them,
/// as a send may wait for its recipient to read.
#[derive(Debug)]
pub struct Peers {
    peers: RwLock<BTreeMap<OwnedUniqueName, Peer>>,
    monitors: RwLock<BTreeMap<OwnedUniqueName, Monitor>>,
    name_registry: RwLock<NameRegistry>,
    /// The connection on which the bus serves the D-Bus API, from when the bus is among its peers
    /// until the bus goes.
    bus_conn: Mutex<Option<Connection>>,
}

impl Peers {
    pub fn new() -> Arc<Self> {
        let name_registry = NameRegistry::default();

        Arc::new(Self {
            peers: RwLock::new(BTreeMap::new()),
            monitors: RwLock::new(BTreeMap::new()),
            name_registry: RwLock::new(name_registry),
            bus_conn: Mutex::new(None),
        })
    }

    /// Add a client, which the bus gives `send_timeout` to take each message.
    pub async fn add(
        self: &Arc<Self>,
        guid: &OwnedGuid,
        id: usize,
        builder: connection::Builder<'_>,
        auth_mechanism: AuthMechanism,
        send_timeout: Duration,
    ) -> Result<()> {
        // Perform the connection build and SASL handshake without holding the peers lock, as it
        // involves async I/O and would otherwise block all other peer operations (Hello
        // handling, etc.).
        let (peer, peer_stream) =
            Peer::new(guid.clone(), id, builder, auth_mechanism, send_timeout).await?;
        let unique_name = peer.unique_name().clone();
        let listener = peer.listen_cancellation();

        let mut peers = self.peers_mut().await;
        match peers.get(&unique_name) {
            Some(peer) => panic!(
                "Unique name `{}` re-used. We're in deep trouble if this happens",
                peer.unique_name()
            ),
            None => {
                spawn(
                    self.clone()
                        .serve_peer(peer_stream, listener, unique_name.clone()),
                )
                .detach();
                peers.insert(unique_name.clone(), peer);
            }
        }

        Ok(())
    }

    /// Add the bus itself, which serves the D-Bus API on `conn`: what the bus sends on it comes out
    /// of `msg_stream`.
    pub async fn add_us(self: &Arc<Self>, msg_stream: zbus::MessageStream, conn: Connection) {
        *self.bus_conn() = Some(conn);
        let (peer, peer_stream) = Peer::new_us(msg_stream).await;
        let unique_name = peer.unique_name().clone();
        let listener = peer.listen_cancellation();

        let mut peers = self.peers_mut().await;
        match peers.get(&unique_name) {
            Some(peer) => panic!(
                "Unique name `{}` re-used. We're in deep trouble if this happens",
                peer.unique_name()
            ),
            None => {
                spawn(
                    self.clone()
                        .serve_peer(peer_stream, listener, unique_name.clone()),
                )
                .detach();
                peers.insert(unique_name.clone(), peer);
            }
        }
    }

    /// Let go of the connection on which the bus serves the D-Bus API, as the bus goes.
    ///
    /// What the bus sends on that connection comes back to it as the messages of its own peer, so
    /// holding on to the connection would keep the task that serves that peer, and with it the
    /// peers, from ever ending.
    pub fn remove_us(&self) {
        self.bus_conn().take();
    }

    pub async fn peers(&self) -> impl Deref<Target = BTreeMap<OwnedUniqueName, Peer>> + '_ {
        self.peers.read().await
    }

    pub async fn peers_mut(&self) -> impl DerefMut<Target = BTreeMap<OwnedUniqueName, Peer>> + '_ {
        self.peers.write().await
    }

    pub async fn name_registry(&self) -> impl Deref<Target = NameRegistry> + '_ {
        self.name_registry.read().await
    }

    pub async fn name_registry_mut(&self) -> impl DerefMut<Target = NameRegistry> + '_ {
        self.name_registry.write().await
    }

    pub async fn make_monitor(
        self: &Arc<Self>,
        peer_name: &UniqueName<'_>,
        match_rules: MatchRules,
    ) -> bool {
        let monitor = {
            let mut peers = self.peers_mut().await;
            let peer = match peers.remove(peer_name.as_str()) {
                Some(peer) => peer,
                None => {
                    return false;
                }
            };

            peer.become_monitor(match_rules)
        };

        let monitor_monitoring_fut = monitor.monitor();
        let unique_name = monitor.unique_name().clone();
        let peers = self.clone();
        self.monitors
            .write()
            .await
            .insert(unique_name.clone(), monitor);

        spawn(async move {
            monitor_monitoring_fut.await;
            peers.monitors.write().await.remove(&unique_name);
            debug!("Monitor {} disconnected", unique_name);
        })
        .detach();

        true
    }

    /// Signal a change of the owner of a name.
    ///
    /// The signals go out on the connection of the bus, like its replies and its other signals,
    /// so that they all reach each recipient in the order that the bus sends them.
    pub async fn notify_name_changes(&self, name_owner_changed: NameOwnerChanged) -> Result<()> {
        let bus = self.bus_conn().clone().context("The bus isn't up")?;
        let name = BusName::from(name_owner_changed.name);
        let old_owner = name_owner_changed.old_owner.map(UniqueName::from);
        let new_owner = name_owner_changed.new_owner.map(UniqueName::from);

        // First broadcast the name change signal.
        let msg = Message::signal(fdo::DBus::PATH, fdo::DBus::INTERFACE, "NameOwnerChanged")
            .sender(fdo::BUS_NAME)
            .build(&(
                &name,
                Optional::from(old_owner.clone()),
                Optional::from(new_owner.clone()),
            ))?;
        bus.send(&msg).await?;

        // Now unicast the appropriate signal to the old and new owners.
        if let Some(old_owner) = old_owner {
            let msg = Message::signal(fdo::DBus::PATH, fdo::DBus::INTERFACE, "NameLost")
                .sender(fdo::BUS_NAME)
                .destination(old_owner)
                .build(&name)?;
            bus.send(&msg).await?;
        }
        if let Some(new_owner) = new_owner {
            let msg = Message::signal(fdo::DBus::PATH, fdo::DBus::INTERFACE, "NameAcquired")
                .sender(fdo::BUS_NAME)
                .destination(new_owner)
                .build(&name)?;
            bus.send(&msg).await?;
        }

        Ok(())
    }

    async fn serve_peer(
        self: Arc<Self>,
        mut peer_stream: Stream,
        mut cancellation_listener: EventListener,
        unique_name: OwnedUniqueName,
    ) -> Result<()> {
        loop {
            let msg = match select(cancellation_listener, peer_stream.next()).await {
                Either::Left(_) | Either::Right((None, _)) => {
                    trace!("Peer `{}` disconnected", unique_name);

                    break;
                }
                Either::Right((Some(msg), listener)) => {
                    cancellation_listener = listener;

                    match msg {
                        Ok(msg) => msg,
                        Err(e) => {
                            debug!("{e}");

                            continue;
                        }
                    }
                }
            };
            // Passing such a message on would fail, and get its recipients disconnected.
            if msg.data().fds().len() > MAX_MESSAGE_FDS {
                warn!(
                    "Disconnecting `{unique_name}`, as it sent a message with more than \
                     {MAX_MESSAGE_FDS} file descriptors"
                );

                break;
            }

            match msg.message_type() {
                message::Type::Signal => self.broadcast_msg(msg).await,
                _ => match msg.header().destination() {
                    Some(dest) => {
                        if let Err(e) = self.send_msg(msg.clone(), dest.clone()).await {
                            warn!("{}", e);
                        }
                    }
                    // peer::Stream ensures a valid destination so this isn't exactly needed.
                    _ => bail!("invalid message: {:?}", msg),
                },
            };
        }

        // Stream is done means the peer disconnected or it became a monitor, unless the peer sent
        // a message that gets it disconnected. Remove it from the list of peers: that, and the end
        // of its stream, close its connection.
        if self.peers_mut().await.remove(&unique_name).is_none() {
            // This means peer was turned into a monitor. `Monitoring` iface will emit the signals.
            return Ok(());
        }
        let names_changes = self
            .name_registry_mut()
            .await
            .release_all(unique_name.inner().clone())
            .await;
        for changed in names_changes {
            self.notify_name_changes(changed).await?;
        }
        self.notify_name_changes(NameOwnerChanged {
            name: BusName::from(unique_name.clone()).into(),
            old_owner: Some(unique_name.clone()),
            new_owner: None,
        })
        .await?;

        Ok(())
    }

    async fn send_msg(&self, msg: Message, destination: BusName<'_>) -> Result<()> {
        trace!(
            "Forwarding message: {:?}, destination: {}",
            msg,
            destination
        );
        match destination {
            BusName::Unique(dest) => self.send_msg_to_unique_name(msg, dest.clone()).await,
            BusName::WellKnown(name) => {
                let dest = match self.name_registry().await.lookup(name.clone()) {
                    Some(dest) => dest,
                    None => bail!("unknown destination: {}", name),
                };
                self.send_msg_to_unique_name(msg, (&*dest).into()).await
            }
        }

        Ok(())
    }

    async fn send_msg_to_unique_name(&self, msg: Message, destination: UniqueName<'_>) {
        let recipient = self
            .peers
            .read()
            .await
            .get(destination.as_str())
            .map(|peer| peer.recipient().clone());
        let mut monitors = Vec::new();
        {
            let name_registry = self.name_registry().await;
            self.add_monitors(&msg, &name_registry, &mut monitors).await;
        }

        match recipient {
            Some(recipient) => recipient.send(&msg).await,
            None => debug!("no peer for destination `{destination}`"),
        }
        for monitor in monitors {
            monitor.send(&msg).await;
        }
    }

    async fn broadcast_msg(&self, msg: Message) {
        trace!("Broadcasting message: {:?}", msg);
        let mut recipients = Vec::new();
        {
            let name_registry = self.name_registry().await;
            for peer in self.peers.read().await.values() {
                if !peer.interested(&msg, &name_registry) {
                    trace!("Peer {} not interested in {msg:?}", peer.unique_name());
                    continue;
                }

                recipients.push(peer.recipient().clone());
            }

            self.add_monitors(&msg, &name_registry, &mut recipients)
                .await;
        }

        for recipient in recipients {
            recipient.send(&msg).await;
        }
    }

    /// Add the monitors that `msg` goes to, to `recipients`.
    async fn add_monitors(
        &self,
        msg: &Message,
        name_registry: &NameRegistry,
        recipients: &mut Vec<Recipient>,
    ) {
        let monitors = self.monitors.read().await;
        if monitors.is_empty() {
            return;
        }
        trace!(
            "Broadcasting message to {} monitors: {:?}",
            monitors.len(),
            msg
        );
        for monitor in monitors.values() {
            if !monitor.interested(msg, name_registry) {
                trace!(
                    "Monitor {} not interested in {msg:?}",
                    monitor.unique_name()
                );
                continue;
            }

            recipients.push(monitor.recipient().clone());
        }
    }

    fn bus_conn(&self) -> MutexGuard<'_, Option<Connection>> {
        // Nothing can panic while the lock is held.
        self.bus_conn.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
