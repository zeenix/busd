//! The hand-over of the peers, monitors and names to the next instance of the bus.

use std::{
    collections::HashMap,
    iter,
    num::NonZeroU32,
    os::fd::OwnedFd,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use futures_util::future::join_all;
use zbus::{
    connection::Handover, message, names::OwnedUniqueName, Connection, Message, MessageStream,
};
use zruntime::{spawn, SharedRuntime};

use super::{Peers, Routing, Served};
use crate::{
    bus::handover::{ClientHandover, MonitorState, NameState, PeerState, Snapshot},
    debug, fdo,
    name_registry::{NameOwner, NameRegistry},
    peer::{Caller, HandedOver, Monitor, Peer, Recipient, Stream},
    warn,
};

impl Peers {
    /// Pause the bus, for it to hand over: have the task that serves each client, peer or
    /// monitor, take no more of its messages, once it is done with those that it took, and give
    /// the stream of the messages back, and wait for that. The clients that connect from now on
    /// pause as soon as they are connected, and those in the middle of their handshakes have
    /// `timeout` to be done with them.
    ///
    /// The clients have as long as they have to take a message, from now on, to take all that the
    /// bus sends them meanwhile, so that a client that doesn't read holds the hand-over up for no
    /// longer than that, rather than for that long each time that the bus sends it something. A
    /// wait for a task to be done with what it took is bounded by that too. Once the bus is
    /// paused, it takes no more clients: one whose handshake is still under way by then gets
    /// disconnected as the handshake ends, as [`Peers::add`] says, or as the bus goes.
    pub(crate) async fn pause(&self, timeout: Duration) -> Result<()> {
        let runtime = SharedRuntime::current()?;
        let deadline = Instant::now() + timeout;
        {
            let mut routing = self.routing_mut().await;
            let since = Instant::now();
            routing.handing_over = Some(since);
            for peer in routing.peers.values().filter(|peer| !peer.is_bus()) {
                peer.pause(since);
            }
            for monitor in routing.monitors.values() {
                monitor.pause(since);
            }
        }
        loop {
            let connecting = Instant::now() < deadline && self.chores().connecting > 0;
            if !connecting {
                // Under the lock that adding a client takes, so that none comes in between.
                let mut routing = self.routing_mut().await;
                if paused(&mut routing) {
                    routing.sealed = true;

                    return Ok(());
                }
            }
            runtime.sleep(SETTLE_INTERVAL).await;
        }
    }

    /// Wait for the bus to settle: to have answered every call that it got, and to have no chores
    /// under way, with the peers to have got all of the messages that the bus sent.
    ///
    /// A hand-over waits for this once the bus is paused, so the bus only has its own work to
    /// finish, as nothing new comes in for it to do, but for what the clients whose connections
    /// ended meanwhile sent before that, which it takes, as [`Peers::serve_gone`] says: those that
    /// went, and those that the bus disconnected, as they didn't take what it sent them in time, or
    /// as a send to them failed, as one to a client that went does.
    pub(crate) async fn settle(self: &Arc<Self>) -> Result<()> {
        let runtime = SharedRuntime::current()?;
        loop {
            let started = self.chores_done().await;
            // The calls of the interfaces that zbus adds to the bus, such as
            // `org.freedesktop.DBus.Properties`, are answered by tasks of their own.
            if !self.bus_calls_answered().await {
                runtime.sleep(SETTLE_INTERVAL).await;

                continue;
            }
            self.barrier().await?;
            // Each client that went is served on as a chore, so the bus isn't settled until then.
            self.serve_gone().await;
            let settled = {
                let chores = self.chores();

                chores.running == 0 && chores.started == started
            };
            // A chore that started while the barrier was on its way may have sent messages after
            // it.
            if settled && self.bus_calls_answered().await {
                return Ok(());
            }
        }
    }

    /// The peers, monitors and names, as the bus hands them over, and what it hands over of the
    /// connection of each client.
    ///
    /// The connections of the clients are handed over all at once, each through the stream of its
    /// messages, within the time that its client has to take a message, as a client that doesn't
    /// read can hold its connection up. That of a client that went since the bus settled, as it
    /// closed its end, is handed over too, with what zbus read of it, for the next instance to pass
    /// that on and clean up after the client. A client whose connection fails to be handed over,
    /// as when the bus is disconnecting it, when the connection ended on an error, or when its
    /// socket fails to be duplicated, as the process has too many file descriptors open, gets
    /// disconnected: the bus leaves it out, so that the next instance cleans up after it as after a
    /// client that went.
    pub(crate) async fn snapshot(&self) -> Result<(Snapshot, Vec<ClientHandover>)> {
        let runtime = SharedRuntime::current()?;
        let mut clients = Vec::new();
        let names = {
            let mut routing = self.routing_mut().await;
            let routing = &mut *routing;
            for peer in routing.peers.values_mut().filter(|peer| !peer.is_bus()) {
                let state = PeerState {
                    id: peer.id() as u64,
                    greeted: peer.greeted(),
                    connection: Vec::new(),
                    match_rules: peer.match_rules().clone(),
                    pending_calls: peer
                        .pending_calls()
                        .records()
                        .map(|(caller_id, caller, serial)| (caller_id as u64, caller, serial.get()))
                        .collect(),
                };
                clients.push(Client {
                    id: peer.id(),
                    conn: peer.conn().clone(),
                    recipient: peer.recipient().clone(),
                    stream: peer.paused().take(),
                    state: ClientState::Peer(state),
                });
            }
            for monitor in routing.monitors.values_mut() {
                let state = MonitorState {
                    id: monitor.id() as u64,
                    connection: Vec::new(),
                    match_rules: monitor.match_rules().clone(),
                };
                clients.push(Client {
                    id: monitor.id(),
                    conn: monitor.conn().clone(),
                    recipient: monitor.recipient().clone(),
                    stream: monitor.paused().take(),
                    state: ClientState::Monitor(state),
                });
            }

            routing
                .name_registry
                .all_names()
                .iter()
                .map(|(name, entry)| NameState {
                    name: name.clone(),
                    owners: iter::once(entry.owner())
                        .chain(entry.waiting_list())
                        .map(|owner| (owner.unique_name().clone(), owner.allow_replacement()))
                        .collect(),
                })
                .collect()
        };
        let handing_over = clients.iter_mut().map(|client| client.hand_over(&runtime));
        let handovers = join_all(handing_over).await;

        let mut snapshot = Snapshot {
            peers: Vec::new(),
            monitors: Vec::new(),
            names,
        };
        let mut handed_over = Vec::new();
        for (client, handover) in clients.into_iter().zip(handovers) {
            let mut state = client.state;
            if let Some((connection, fds)) = handover {
                *state.connection() = connection;
                handed_over.push(ClientHandover { id: client.id, fds });
            }
            match state {
                ClientState::Peer(state) => snapshot.peers.push(state),
                ClientState::Monitor(state) => snapshot.monitors.push(state),
            }
        }

        Ok((snapshot, handed_over))
    }

    /// Note that the reply to the barrier with `serial` came back.
    pub(super) fn barrier_returned(&self, serial: u32) {
        let mut chores = self.chores();
        chores.barrier = chores.barrier.max(serial);
        drop(chores);
        self.chores_changed.notify(usize::MAX);
    }

    /// Let go of the clients without disconnecting them, as the bus handed them over: nothing more
    /// goes to them from this instance of the bus.
    pub(crate) async fn retire(&self) {
        // Nothing more comes from the bus, such as its errors to the calls that wait on the peers
        // that go now.
        self.remove_us();
        let mut routing = self.routing_mut().await;
        for peer in routing.peers.values() {
            peer.recipient().hand_over();
        }
        for monitor in routing.monitors.values() {
            monitor.recipient().hand_over();
        }
        routing.peers.clear();
        routing.monitors.clear();
        routing.name_registry = NameRegistry::default();
    }

    /// Serve the peers and monitors of `snapshot` on the connections of `clients`, and give the
    /// peers their names, as the instance of the bus that handed them over did.
    ///
    /// The peers and the monitors whose sockets are missing are gone, as when the service manager
    /// didn't take their sockets, or let go of them, so the bus cleans up after them as after any
    /// peer that goes, and after those whose connections it fails to serve as well, such as those
    /// that lack file descriptors that they sent. It fails, if at all, before it serves any client.
    pub(crate) async fn restore(
        self: &Arc<Self>,
        snapshot: Snapshot,
        mut clients: HashMap<usize, ClientHandover>,
        send_timeout: Duration,
    ) -> Result<()> {
        let Snapshot {
            peers,
            monitors,
            names,
        } = snapshot;
        // The connections start reading as they are created, but the bus only forwards what comes
        // in once all of its peers, monitors and names are back.
        let mut restored = Vec::new();
        let mut gone = Vec::new();
        for state in peers {
            let id = usize::try_from(state.id)?;
            let unique_name = OwnedUniqueName::try_from(format!(":busd.{id}"))?;
            let records = pending_call_records(state.pending_calls)?;
            let Some(client) = clients.remove(&id) else {
                debug!("Peer `{unique_name}` went while the bus handed over");
                gone.push((unique_name, records, Vec::new()));

                continue;
            };
            let handed_over = async {
                let client = HandedOver {
                    id,
                    handover: Handover::decode(&state.connection, client.fds)?,
                    greeted: state.greeted,
                    match_rules: state.match_rules,
                };

                Peer::handed_over(client, send_timeout, &self.users).await
            };
            match handed_over.await {
                Ok((peer, stream)) => restored.push((peer, stream, records)),
                Err(e) => {
                    warn!("Disconnecting `{unique_name}`, as serving it failed: {e}");
                    gone.push((unique_name, records, Vec::new()));
                }
            }
        }
        let mut restored_monitors = Vec::new();
        for state in monitors {
            let id = usize::try_from(state.id)?;
            let Some(client) = clients.remove(&id) else {
                continue;
            };
            let handed_over = async {
                let client = HandedOver {
                    id,
                    handover: Handover::decode(&state.connection, client.fds)?,
                    greeted: true,
                    match_rules: Default::default(),
                };

                Peer::handed_over(client, send_timeout, &self.users).await
            };
            match handed_over.await {
                Ok((peer, stream)) => {
                    let monitor = peer.become_monitor(state.match_rules);
                    restored_monitors.push((monitor, stream.into_inner()));
                }
                Err(e) => {
                    warn!("Disconnecting the monitor `:busd.{id}`, as serving it failed: {e}")
                }
            }
        }
        {
            let mut routing = self.routing_mut().await;
            for NameState { name, owners } in names {
                let mut owners = owners
                    .into_iter()
                    .map(|(unique_name, replace)| NameOwner::new(unique_name, replace));
                let owner = owners.next().context("A name with no owner")?;
                routing.name_registry.insert(name, owner, owners);
            }
            // The names of the peers that went pass on to those that queue for them before the bus
            // serves anyone, so that no call to one of those names finds its owner gone.
            for (unique_name, _, names_changes) in &mut gone {
                *names_changes = routing
                    .name_registry
                    .release_all(unique_name.inner().clone())
                    .await;
            }
            // The calls that wait on the peers, of the callers that are back too, the bus itself
            // among them: the bus would forget the calls of the others as it cleans up after them.
            let callers: HashMap<usize, Arc<Caller>> = routing
                .peers
                .values()
                .map(|peer| (peer.id(), peer.caller().clone()))
                .chain(
                    restored
                        .iter()
                        .map(|(peer, _, _)| (peer.id(), peer.caller().clone())),
                )
                .collect();
            for (peer, stream, records) in restored {
                for (caller_id, _, serial) in records {
                    if let Some(caller) = callers.get(&caller_id) {
                        peer.pending_calls()
                            .add_handed_over(caller_id, caller, serial);
                    }
                }
                self.insert(&mut routing, peer, stream);
            }
            for (monitor, stream) in restored_monitors {
                self.insert_monitor(&mut routing, monitor, stream);
            }
        }
        // The bus serves its clients from here on, so it goes on whatever comes.
        for (unique_name, records, names_changes) in gone {
            let waiting = records
                .into_iter()
                .map(|(_, caller, serial)| (caller, serial))
                .collect();
            let cleaned_up = self.finish_clean_up(&unique_name, names_changes, waiting);
            if let Err(e) = cleaned_up.await {
                warn!("Failed to clean up after `{unique_name}`: {e}");
            }
        }
        if !clients.is_empty() {
            warn!(
                "Closing {} sockets that were handed over for no client",
                clients.len()
            );
        }

        Ok(())
    }

    /// Add `monitor`, whose messages come out of `stream`, to the monitors of `routing`, and serve
    /// it until it goes.
    fn insert_monitor(
        self: &Arc<Self>,
        routing: &mut Routing,
        monitor: Monitor,
        stream: MessageStream,
    ) {
        let unique_name = monitor.unique_name().clone();
        let listener = monitor.listen_cancellation();
        routing.monitors.insert(unique_name.clone(), monitor);
        spawn(self.clone().serve_monitor(stream, listener, unique_name)).detach();
    }

    /// Wait for no chore of the bus to be under way, and how many have started by then.
    async fn chores_done(&self) -> u64 {
        loop {
            let listener = self.chores_changed.listen();
            let started = {
                let chores = self.chores();

                (chores.running == 0).then_some(chores.started)
            };
            if let Some(started) = started {
                return started;
            }
            listener.await;
        }
    }

    /// Whether the bus answered each call to it that waits for its reply.
    async fn bus_calls_answered(&self) -> bool {
        self.routing()
            .await
            .peers
            .get(fdo::BUS_NAME)
            .is_none_or(|bus| bus.pending_calls().is_empty())
    }

    /// Wait for the bus to have answered every call that it got so far, and for its peers to have
    /// got the messages that it sent until then.
    ///
    /// The bus answers the calls of its D-Bus interfaces one at a time, in the order that they come
    /// in, and sends its replies, and its signals, on one connection, which its peer passes on in
    /// that order. So the bus makes a call of its own, to itself, behind those of its peers, and
    /// waits for the reply to come back out of its peer, which `serve_peer` doesn't pass on.
    async fn barrier(&self) -> Result<()> {
        let conn = self
            .routing()
            .await
            .peers
            .get(fdo::BUS_NAME)
            .map(|bus| bus.conn().clone())
            .context("The bus isn't up")?;
        let serial = {
            let mut chores = self.chores();
            chores.barriers_sent += 1;

            NonZeroU32::new(chores.barriers_sent).context("Too many barriers")?
        };
        let call = Message::method_call(fdo::DBus::PATH, "GetId")
            .interface(fdo::DBus::INTERFACE)
            .destination(fdo::BUS_NAME)
            .sender(fdo::BUS_NAME)
            .serial(serial)
            .build(&())?;
        let mut listener = self.chores_changed.listen();
        conn.send(&call).await?;
        while self.chores().barrier < serial.get() {
            listener.await;
            listener = self.chores_changed.listen();
        }

        Ok(())
    }

    /// Serve on the clients, peers and monitors, whose connections ended while the bus paused,
    /// once the tasks that served them gave the streams of their messages back, each as a chore of
    /// its own, until its stream ends: the bus takes what zbus read of them, and cleans up after
    /// them, as after any client that goes. Those are the clients that went, and those that the bus
    /// disconnected meanwhile, once their connections are closed.
    ///
    /// zbus hands the connection of a client that closed its end over, with what it read of the
    /// client, but not one that ended on an error, nor one that the bus closed, as it does once a
    /// send to the client fails, and what zbus read of such a client would be lost otherwise. The
    /// bus closes the connection of a client that went too, if it sends the client anything before
    /// this finds it gone, as the send fails. On Linux, a client that closes its end while its
    /// socket holds messages of the bus that it didn't read, as one that sends a signal, or a
    /// reply, and exits without reading the signals of the bus, ends its connection on an error
    /// after what it sent, rather than at its end: the bus reads `ECONNRESET` there. So the bus
    /// serves the clients whose connections ended here, while their recipients are still its
    /// clients, whatever their connections ended on, and leaves only those that go after that to
    /// the hand-over.
    async fn serve_gone(self: &Arc<Self>) {
        let mut guard = self.routing_mut().await;
        let routing = &mut *guard;
        for peer in routing.peers.values_mut() {
            if !peer.conn().is_closed() {
                continue;
            }
            let Some(stream) = peer.paused().take() else {
                continue;
            };
            let stream = Stream::for_peer(peer, stream);
            let serving =
                self.clone()
                    .serve_peer(stream, peer.listen_cancellation(), Served::new(peer));
            let chore = self.chore();
            spawn(async move {
                let _chore = chore;

                serving.await
            })
            .detach();
        }
        for monitor in routing.monitors.values_mut() {
            if !monitor.conn().is_closed() {
                continue;
            }
            let Some(stream) = monitor.paused().take() else {
                continue;
            };
            let serving = self.clone().serve_monitor(
                stream,
                monitor.listen_cancellation(),
                monitor.unique_name().clone(),
            );
            let chore = self.chore();
            spawn(async move {
                let _chore = chore;

                serving.await
            })
            .detach();
        }
    }
}

/// Take the replies to the barriers of a hand-over out of `batch`, a batch of messages of the bus,
/// and the serial of the last call that they answer, if any.
///
/// The bus makes its barriers in the name of `org.freedesktop.DBus`, so they are the only replies
/// of the bus that go to the bus itself.
pub(super) fn take_barriers(batch: &mut Vec<Message>) -> Option<u32> {
    let mut last = None;
    batch.retain(|msg| {
        let header = msg.header();
        let barrier = matches!(
            msg.message_type(),
            message::Type::MethodReturn | message::Type::Error
        ) && header
            .destination()
            .is_some_and(|destination| destination.as_str() == fdo::BUS_NAME);
        if barrier {
            last = header.reply_serial().map(NonZeroU32::get).max(last);
        }

        !barrier
    });

    last
}

/// Whether the task that served each client of `routing`, peer or monitor, gave the stream of its
/// messages back, as the bus pauses.
fn paused(routing: &mut Routing) -> bool {
    routing
        .peers
        .values_mut()
        .all(|peer| peer.is_bus() || peer.paused().is_some())
        && routing
            .monitors
            .values_mut()
            .all(|monitor| monitor.paused().is_some())
}

/// A client, as the bus hands it over: see [`Peers::snapshot`].
struct Client {
    /// The number of the client.
    id: usize,
    conn: Connection,
    recipient: Recipient,
    /// The stream of the messages of the client, which the task that served it gave back.
    stream: Option<MessageStream>,
    state: ClientState,
}

impl Client {
    /// Hand the connection of the client over, through the stream of its messages, within the time
    /// that the client has to take a message, and encode it: none if that fails, or takes longer.
    ///
    /// A client whose connection ended as the client closed its end, such as once it sent a last
    /// message, is handed over all the same, with what zbus read of it, which the next instance
    /// passes on before it cleans up after the client. A client that the bus is disconnecting
    /// isn't handed over. One whose connection fails to be handed over gets closed, and so does one
    /// whose hand-over takes too long.
    async fn hand_over(&mut self, runtime: &SharedRuntime) -> Option<(Vec<u8>, Vec<OwnedFd>)> {
        let name = self.recipient.unique_name()?;
        if self.recipient.disconnecting() {
            debug!("Leaving `{name}` out of the hand-over, as the bus is disconnecting it");

            return None;
        }
        let stream = self.stream.as_mut()?;
        let timeout = self.recipient.send_timeout()?;
        let handing_over = async { stream.hand_over().await?.encode() };
        let error = match runtime.timeout(timeout, handing_over).await {
            Ok(Ok(encoded)) => return Some(encoded),
            Ok(Err(e)) => e.to_string(),
            Err(_) => "it took too long".to_owned(),
        };
        warn!("Disconnecting `{name}`, as its connection failed to be handed over: {error}");
        // The stream still has the messages that zbus read of the client and that the bus didn't
        // take, such as those that zbus read before the connection ended on an error, but the bus
        // can't forward them anymore: the connections of the clients are handed over all at once,
        // so those that the messages are for may have been handed over already. They go with the
        // stream. The connection reads nothing more, and all that is left is to close it.
        let name = name.clone();
        let conn = self.conn.clone();
        spawn(async move {
            if let Err(e) = conn.close().await {
                debug!("Failed to close the connection of `{name}`: {e}");
            }
        })
        .detach();

        None
    }
}

/// What the bus hands over of a client, but for the file descriptors of its connection.
enum ClientState {
    Peer(PeerState),
    Monitor(MonitorState),
}

impl ClientState {
    /// What the bus hands over of the connection of the client: its hand-over, as zbus encodes it.
    fn connection(&mut self) -> &mut Vec<u8> {
        match self {
            Self::Peer(state) => &mut state.connection,
            Self::Monitor(state) => &mut state.connection,
        }
    }
}

/// The records of the calls that wait on a peer, as the bus hands them over, `records`: the number
/// of the caller of each, its unique name, and the serial of the call.
fn pending_call_records(
    records: Vec<(u64, OwnedUniqueName, u32)>,
) -> Result<Vec<(usize, OwnedUniqueName, NonZeroU32)>> {
    records
        .into_iter()
        .map(|(caller_id, caller, serial)| {
            let serial = NonZeroU32::new(serial).context("A call with no serial")?;

            Ok((usize::try_from(caller_id)?, caller, serial))
        })
        .collect()
}

/// How long a wait for the bus to pause, or to settle, waits before it checks again.
const SETTLE_INTERVAL: Duration = Duration::from_millis(1);

#[cfg(test)]
mod tests {
    use std::{future::poll_fn, net::Shutdown, os::unix::net::UnixStream, task::Poll};

    use futures_util::{future::join, StreamExt};
    use ntest::timeout;
    use rand::{
        distr::{Alphanumeric, SampleString},
        rng,
    };
    use zbus::{connection, MatchRule};

    use super::*;
    use crate::{
        bus::{Bus, STOP_TIMEOUT},
        peer::SEND_TIMEOUT,
    };

    /// A client that sends a last signal and closes its end of the connection, once the bus took
    /// the last of its messages that it forwards itself, but before it hands the connection over,
    /// as zbus read the signal and the end of what the client sent by then, is handed over with
    /// the signal: the next instance passes the signal on, and then cleans up after the client.
    #[test]
    #[timeout(30000)]
    fn last_signal_of_a_client_that_closes_its_end_is_handed_over() {
        zruntime::block_on(async {
            let bus = Bus::for_address(Some(&address())).await.unwrap();
            let peers = bus.peers().clone();
            let (receiver, _) = connect(&bus, 1).await;
            let rule = MatchRule::builder()
                .msg_type(message::Type::Signal)
                .build()
                .unwrap();
            let mut signals = MessageStream::for_match_rule(rule, &receiver, None)
                .await
                .unwrap();
            let (sender, socket) = connect(&bus, 2).await;
            let sender_name = sender.unique_name().unwrap().to_string();

            peers.pause(STOP_TIMEOUT).await.unwrap();
            peers.settle().await.unwrap();
            let bye = Message::signal("/org/zbus/Test", "org.zbus.Test", "Bye")
                .build(&())
                .unwrap();
            sender.send(&bye).await.unwrap();
            socket.shutdown(Shutdown::Write).unwrap();
            let conn = peers
                .routing()
                .await
                .peers
                .get(sender_name.as_str())
                .map(|peer| peer.conn().clone())
                .unwrap();
            conn.closed().await;
            let (snapshot, clients) = peers.snapshot().await.unwrap();
            assert!(
                clients.iter().any(|client| client.id == 2),
                "the client that closed its end was left out"
            );
            peers.retire().await;
            drop(bus);

            let next = Bus::for_address(Some(&address())).await.unwrap();
            let clients = clients
                .into_iter()
                .map(|client| (client.id, client))
                .collect();
            next.peers()
                .restore(snapshot, clients, SEND_TIMEOUT)
                .await
                .unwrap();
            let mut passed_on = false;
            loop {
                let signal = signals.next().await.unwrap().unwrap();
                let header = signal.header();
                match header.member().map(|member| member.as_str()) {
                    Some("Bye") => passed_on = true,
                    Some("NameOwnerChanged") => {
                        let (name, _, new_owner): (String, String, String) =
                            signal.body().deserialize().unwrap();
                        if name == sender_name && new_owner.is_empty() {
                            break;
                        }
                    }
                    _ => (),
                }
            }
            assert!(
                passed_on,
                "the client was cleaned up after before its signal was passed on"
            );
        });
    }

    /// A client that sends a last signal and goes while the bus settles, and that a send of the
    /// bus fails to then, as it went, has its signal passed on all the same, though the bus is
    /// disconnecting it, and is cleaned up after: zbus hands over no connection that the bus
    /// closed, so the bus serves the client to the end of its stream, as any client that went.
    #[test]
    #[timeout(30000)]
    fn last_signal_of_a_client_that_a_send_fails_to_is_passed_on() {
        zruntime::block_on(async {
            let bus = Bus::for_address(Some(&address())).await.unwrap();
            let peers = bus.peers().clone();
            let rule = MatchRule::builder()
                .msg_type(message::Type::Signal)
                .build()
                .unwrap();
            let (receiver, _) = connect(&bus, 1).await;
            let mut signals = MessageStream::for_match_rule(rule.clone(), &receiver, None)
                .await
                .unwrap();
            // The bus forwards the signals to `x` too, so it sends `x` the signal of `y`.
            let (x, x_socket) = connect(&bus, 2).await;
            let _x_signals = MessageStream::for_match_rule(rule, &x, None).await.unwrap();
            let (y, y_socket) = connect(&bus, 3).await;
            let x_name = x.unique_name().unwrap().to_string();
            let y_name = y.unique_name().unwrap().to_string();

            peers.pause(STOP_TIMEOUT).await.unwrap();
            let bye = Message::signal("/org/zbus/Test", "org.zbus.Test", "Bye")
                .build(&())
                .unwrap();
            x.send(&bye).await.unwrap();
            y.send(&bye).await.unwrap();
            y_socket.shutdown(Shutdown::Both).unwrap();
            let y_conn = peers
                .routing()
                .await
                .peers
                .get(y_name.as_str())
                .map(|peer| peer.conn().clone())
                .unwrap();
            y_conn.closed().await;
            // `x` goes once the bus took the stream of `y` back, to serve it on, and before the
            // chore that serves it forwards the signal of `y` to `x`: the runtime runs one task at
            // a time, and `join` polls this each time that it is polled, so this runs as soon as
            // the bus waits for that chore, before the chore runs.
            let x_goes = poll_fn(|_| {
                let taken = peers.routing.try_write().is_some_and(|mut routing| {
                    routing
                        .peers
                        .get_mut(y_name.as_str())
                        .is_none_or(|y| y.paused().is_none())
                });
                if !taken {
                    return Poll::Pending;
                }
                x_socket.shutdown(Shutdown::Both).unwrap();

                Poll::Ready(())
            });
            let (settled, ()) = join(peers.settle(), x_goes).await;
            settled.unwrap();
            let (snapshot, clients) = peers.snapshot().await.unwrap();
            peers.retire().await;
            drop(bus);

            let next = Bus::for_address(Some(&address())).await.unwrap();
            let clients = clients
                .into_iter()
                .map(|client| (client.id, client))
                .collect();
            next.peers()
                .restore(snapshot, clients, SEND_TIMEOUT)
                .await
                .unwrap();
            let mut passed_on = Vec::new();
            let mut cleaned_up = Vec::new();
            while cleaned_up.len() < 2 {
                let signal = signals.next().await.unwrap().unwrap();
                let header = signal.header();
                match header.member().map(|member| member.as_str()) {
                    Some("Bye") => passed_on.push(header.sender().unwrap().to_string()),
                    Some("NameOwnerChanged") => {
                        let (name, _, new_owner): (String, String, String) =
                            signal.body().deserialize().unwrap();
                        if (name == x_name || name == y_name) && new_owner.is_empty() {
                            assert!(
                                passed_on.contains(&name),
                                "`{name}` was cleaned up after before its signal was passed on"
                            );
                            cleaned_up.push(name);
                        }
                    }
                    _ => (),
                }
            }
        });
    }

    /// A client of `bus`, numbered `id`, over a socket pair, and its end of the pair.
    async fn connect(bus: &Bus, id: usize) -> (Connection, UnixStream) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let socket = ours.try_clone().unwrap();
        let uid = rustix::process::getuid().as_raw();
        let server = connection::Builder::unix_stream(theirs);
        let adding = bus.peers().add(bus.guid(), id, server, SEND_TIMEOUT, uid);
        let connecting = connection::Builder::unix_stream(ours).build();
        let (added, conn) = join(adding, connecting).await;
        added.unwrap();

        (conn.unwrap(), socket)
    }

    /// The address of an abstract socket that no other bus listens on.
    fn address() -> String {
        format!(
            "unix:abstract={}",
            Alphanumeric.sample_string(&mut rng(), 10)
        )
    }
}
