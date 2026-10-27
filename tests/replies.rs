use std::{env::temp_dir, future::Future, num::NonZeroU32, pin::pin, sync::Once};

use anyhow::{ensure, Context};
use busd::bus::Bus;
use futures_util::{
    future::{join, select, Either},
    TryStreamExt,
};
use ntest::timeout;
use rand::{
    distr::{Alphanumeric, SampleString},
    rng,
};
use tracing::instrument;
use zbus::{
    connection,
    fdo::{self, DBusProxy, MonitoringProxy},
    message, Connection, MatchRule, Message, MessageStream,
};
use zruntime::Event;

const PATH: &str = "/org/zbus/Replies";
const INTERFACE: &str = "org.zbus.Replies";

/// A reply that a peer sends to a caller, for a call that the caller made of another peer, doesn't
/// get to the caller, and the reply of the callee does.
#[test]
#[instrument]
#[timeout(30000)]
fn forged_reply() {
    init_tracing();

    zruntime::block_on(with_bus(forged_reply_));
}

/// A call that waits on a peer that goes gets a `NoReply` error from the bus.
#[test]
#[instrument]
#[timeout(30000)]
fn callee_leaves() {
    init_tracing();

    zruntime::block_on(with_bus(callee_leaves_));
}

/// A call that waits on a peer that becomes a monitor gets a `NoReply` error from the bus.
#[test]
#[instrument]
#[timeout(30000)]
fn callee_becomes_monitor() {
    init_tracing();

    zruntime::block_on(with_bus(callee_becomes_monitor_));
}

/// A call with the serial of another of its caller that waits on the same callee is refused, and
/// the callee only answers the first. A call that expects no reply doesn't wait on its callee.
#[test]
#[instrument]
#[timeout(30000)]
fn same_serial() {
    init_tracing();

    zruntime::block_on(with_bus(same_serial_));
}

async fn forged_reply_(address: String) -> anyhow::Result<()> {
    let caller = connect(&address).await?;
    let (callee, mut calls) = callee(&address).await?;
    let forger = connect(&address).await?;

    let calling = caller.call_method(callee.unique_name(), PATH, Some(INTERFACE), "Call", &());
    let answering = async {
        let call = calls.try_next().await?.context("no call")?;
        // The forger knows the serial, as one that guesses it right does.
        let forged = Message::method_return(&call.header()).build(&"forged")?;
        forger.send(&forged).await?;
        // By the time that the bus answers the forger, it has passed the forged reply on, if it
        // does at all.
        DBusProxy::new(&forger).await?.get_id().await?;
        let reply = Message::method_return(&call.header()).build(&"genuine")?;
        callee.send(&reply).await?;

        anyhow::Ok(())
    };
    let (reply, answered) = join(calling, answering).await;
    answered?;
    let reply: String = reply?.body().deserialize()?;
    ensure!(reply == "genuine", "the caller got the `{reply}` reply");

    Ok(())
}

async fn callee_leaves_(address: String) -> anyhow::Result<()> {
    let caller = connect(&address).await?;
    let (callee, mut calls) = callee(&address).await?;

    let calling = caller.call_method(callee.unique_name(), PATH, Some(INTERFACE), "Call", &());
    let leaving = async {
        calls.try_next().await?.context("no call")?;
        drop(calls);
        callee.clone().close().await?;

        anyhow::Ok(())
    };
    let (reply, left) = join(calling, leaving).await;
    left?;
    ensure!(
        matches!(reply, Err(zbus::Error::FDO(ref e)) if matches!(**e, fdo::Error::NoReply(_))),
        "the caller got {reply:?}"
    );

    Ok(())
}

async fn callee_becomes_monitor_(address: String) -> anyhow::Result<()> {
    let caller = connect(&address).await?;
    let (callee, mut calls) = callee(&address).await?;

    let calling = caller.call_method(callee.unique_name(), PATH, Some(INTERFACE), "Call", &());
    let becoming = async {
        calls.try_next().await?.context("no call")?;
        MonitoringProxy::new(&callee)
            .await?
            .become_monitor(&[], 0)
            .await?;

        anyhow::Ok(())
    };
    let (reply, became) = join(calling, becoming).await;
    became?;
    ensure!(
        matches!(reply, Err(zbus::Error::FDO(ref e)) if matches!(**e, fdo::Error::NoReply(_))),
        "the caller got {reply:?}"
    );

    Ok(())
}

async fn same_serial_(address: String) -> anyhow::Result<()> {
    let caller = connect(&address).await?;
    let mut replies = MessageStream::from(&caller);
    let (callee, mut calls) = callee(&address).await?;
    let callee_name = callee.unique_name().unwrap();
    let serial = NonZeroU32::new(7).unwrap();
    let call = |member| {
        Message::method_call(PATH, member)
            .destination(callee_name)
            .interface(INTERFACE)
            .serial(serial)
    };

    // Two calls that expect no reply, then two that do, all with the same serial.
    for member in ["First", "Second"] {
        let msg = call(member)
            .with_flags(message::Flags::NO_REPLY_EXPECTED)
            .build(&())?;
        caller.send(&msg).await?;
    }
    for member in ["Third", "Fourth"] {
        caller.send(&call(member).build(&())?).await?;
    }

    // The callee gets all but the last, which the bus refuses.
    for member in ["First", "Second", "Third"] {
        let msg = calls.try_next().await?.context("no call")?;
        let header = msg.header();
        let got = header.member().unwrap();
        ensure!(
            got == member,
            "the callee got `{got}` rather than `{member}`"
        );
    }
    let refused = next_reply(&mut replies).await?;
    let header = refused.header();
    ensure!(
        header.message_type() == message::Type::Error
            && header.error_name().unwrap() == "org.freedesktop.DBus.Error.AccessDenied"
            && header.sender().unwrap() == busd::fdo::BUS_NAME
            && header.reply_serial() == Some(serial),
        "the caller got {refused:?}"
    );

    // The callee answers the third, which the caller gets, and that one only.
    let third = Message::method_call(PATH, "Third")
        .sender(caller.unique_name().unwrap())
        .serial(serial)
        .build(&())?;
    for answer in ["first", "second"] {
        let reply = Message::method_return(&third.header()).build(&answer)?;
        callee.send(&reply).await?;
    }
    let reply = next_reply(&mut replies).await?;
    let answer: String = reply.body().deserialize()?;
    ensure!(answer == "first", "the caller got the `{answer}` reply");
    // By the time that the bus answers the callee, it has passed the second reply on, if it does at
    // all, and the caller gets the reply of the bus to its own call after it.
    DBusProxy::new(&callee).await?.get_id().await?;
    DBusProxy::new(&caller).await?.get_id().await?;
    let reply = next_reply(&mut replies).await?;
    let header = reply.header();
    ensure!(
        header.sender().unwrap() == busd::fdo::BUS_NAME,
        "the caller got {reply:?}"
    );

    Ok(())
}

/// A connection that gets the calls of the tests, which it doesn't answer of its own accord.
async fn callee(address: &str) -> anyhow::Result<(Connection, MessageStream)> {
    let callee = connect(address).await?;
    let rule = MatchRule::builder()
        .msg_type(message::Type::MethodCall)
        .interface(INTERFACE)
        .build()?;
    let calls = MessageStream::for_match_rule(rule, &callee, None).await?;

    Ok((callee, calls))
}

/// The next reply, or error, in `msgs`.
async fn next_reply(msgs: &mut MessageStream) -> anyhow::Result<Message> {
    loop {
        let msg = msgs.try_next().await?.context("no reply")?;
        if matches!(
            msg.message_type(),
            message::Type::MethodReturn | message::Type::Error
        ) {
            return Ok(msg);
        }
    }
}

async fn connect(address: &str) -> zbus::Result<Connection> {
    connection::Builder::address(address).build().await
}

/// Set the tracing subscriber up, once for all the tests of this file, which run in one process.
fn init_tracing() {
    static INIT: Once = Once::new();
    INIT.call_once(busd::tracing_subscriber::init);
}

/// Run `test` against a bus of its own, at an address that it is given.
async fn with_bus<F, T>(test: F)
where
    F: FnOnce(String) -> T,
    T: Future<Output = anyhow::Result<()>>,
{
    let s = Alphanumeric.sample_string(&mut rng(), 10);
    let path = temp_dir().join(s);
    let address = format!("unix:path={}", path.display());
    let mut bus = Bus::for_address(Some(&address)).await.unwrap();
    let stop = Event::new();
    let stopped = stop.listen();

    let handle = zruntime::spawn(async move {
        {
            let running = pin!(bus.run());
            match select(stopped, running).await {
                Either::Left(_) => (),
                Either::Right((Ok(()), _)) => panic!("Bus exited unexpectedly"),
                Either::Right((Err(e), _)) => panic!("Bus exited with an error: {e}"),
            }
        }

        bus
    });

    let ret = test(address).await;
    stop.notify(usize::MAX);
    let bus = handle.await.unwrap();
    bus.cleanup().await.unwrap();
    ret.unwrap();
}
