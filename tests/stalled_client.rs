use std::{
    env::temp_dir,
    fs::File,
    future::Future,
    io::{self, Read, Write},
    net::Shutdown,
    num::NonZeroUsize,
    os::{
        fd::{BorrowedFd, OwnedFd},
        unix::net::UnixStream,
    },
    pin::pin,
    sync::{Arc, Once},
    time::Duration,
};

use anyhow::ensure;
use busd::{bus::Bus, peer::MAX_QUEUED_BYTES};
use futures_util::{
    future::{select, Either},
    stream::StreamExt,
};
use ntest::timeout;
use rand::{
    distr::{Alphanumeric, SampleString},
    rng,
};
use tracing::instrument;
use zbus::{
    connection::{
        self,
        socket::{ReadHalf, Socket, Split, WriteHalf},
    },
    export::async_trait::async_trait,
    fdo::{DBusProxy, RequestNameFlags},
    message,
    names::{UniqueName, WellKnownName},
    wire::Fd,
    Connection, MatchRule, MessageStream,
};
use zruntime::{AsyncIo, Event, Shared, SharedRuntime};

/// A client that stops reading its socket, while it is subscribed to signals, must hold up no one
/// but itself: the signals for it pile up in its queue, while the bus goes on forwarding all the
/// rest.
#[test]
#[instrument]
#[timeout(30000)]
fn stalled_client() {
    init_tracing();

    zruntime::block_on(with_bus(stalled_client_));
}

/// A client whose queue would hold more than [`MAX_QUEUED_BYTES`] gets disconnected.
#[test]
#[instrument]
#[timeout(30000)]
fn overflowing_client() {
    init_tracing();

    zruntime::block_on(with_bus(overflowing_client_));
}

/// A message that a client can't take, as it has file descriptors and the client didn't negotiate
/// passing them, doesn't hold up the messages after it.
#[test]
#[instrument]
#[timeout(30000)]
fn client_without_fds() {
    init_tracing();

    zruntime::block_on(with_bus(client_without_fds_));
}

/// A client that the bus can't write to anymore gets disconnected, even while the bus can still
/// read from it.
#[cfg(target_os = "linux")]
#[test]
#[instrument]
#[timeout(30000)]
fn unwritable_client() {
    init_tracing();

    zruntime::block_on(with_bus(unwritable_client_));
}

async fn stalled_client_(address: String) -> anyhow::Result<()> {
    let (_stalled, _signals) = stalled_subscriber(&address).await?;

    // Far more signals than the sockets between the bus and the stalled client can take.
    let flooder = connection::Builder::address(address.as_str())
        .build()
        .await?;
    let payload = vec![0u8; 4096];
    for _ in 0..512 {
        flood(&flooder, &payload).await?;
    }

    // The bus still answers everyone else, a client that connects now included.
    let client = connection::Builder::address(address.as_str())
        .build()
        .await?;
    let dbus = DBusProxy::new(&client).await?;
    let name = WellKnownName::try_from("org.zbus.Stalled")?;
    for _ in 0..10 {
        dbus.request_name(name.clone(), RequestNameFlags::empty())
            .await?;
        dbus.release_name(name.clone()).await?;
    }
    let newcomer = connection::Builder::address(address.as_str())
        .build()
        .await?;
    DBusProxy::new(&newcomer).await?.get_id().await?;

    Ok(())
}

async fn overflowing_client_(address: String) -> anyhow::Result<()> {
    let (stalled, _signals) = stalled_subscriber(&address).await?;
    let stalled_name = stalled.unique_name().unwrap().to_owned();
    let observer = connection::Builder::address(address.as_str())
        .build()
        .await?;
    let mut owner_changes = owner_changes(&observer, &stalled_name).await?;

    // Signals of 8 MiB, until the stalled client's queue would hold more than it may.
    let flooder = connection::Builder::address(address.as_str())
        .build()
        .await?;
    let payload = vec![0u8; 8 * 1024 * 1024];
    let mut flooded = 0;
    while flooded <= MAX_QUEUED_BYTES {
        flood(&flooder, &payload).await?;
        flooded += payload.len();
    }

    // The bus disconnects the stalled client, and says so.
    disconnected(&mut owner_changes, &stalled_name).await?;
    DBusProxy::new(&observer).await?.get_id().await?;

    Ok(())
}

async fn client_without_fds_(address: String) -> anyhow::Result<()> {
    let (client, _) = client_on_socket(&address).await?;
    let client_name = client.unique_name().unwrap().to_owned();

    // A signal with a file descriptor for the client, then a call to the bus, which returns once
    // the bus has queued the signal for the client.
    let sender = connection::Builder::address(address.as_str())
        .build()
        .await?;
    let file = File::open("/dev/null")?;
    sender
        .emit_signal(
            Some(client_name.as_str()),
            "/org/zbus/Fds",
            "org.zbus.Fds",
            "WithFd",
            &Fd::from(&file),
        )
        .await?;
    DBusProxy::new(&sender).await?.get_id().await?;

    // The reply to a call of the client comes after the signal in its queue, and still gets to it.
    DBusProxy::new(&client).await?.get_id().await?;

    Ok(())
}

#[cfg(target_os = "linux")]
async fn unwritable_client_(address: String) -> anyhow::Result<()> {
    let (client, socket) = client_on_socket(&address).await?;
    let client_name = client.unique_name().unwrap().to_owned();
    let observer = connection::Builder::address(address.as_str())
        .build()
        .await?;
    let mut owner_changes = owner_changes(&observer, &client_name).await?;

    // On Linux, writing to a socket fails once its peer shuts it down for reading.
    socket.0.get_ref().shutdown(Shutdown::Read)?;
    let sender = connection::Builder::address(address.as_str())
        .build()
        .await?;
    sender
        .emit_signal(
            Some(client_name.as_str()),
            "/org/zbus/Unwritable",
            "org.zbus.Unwritable",
            "Unwritable",
            &(),
        )
        .await?;

    // The bus disconnects the client, and says so.
    disconnected(&mut owner_changes, &client_name).await
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

/// A client that is subscribed to every signal, and reads none.
///
/// Its stream of signals holds one, and is never read, so once a signal is in it, the client
/// reads nothing more from its socket.
async fn stalled_subscriber(address: &str) -> anyhow::Result<(Connection, MessageStream)> {
    let conn = connection::Builder::address(address).build().await?;
    let rule = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .build()?;
    let signals = MessageStream::for_match_rule(rule, &conn, Some(NonZeroUsize::MIN)).await?;

    Ok((conn, signals))
}

/// A client on a socket that zbus doesn't negotiate passing file descriptors over.
async fn client_on_socket(address: &str) -> anyhow::Result<(Connection, ClientSocket)> {
    let path = address.strip_prefix("unix:path=").unwrap().to_owned();
    let stream = zruntime::unblock(move || UnixStream::connect(path)).await?;
    let socket = ClientSocket(Arc::new(AsyncIo::new(&SharedRuntime::current()?, stream)?));
    let conn = connection::Builder::socket(socket.clone())
        // A reply that never comes fails the call, rather than have the test time out.
        .method_timeout(Duration::from_secs(5))
        .build()
        .await?;

    Ok((conn, socket))
}

/// The `NameOwnerChanged` signals for `name`, as `conn` gets them.
async fn owner_changes(conn: &Connection, name: &UniqueName<'_>) -> anyhow::Result<MessageStream> {
    let rule = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .sender("org.freedesktop.DBus")
        .interface("org.freedesktop.DBus")
        .member("NameOwnerChanged")
        .arg(0, name.as_str())
        .build()?;

    Ok(MessageStream::for_match_rule(rule, conn, None).await?)
}

/// Wait for the bus to say that the client of the unique name `name` is gone.
async fn disconnected(
    owner_changes: &mut MessageStream,
    name: &UniqueName<'_>,
) -> anyhow::Result<()> {
    let msg = owner_changes.next().await.unwrap()?;
    let (changed, _, new_owner): (String, String, String) = msg.body().deserialize()?;
    ensure!(changed == name.as_str(), "unexpected name `{changed}`");
    ensure!(new_owner.is_empty(), "`{name}` went to `{new_owner}`");

    Ok(())
}

async fn flood(conn: &Connection, payload: &[u8]) -> zbus::Result<()> {
    conn.emit_signal(
        None::<&str>,
        "/org/zbus/Flood",
        "org.zbus.Flood",
        "Flood",
        &serde_bytes::Bytes::new(payload),
    )
    .await
}

/// A client's Unix socket, wrapped so that zbus doesn't see it as one.
///
/// So zbus doesn't negotiate passing file descriptors over it, and a test can reach the socket
/// under the client's connection.
#[derive(Debug, Clone)]
struct ClientSocket(Arc<AsyncIo<UnixStream, Shared>>);

impl Socket for ClientSocket {
    type ReadHalf = Self;
    type WriteHalf = Self;

    fn split(self) -> Split<Self, Self> {
        Split::new(self.clone(), self)
    }
}

#[async_trait]
impl ReadHalf for ClientSocket {
    async fn recvmsg(&mut self, buf: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
        let len = self.0.read_with(|mut stream| stream.read(buf)).await?;

        Ok((len, vec![]))
    }
}

#[async_trait]
impl WriteHalf for ClientSocket {
    async fn sendmsg(&mut self, buf: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<usize> {
        assert!(
            fds.is_empty(),
            "file descriptors to send over a socket that can't"
        );

        self.0.write_with(|mut stream| stream.write(buf)).await
    }

    async fn close(&mut self) -> io::Result<()> {
        self.0.get_ref().shutdown(Shutdown::Both)
    }
}
