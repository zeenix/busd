use std::{env::temp_dir, pin::pin};

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
    fdo::{DBusProxy, MonitoringProxy},
    message,
    proxy::CacheProperties,
    MatchRule, MessageStream,
};
use zruntime::Event;

/// Broadcast signals, which have no destination, reach the match rules that they match, and
/// leave the bus running.
#[test]
#[instrument]
#[timeout(30000)]
fn broadcast_signals() {
    busd::tracing_subscriber::init();

    zruntime::block_on(async {
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

        let ret = broadcast_signals_(&address).await;
        stop.notify(usize::MAX);
        let bus = handle.await.unwrap();
        bus.cleanup().await.unwrap();
        ret.unwrap();
    });
}

#[instrument]
async fn broadcast_signals_(address: &str) -> anyhow::Result<()> {
    // A monitor of signals alone, which has a match rule, unlike a monitor of every message.
    let monitor = connection::Builder::address(address).build().await?;
    let mut monitored = MessageStream::from(&monitor);
    let signals = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .build()?;
    MonitoringProxy::builder(&monitor)
        .cache_properties(CacheProperties::No)
        .build()
        .await?
        .become_monitor(&[signals], 0)
        .await?;

    let emitter = connection::Builder::address(address).build().await?;
    emitter
        .emit_signal(
            None::<&str>,
            "/org/zbus/Broadcast",
            "org.zbus.Broadcast",
            "Everyone",
            &(),
        )
        .await?;

    // The monitor gets the broadcast signal, among others.
    loop {
        let msg = monitored.try_next().await?.unwrap();
        let header = msg.header();
        if header
            .member()
            .is_some_and(|member| member.as_str() == "Everyone")
        {
            break;
        }
    }

    // And the bus still answers.
    DBusProxy::new(&emitter).await?.get_id().await?;

    Ok(())
}
