// The test here is of a limit of Linux, so it runs on Linux only.
#![cfg(target_os = "linux")]

use std::{
    env::temp_dir,
    fs::File,
    future::Future,
    io::{self, IoSlice, Read, Write},
    mem::MaybeUninit,
    os::{fd::AsFd, unix::net::UnixStream},
    pin::pin,
    sync::Once,
    time::Duration,
};

use anyhow::{ensure, Context};
use busd::{bus::Bus, peer::MAX_MESSAGE_FDS};
use futures_util::{
    future::{select, Either},
    stream::StreamExt,
};
use ntest::timeout;
use rand::{
    distr::{Alphanumeric, SampleString},
    rng,
};
use rustix::net::{sendmsg, SendAncillaryBuffer, SendAncillaryMessage, SendFlags};
use tracing::instrument;
use zbus::{
    connection, fdo::DBusProxy, message, names::UniqueName, wire::Fd, MatchRule, Message,
    MessageStream,
};
use zruntime::Event;

/// A client that sends a message with more file descriptors than the bus can pass on gets
/// disconnected, rather than the recipient of the message.
///
/// The limit is Linux's, so the test runs on Linux only.
#[test]
#[instrument]
#[timeout(30000)]
fn too_many_fds() {
    init_tracing();

    zruntime::block_on(with_bus(too_many_fds_));
}

async fn too_many_fds_(address: String) -> anyhow::Result<()> {
    let recipient = connection::Builder::address(address.as_str())
        .build()
        .await?;
    let recipient_name = recipient.unique_name().unwrap().to_owned();
    let rule = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .interface("org.zbus.Fds")
        .member("WithFds")
        .build()?;
    let mut signals = MessageStream::for_match_rule(rule, &recipient, None).await?;
    let path = address.strip_prefix("unix:path=").unwrap().to_owned();
    let sender = zruntime::unblock(move || RawClient::connect(&path)).await?;
    let files = (0..=MAX_MESSAGE_FDS)
        .map(|_| File::open("/dev/null"))
        .collect::<io::Result<Vec<_>>>()?;

    // A message with as many file descriptors as the bus can pass on gets to the recipient.
    let signal = with_fds(&recipient_name, &files[..MAX_MESSAGE_FDS])?;
    let sender = zruntime::unblock(move || sender.send(&signal).map(|()| sender)).await?;
    let msg = signals.next().await.unwrap()?;
    let fds = msg.data().fds().len();
    ensure!(
        fds == MAX_MESSAGE_FDS,
        "the signal came with {fds} file descriptors"
    );

    // With one more, the bus disconnects the sender, and the recipient stays.
    let signal = with_fds(&recipient_name, &files)?;
    zruntime::unblock(move || sender.send(&signal).and_then(|()| sender.disconnected()))
        .await
        .context("the bus didn't disconnect the sender")?;
    DBusProxy::new(&recipient).await?.get_id().await?;

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

/// A signal for `destination`, with a file descriptor of each of `files`.
fn with_fds(destination: &UniqueName<'_>, files: &[File]) -> zbus::Result<Message> {
    let fds: Vec<_> = files.iter().map(Fd::from).collect();

    Message::signal("/org/zbus/Fds", "org.zbus.Fds", "WithFds")
        .destination(destination)
        .build(&fds)
}

/// A client on a socket of its own, which passes the file descriptors of a message over as many
/// writes as Linux needs, where zbus passes them all with the first write.
struct RawClient(UnixStream);

impl RawClient {
    /// Connect to the bus on `path`, negotiate passing file descriptors, and say hello.
    fn connect(path: &str) -> anyhow::Result<Self> {
        let mut stream = UnixStream::connect(path)?;
        let uid = rustix::process::getuid().as_raw().to_string();
        let uid: String = uid.bytes().map(|b| format!("{b:02x}")).collect();
        stream.write_all(format!("\0AUTH EXTERNAL {uid}\r\n").as_bytes())?;
        read_line(&stream, "OK ")?;
        stream.write_all(b"NEGOTIATE_UNIX_FD\r\n")?;
        read_line(&stream, "AGREE_UNIX_FD")?;
        stream.write_all(b"BEGIN\r\n")?;
        let hello = Message::method_call("/org/freedesktop/DBus", "Hello")
            .destination("org.freedesktop.DBus")
            .interface("org.freedesktop.DBus")
            .build(&())?;
        stream.write_all(hello.data())?;
        // Wait for the reply to start, as clients do, since zbus takes the file descriptors that
        // it reads along with the handshake for those of the first message after it.
        stream.read_exact(&mut [0])?;

        Ok(Self(stream))
    }

    /// Send `msg`, with at most [`MAX_MESSAGE_FDS`] of its file descriptors per write.
    fn send(&self, msg: &Message) -> io::Result<()> {
        let data = msg.data();
        let fds: Vec<_> = data.fds().iter().map(AsFd::as_fd).collect();
        let chunks: Vec<_> = fds.chunks(MAX_MESSAGE_FDS).collect();
        let mut sent = 0;
        for (i, chunk) in chunks.iter().enumerate() {
            // A byte goes with each write of file descriptors but the last, which takes the rest.
            let end = if i + 1 < chunks.len() {
                sent + 1
            } else {
                data.len()
            };
            let mut space =
                [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_MESSAGE_FDS))];
            let mut control = SendAncillaryBuffer::new(&mut space);
            control.push(SendAncillaryMessage::ScmRights(chunk));
            let iov = [IoSlice::new(&data[sent..end])];
            sent += sendmsg(&self.0, &iov, &mut control, SendFlags::empty())?;
        }

        (&self.0).write_all(&data[sent..])
    }

    /// Wait for the bus to disconnect the client, reading what it sends until then.
    fn disconnected(&self) -> io::Result<()> {
        self.0.set_read_timeout(Some(Duration::from_secs(10)))?;

        io::copy(&mut &self.0, &mut io::sink()).map(|_| ())
    }
}

/// Read a line of the handshake, which must start with `expected`.
fn read_line(mut stream: &UnixStream, expected: &str) -> anyhow::Result<()> {
    let mut line = vec![];
    let mut byte = [0];
    while !line.ends_with(b"\r\n") {
        stream.read_exact(&mut byte)?;
        line.push(byte[0]);
    }
    let line = String::from_utf8_lossy(&line);
    ensure!(
        line.starts_with(expected),
        "unexpected `{}`",
        line.trim_end()
    );

    Ok(())
}
