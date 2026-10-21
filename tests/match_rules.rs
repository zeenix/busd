use std::{env::temp_dir, future::Future, pin::pin, sync::Once};

use anyhow::ensure;
use busd::bus::Bus;
use futures_util::{
    future::{select, Either},
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
    fdo::{DBusProxy, RequestNameFlags},
    message, MatchRule, MessageStream,
};
use zruntime::Event;

const NAME: &str = "org.zbus.MatchRules";
const PATH: &str = "/org/zbus/MatchRules";
const INTERFACE: &str = "org.zbus.MatchRules";

/// A signal sent to a well-known name goes to the owner of the name, whatever its match rules.
#[test]
#[instrument]
#[timeout(30000)]
fn signals_to_well_known_names() {
    init_tracing();

    zruntime::block_on(with_bus(signals_to_well_known_names_));
}

async fn signals_to_well_known_names_(address: String) -> anyhow::Result<()> {
    // The owner of a name, with no match rule for the signals of the test.
    let owner = connection::Builder::address(address.as_str())
        .build()
        .await?;
    let owner_name = owner.unique_name().unwrap().to_owned();
    DBusProxy::new(&owner)
        .await?
        .request_name(NAME.try_into()?, RequestNameFlags::empty())
        .await?;
    let mut received = MessageStream::from(&owner);

    // A signal sent to the name, then one sent to the unique name of its owner.
    let emitter = connection::Builder::address(address.as_str())
        .build()
        .await?;
    for (destination, member) in [
        (NAME, "ToWellKnownName"),
        (owner_name.as_str(), "ToUniqueName"),
    ] {
        emitter
            .emit_signal(Some(destination), PATH, INTERFACE, member, &())
            .await?;
    }

    // The owner gets both, in that order.
    let mut members = vec![];
    while members.last().map(String::as_str) != Some("ToUniqueName") {
        let msg = received.try_next().await?.unwrap();
        let header = msg.header();
        if header.interface().is_some_and(|i| i.as_str() == INTERFACE) {
            members.push(header.member().unwrap().to_string());
        }
    }
    ensure!(
        members == ["ToWellKnownName", "ToUniqueName"],
        "the owner got {members:?}"
    );

    Ok(())
}

/// A signal with a destination goes to no one else, whatever their match rules.
#[test]
#[instrument]
#[timeout(30000)]
fn signals_to_others() {
    init_tracing();

    zruntime::block_on(with_bus(signals_to_others_));
}

async fn signals_to_others_(address: String) -> anyhow::Result<()> {
    // A client with a match rule that matches every signal of the test.
    let snooper = connection::Builder::address(address.as_str())
        .build()
        .await?;
    let rule = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .interface(INTERFACE)
        .build()?;
    let mut snooped = MessageStream::for_match_rule(rule, &snooper, None).await?;

    // And one that owns a name.
    let owner = connection::Builder::address(address.as_str())
        .build()
        .await?;
    let owner_name = owner.unique_name().unwrap().to_owned();
    DBusProxy::new(&owner)
        .await?
        .request_name(NAME.try_into()?, RequestNameFlags::empty())
        .await?;

    // A signal sent to the name, one sent to the unique name of its owner, and a broadcast one.
    let emitter = connection::Builder::address(address.as_str())
        .build()
        .await?;
    for (destination, member) in [
        (Some(NAME), "ToWellKnownName"),
        (Some(owner_name.as_str()), "ToUniqueName"),
        (None, "Broadcast"),
    ] {
        emitter
            .emit_signal(destination, PATH, INTERFACE, member, &())
            .await?;
    }

    // The snooper only gets the broadcast signal, which came last.
    let msg = snooped.try_next().await?.unwrap();
    let header = msg.header();
    let member = header.member().unwrap();
    ensure!(member == "Broadcast", "the snooper got `{member}` first");

    Ok(())
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
