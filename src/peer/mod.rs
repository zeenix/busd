mod stream;
pub use stream::*;
use zruntime::{Event, EventListener};
mod monitor;
pub use monitor::*;
mod outgoing;
pub use outgoing::*;

use crate::trace;
use anyhow::Result;
use zbus::{
    connection::{self, AuthMechanism},
    names::{BusName, OwnedUniqueName},
    Connection, MessageStream, OwnedGuid, OwnedMatchRule,
};

use crate::{fdo, match_rules::MatchRules, name_registry::NameRegistry};

/// A peer connection.
#[derive(Debug)]
pub struct Peer {
    conn: Connection,
    recipient: Recipient,
    unique_name: OwnedUniqueName,
    match_rules: MatchRules,
    greeted: bool,
    canceled_event: Event,
}

impl Peer {
    pub async fn new(
        guid: OwnedGuid,
        id: usize,
        builder: connection::Builder<'_>,
        auth_mechanism: AuthMechanism,
    ) -> Result<(Self, Stream)> {
        let unique_name = OwnedUniqueName::try_from(format!(":busd.{id}")).unwrap();
        // Use `build_message_stream` to activate a message receiver before the socket reader
        // task is spawned, so that messages the client pipelines right after authentication
        // (notably `Hello`) are never dropped in the race window between `build()` and stream
        // creation. See https://github.com/z-galaxy/zbus/pull/1760.
        let msg_stream = builder
            .server(guid)
            .p2p()
            .auth_mechanism(auth_mechanism)
            .build_message_stream()
            .await?;
        let conn = Connection::from(&msg_stream);
        trace!("created: {:?}", conn);

        let peer = Self {
            recipient: Recipient::Client(Outgoing::new(conn.clone(), unique_name.clone())),
            conn,
            unique_name,
            match_rules: MatchRules::default(),
            greeted: false,
            canceled_event: Event::new(),
        };
        let stream = Stream::for_peer(&peer, msg_stream);
        Ok((peer, stream))
    }

    // This the the bus itself, serving the FDO D-Bus API.
    pub async fn new_us(msg_stream: MessageStream) -> (Self, Stream) {
        let unique_name = OwnedUniqueName::try_from(fdo::BUS_NAME).unwrap();
        let conn = Connection::from(&msg_stream);

        let peer = Self {
            recipient: Recipient::Bus(conn.clone()),
            conn,
            unique_name,
            match_rules: MatchRules::default(),
            greeted: true,
            canceled_event: Event::new(),
        };
        let stream = Stream::for_peer(&peer, msg_stream);
        (peer, stream)
    }

    pub fn unique_name(&self) -> &OwnedUniqueName {
        &self.unique_name
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Where messages for the peer go.
    pub fn recipient(&self) -> &Recipient {
        &self.recipient
    }

    pub fn listen_cancellation(&self) -> EventListener {
        self.canceled_event.listen()
    }

    /// Whether the message goes to this peer.
    ///
    /// A message with a destination goes to its destination alone, whatever the match rules of
    /// the peers. One without goes to the peers with a match rule that matches it.
    ///
    /// # Panics
    ///
    /// Same as [`MatchRules::matches`].
    pub fn interested(&self, msg: &zbus::Message, name_registry: &NameRegistry) -> bool {
        match msg.header().destination() {
            Some(BusName::Unique(name)) => *name == *self.unique_name,
            Some(BusName::WellKnown(name)) => name_registry
                .lookup(name.clone())
                .is_some_and(|owner| owner == self.unique_name),
            None => self.match_rules.matches(msg, name_registry),
        }
    }

    pub fn add_match_rule(&mut self, rule: OwnedMatchRule) {
        self.match_rules.add(rule);
    }

    /// Remove the first rule that matches.
    pub fn remove_match_rule(&mut self, rule: OwnedMatchRule) -> zbus::fdo::Result<()> {
        self.match_rules.remove(rule)
    }

    /// This can only be called once.
    pub async fn hello(&mut self) -> zbus::fdo::Result<()> {
        if self.greeted {
            return Err(zbus::fdo::Error::Failed(
                "Can only call `Hello` method once".to_string(),
            ));
        }
        self.greeted = true;

        Result::Ok(())
    }

    pub fn become_monitor(self, match_rules: MatchRules) -> Monitor {
        Monitor::new(self, match_rules)
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.canceled_event.notify(usize::MAX);
    }
}
