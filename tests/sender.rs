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
use zbus::{connection, message, Connection, MatchRule, Message, MessageStream};
use zruntime::Event;

const PATH: &str = "/org/zbus/Sender";
const INTERFACE: &str = "org.zbus.Sender";

/// A message of a client that leaves its sender field out, as most clients do, gets to its
/// recipient with the unique name of the client for its sender, along with its body and its file
/// descriptors.
#[test]
#[instrument]
#[timeout(30000)]
fn sender_left_out() {
    init_tracing();

    zruntime::block_on(with_bus(sender_left_out_));
}

async fn sender_left_out_(address: String) -> anyhow::Result<()> {
    let recipient = connect(&address).await?;
    let rule = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .interface(INTERFACE)
        .build()?;
    let mut signals = MessageStream::for_match_rule(rule, &recipient, None).await?;
    let sender = connect(&address).await?;
    let unique_name = sender.unique_name().unwrap();

    let signal = Message::signal(PATH, INTERFACE, "Body").build(&(7u32, "seven"))?;
    ensure!(
        signal.header().sender().is_none(),
        "the signal has a sender"
    );
    sender.send(&signal).await?;
    #[cfg(unix)]
    {
        let stdout = std::io::stdout();
        let signal =
            Message::signal(PATH, INTERFACE, "Fd").build(&(zbus::Fd::from(&stdout), 7u8))?;
        sender.send(&signal).await?;
    }

    let msg = signals.try_next().await?.unwrap();
    ensure!(
        msg.header().sender() == Some(unique_name),
        "the signal got to its recipient from {:?}",
        msg.header().sender(),
    );
    let body: (u32, String) = msg.body().deserialize()?;
    ensure!(body == (7, "seven".to_string()), "the body is {body:?}");
    #[cfg(unix)]
    {
        let msg = signals.try_next().await?.unwrap();
        ensure!(
            msg.header().sender() == Some(unique_name),
            "the signal got to its recipient from {:?}",
            msg.header().sender(),
        );
        let fds = msg.data().fds().len();
        ensure!(fds == 1, "the signal brought {fds} file descriptors");
    }

    Ok(())
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
