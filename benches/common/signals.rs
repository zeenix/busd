//! The signals of the benchmarks: broadcast by an emitter, without a destination, and delivered by
//! the bus to the subscribers whose match rule they match.
//!
//! Every signal carries its sequence number and a payload of bytes. A subscriber checks that the
//! numbers arrive in order, one after the other, so a benchmark fails if the bus loses, repeats or
//! reorders a signal.

use std::{num::NonZeroUsize, ops::Range};

use futures_util::TryStreamExt;
use serde_bytes::Bytes;
use zbus::{connection, message::Type, Connection, MatchRule, Message, MessageStream};

use crate::calls::{Result, PATH};

/// The interface of the signals.
pub const INTERFACE: &str = "org.dbus2.benchmark.Fanout";

/// How many signals more than a subscriber is to receive at a time the queue of its stream has room
/// for.
///
/// zbus holds back the reader of a connection's socket whenever the stream of one of its match
/// rules is full. A queue for every signal that a subscriber is to receive before it reads the
/// stream again, and this many to spare, rules that out, so that a subscriber reads its socket as
/// fast as the bus fills it, whatever the order in which the tasks of its runtime happen to run.
pub const QUEUE_SPARE: usize = 16;

/// The match rule of the signals of `member`.
pub fn rule(member: &str) -> Result<MatchRule<'static>> {
    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .interface(INTERFACE)
        .member(member.to_owned())
        .build()?;

    Ok(rule)
}

/// Connects a subscriber to the bus at `address`, and subscribes it to the signals that `rule`
/// matches, with room in the queue of its stream for `queue` of them.
///
/// The bus has registered the match rule by the time that this returns, as it waits for the bus to
/// reply to `AddMatch`.
pub async fn subscribe(
    address: &str,
    rule: MatchRule<'static>,
    queue: NonZeroUsize,
) -> Result<MessageStream> {
    let connection = connection::Builder::address(address).build().await?;

    Ok(MessageStream::for_match_rule(rule, &connection, Some(queue)).await?)
}

/// Broadcasts the signals of `member` with the sequence numbers of `seqs`, each with `data` as
/// payload.
///
/// The payload is borrowed by every signal, rather than copied into it first. The signals have no
/// sender, for the bus to set.
pub async fn emit(
    connection: &Connection,
    member: &str,
    seqs: Range<u64>,
    data: &[u8],
) -> Result<()> {
    let payload = Bytes::new(data);
    for seq in seqs {
        let msg = Message::signal(PATH, INTERFACE, member).build(&(seq, payload))?;
        connection.send(&msg).await?;
    }

    Ok(())
}

/// Receives the signals with the sequence numbers of `seqs` from `stream`, and checks that they
/// arrive in order, each with `payload_len` bytes of payload.
pub async fn receive(
    stream: &mut MessageStream,
    seqs: Range<u64>,
    payload_len: usize,
) -> Result<()> {
    for expected in seqs {
        let Some(msg) = stream.try_next().await? else {
            return Err(format!("the stream ended where signal {expected} was due").into());
        };
        let body = msg.body();
        let (seq, payload): (u64, &Bytes) = body.deserialize()?;

        if seq != expected {
            return Err(format!("signal {seq} arrived where signal {expected} was due").into());
        }
        if payload.len() != payload_len {
            let len = payload.len();
            return Err(format!("signal {seq} carried {len} bytes, not {payload_len}").into());
        }
    }

    Ok(())
}
