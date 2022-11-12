use std::{env::temp_dir, iter::repeat_with};

use dbuz::bus::Bus;
use ntest::timeout;
use tokio::{
    select,
    sync::mpsc::{channel, Sender},
};
use tracing::instrument;
use tracing_subscriber::{util::SubscriberInitExt, EnvFilter, FmtSubscriber};
use zbus::{dbus_interface, dbus_proxy, CacheProperties, Connection, ConnectionBuilder};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[instrument]
#[timeout(15000)]
async fn greet() {
    FmtSubscriber::builder()
        .with_env_filter(EnvFilter::from_default_env())
        .finish()
        .init();

    let dir = temp_dir().join("dbuz-test");
    let res = tokio::fs::create_dir(&dir).await;
    if let Err(e) = &res {
        // It's fine if it already exists.
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            res.unwrap();
        }
    }
    let s: String = repeat_with(fastrand::alphanumeric).take(10).collect();
    let path = dir.join(s);

    let mut bus = Bus::new(Some(&*path)).await.unwrap();
    let (tx, mut rx) = channel(10);
    let socket_addr = format!("unix:path={}", path.display());
    let client_service_ready = async move {
        rx.recv().await.unwrap();
        rx.recv().await.unwrap();
    };

    let handle = tokio::spawn(async move {
        select! {
            _ = client_service_ready => (),
            _ = bus.run() => {
                panic!("Bus stopped unexpectedly");
            }
        }

        bus
    });

    let ret = match greet_service(&socket_addr, tx.clone()).await {
        Ok(service_conn) => greet_client(&socket_addr, tx).await.map(|_| service_conn),
        Err(e) => Err(e),
    };
    let bus = handle.await.unwrap();
    bus.cleanup().await.unwrap();
    ret.unwrap();
}

#[instrument]
async fn greet_service(socket_addr: &str, tx: Sender<()>) -> anyhow::Result<Connection> {
    struct Greeter {
        count: u64,
        tx: Sender<()>,
    }

    #[dbus_interface(name = "org.zbus.MyGreeter1")]
    impl Greeter {
        async fn say_hello(&mut self, name: &str) -> String {
            self.count += 1;
            self.tx.send(()).await.unwrap();
            format!("Hello {}! I have been called {} times.", name, self.count)
        }
    }

    let greeter = Greeter { count: 0, tx };
    ConnectionBuilder::address(socket_addr)?
        .name("org.zbus.MyGreeter")?
        .serve_at("/org/zbus/MyGreeter", greeter)?
        .build()
        .await
        .map_err(Into::into)
}

#[instrument]
async fn greet_client(socket_addr: &str, tx: Sender<()>) -> anyhow::Result<()> {
    #[dbus_proxy(
        interface = "org.zbus.MyGreeter1",
        default_path = "/org/zbus/MyGreeter"
    )]
    trait MyGreeter {
        fn say_hello(&self, name: &str) -> zbus::Result<String>;
    }

    let conn = ConnectionBuilder::address(socket_addr)?.build().await?;

    let proxy = MyGreeterProxy::builder(&conn)
        .destination("org.zbus.MyGreeter")?
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let reply = proxy.say_hello("Maria").await?;
    assert_eq!(reply, "Hello Maria! I have been called 1 times.");

    tx.send(()).await.unwrap();

    Ok(())
}
