//! What systemd passes busd as it starts it: the socket of its socket activation, and what the
//! previous instance of the bus handed over through the file descriptor store on a restart.

use std::{
    collections::HashMap,
    os::fd::{AsFd, OwnedFd},
    path::Path,
    str::FromStr,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use busd::{
    bus::{Bus, ClientHandover, Handover},
    debug, error, info,
    systemd::{self, Notifier},
    warn,
};
use rustix::{
    fs::fstat,
    pipe::{pipe_with, PipeFlags},
};
use zbus::{
    address::{transport::UnixSocket, Transport},
    Address,
};
use zruntime::SharedRuntime;

/// The file descriptors that systemd passed busd, sorted out.
#[derive(Debug, Default)]
pub struct Passed {
    /// The socket that systemd's socket activation passed, to listen on.
    listener: Option<OwnedFd>,
    /// What the previous instance of the bus handed over, if it did.
    handover: Option<Handover>,
    /// Whether a socket that a previous instance of the bus listened on, and stored, was dropped,
    /// as it came without the state of the bus.
    dropped_listener: bool,
    /// The names of the file descriptors that came out of the file descriptor store, which busd
    /// removes from there: nothing stays stored while busd runs.
    stored: Vec<String>,
}

impl Passed {
    /// Sort out the file descriptors that systemd passed, with their names.
    ///
    /// Those that busd stored have names of busd's own. The others are the sockets of socket
    /// activation, which have the names of their units by default, such as `busd.socket`, and of
    /// which there is one at most. A file descriptor of a client under a name that busd doesn't
    /// store any under, as one that a version of busd that named them otherwise stored, is of no
    /// use, and is removed from the store too.
    pub fn new(fds: Vec<(String, OwnedFd)>) -> Result<Self> {
        let mut passed = Self::default();
        let mut state = None;
        let mut stored_listener = None;
        let mut client_fds = HashMap::<_, Vec<_>>::new();
        let mut duplicates = Vec::new();
        for (name, fd) in fds {
            if name == STATE {
                match systemd::read_memfd(fd.as_fd()) {
                    Ok(bytes) => state = Some(bytes),
                    Err(e) => warn!("Failed to read the state of the bus that was stored: {e}"),
                }
            } else if name == LISTENER {
                stored_listener = Some(fd);
            } else if let Some(client) = name.strip_prefix(CLIENT) {
                match client_fd(client) {
                    Some(((id, n), None)) => client_fds.entry(id).or_default().push((n, fd)),
                    // What was stored for a duplicate only holds its place.
                    Some((place, Some(original))) => duplicates.push((place, original)),
                    None => warn!("Removing `{name}`, which busd doesn't know, from the store"),
                }
            } else if passed.listener.is_none() {
                passed.listener = Some(fd);

                continue;
            } else {
                bail!("systemd passed more than one socket to listen on: busd listens on one.");
            }
            passed.stored.push(name);
        }
        for ((id, n), (original_id, original_n)) in duplicates {
            let original = client_fds
                .get(&original_id)
                .and_then(|fds| fds.iter().find(|(m, _)| *m == original_n))
                .map(|(_, fd)| fd.try_clone());
            match original {
                Some(Ok(fd)) => client_fds.entry(id).or_default().push((n, fd)),
                Some(Err(e)) => warn!(
                    "Failed to duplicate `{CLIENT}{original_id}-{original_n}` for the client \
                     numbered {id}: {e}"
                ),
                // The client lacks it, as one whose file descriptor is missing.
                None => (),
            }
        }
        // Each client gets the file descriptors of its connection back in the order that they were
        // stored in, its socket first. Those of a client whose socket is missing close.
        let clients: Vec<_> = client_fds
            .into_iter()
            .filter_map(|(id, mut fds)| {
                fds.sort_unstable_by_key(|(n, _)| *n);
                if fds.first().is_none_or(|(n, _)| *n != 0) {
                    return None;
                }

                Some(ClientHandover {
                    id,
                    fds: fds.into_iter().map(|(_, fd)| fd).collect(),
                })
            })
            .collect();
        match state {
            Some(state) => {
                passed.handover = Some(Handover {
                    state,
                    listener: stored_listener,
                    clients,
                });
            }
            None if !clients.is_empty() || stored_listener.is_some() => {
                warn!("Dropping the sockets that were stored without the state of the bus");
                passed.dropped_listener = stored_listener.is_some();
            }
            None => (),
        }

        Ok(passed)
    }

    /// Create the bus out of what systemd passed: the next instance of the bus that handed over,
    /// if one did, on the socket of the socket activation, if any.
    ///
    /// Without either, `None`, for busd to create the bus as it would without systemd. Taking over
    /// may fail, as when the previous instance of busd encoded its state otherwise: the clients
    /// that were handed over get disconnected then, and the bus starts as if nothing was handed
    /// over. busd removes everything from the store before anything else, so that whatever comes
    /// of it, the instances that come after don't find it again.
    ///
    /// The next instance listens where the previous one did, whatever `address`, the one that busd
    /// is to listen at, says: it warns if they differ, unless it listens on the socket of the
    /// socket activation, which it does whatever `address` says.
    pub async fn bus(
        self,
        notifier: Option<&Notifier>,
        address: Option<&str>,
    ) -> Result<Option<Bus>> {
        let Self {
            mut listener,
            handover,
            dropped_listener,
            mut stored,
        } = self;
        if let Some(notifier) = notifier {
            stored.sort_unstable();
            stored.dedup();
            for name in stored {
                if let Err(e) = notifier.remove(&name) {
                    warn!("Failed to remove `{name}` from the store: {e}");
                }
            }
        }
        // Whether the bus binds a socket in place of the one that a previous instance listened on.
        let mut rebinds = dropped_listener;
        if let Some(handover) = handover {
            let clients = handover.clients.len();
            let stored_listener = handover.listener.is_some();
            let activated = listener.is_some();
            let fallback = listener.as_ref().map(OwnedFd::try_clone).transpose()?;
            match Bus::take_over(handover, listener).await {
                Ok(bus) => {
                    info!("Took {clients} clients over from the previous instance of the bus.");
                    if !activated {
                        warn_if_moved(&bus, address);
                    }

                    return Ok(Some(bus));
                }
                Err(e) => {
                    error!(
                        "Failed to take over from the previous instance of the bus, which \
                         disconnects its {clients} clients: {e:#}"
                    );
                    listener = fallback;
                    rebinds |= stored_listener;
                }
            }
        }

        match listener {
            Some(listener) => Ok(Some(Bus::for_listener(listener).await?)),
            None => {
                // The socket goes once systemd closes its copy too, as it gets to its removal,
                // which the bus waits for, so that it finds the socket stale.
                if let Some(notifier) = notifier.filter(|_| rebinds) {
                    if let Err(e) = notifier.barrier(BARRIER_TIMEOUT) {
                        warn!("Failed to wait for systemd to let go of the stored socket: {e}");
                    }
                }

                Ok(None)
            }
        }
    }
}

/// How many file descriptors the file descriptor store of systemd takes, if that's enough for busd
/// to hand over.
///
/// systemd keeps a store for busd if its unit has `FileDescriptorStoreMax=`, which it tells busd of
/// from version 254 on. busd hands over if the store takes the state of the bus and one more file
/// descriptor: the socket that the bus listens on, unless socket activation passed it, or that of a
/// client. It says why it doesn't hand over otherwise.
pub fn store_max() -> Option<usize> {
    match systemd::fd_store_max() {
        Some(max) if max >= MIN_STORE => Some(max),
        Some(max) => {
            warn!(
                "Not handing over to the next instance of the bus, as the file descriptor store \
                 takes {max}, which is too few: raise `FileDescriptorStoreMax=` of the service."
            );

            None
        }
        None => {
            info!(
                "Not handing over to the next instance of the bus, as systemd keeps no file \
                 descriptor store for it: `$FDSTORE` isn't set."
            );

            None
        }
    }
}

/// Hand `bus` over to its next instance, through the file descriptor store of systemd, which
/// `notifier` reaches.
///
/// The state of the bus goes first, then the socket that it listens on, and then the clients that
/// fit, each with the file descriptors of its connection, as zbus encodes its hand-over: its
/// socket, followed by those that came with the messages that the bus didn't take, as many as the
/// store takes, `max`, from [`store_max`], and as systemd passes the next instance. systemd stores
/// one file descriptor of each open file description, so a duplicate of one that was stored, as
/// when a client sent the same file descriptor twice, goes as a place-holder, under a name that
/// says which file descriptor the next instance is to duplicate in its place. That instance
/// starts with the soft limit of open file descriptors that busd started with, `fd_limit`, unless
/// the limit of the unit changed meanwhile, and systemd fails to start it with more than that limit
/// takes, after the socket of the socket activation, if any. The clients that don't fit get
/// disconnected as the bus goes, with a warning.
///
/// `reserve`, if busd could make one as it started, is given up for what handing over opens.
pub async fn hand_over(
    bus: Bus,
    notifier: &Notifier,
    max: usize,
    fd_limit: Option<u64>,
    reserve: Option<Reserve>,
) -> Result<()> {
    let (for_listener, for_storing) = reserve.map(|r| (r.listener, r.storing)).unzip();
    drop(for_listener);
    let mut handover = SharedRuntime::current()?
        .timeout(HAND_OVER_TIMEOUT, bus.hand_over())
        .await
        .context("The bus took too long to hand over")??;
    drop(for_storing);
    // The next instance gets the state and the socket to listen on, from the store or from the
    // socket activation, and then the clients.
    let in_store = max.saturating_sub(1 + usize::from(handover.listener.is_some()));
    let under_limit = fd_limit
        .and_then(|limit| usize::try_from(limit).ok())
        .map_or(usize::MAX, |limit| {
            limit.saturating_sub(FIRST_PASSED_FD + 2 + SPARE_FDS)
        });
    let clients = handover.clients.len();
    fit(&mut handover.clients, in_store.min(under_limit));
    let fit = handover.clients.len();
    if fit < clients {
        let why = match fd_limit {
            Some(limit) if under_limit < in_store => format!(
                "systemd only passes the next instance as many file descriptors as its soft limit \
                 of open file descriptors takes, {limit} as busd started: raise that of \
                 `LimitNOFILE=`"
            ),
            _ => format!("the file descriptor store takes {max}: raise `FileDescriptorStoreMax=`"),
        };
        warn!("Handing over {fit} of the {clients} clients, as {why} of the service.");
    }
    let state = systemd::memfd(STATE, &handover.state)?;
    notifier.store(STATE, &[state.as_fd()])?;
    // The place-holders of the duplicates take its place among the open file descriptors.
    drop(state);
    if let Some(listener) = &handover.listener {
        notifier.store(LISTENER, &[listener.as_fd()])?;
    }
    let duplicates = duplicates(&handover.clients);
    let mut stored = usize::from(handover.listener.is_some());
    for client in &handover.clients {
        let id = client.id;
        for (n, fd) in client.fds.iter().enumerate() {
            let Some((original_id, original_n)) = duplicates.get(&(id, n)) else {
                notifier.store(&format!("{CLIENT}{id}-{n}"), &[fd.as_fd()])?;

                continue;
            };
            let name = format!("{CLIENT}{id}-{n}-{original_id}-{original_n}");
            let place_holder = systemd::memfd(&name, &[])?;
            notifier.store(&name, &[place_holder.as_fd()])?;
        }
        stored += client.fds.len();
    }
    // The service manager attributes what the bus sent to it by its process, which goes next.
    notifier.barrier(BARRIER_TIMEOUT)?;
    debug!("Stored the state of the bus and {stored} file descriptors for its next instance");

    Ok(())
}

/// File descriptors that busd keeps open from when it starts, to give up for those that handing
/// over opens: a duplicate of the socket that the bus listens on, before it duplicates those of its
/// clients, and the state and the barrier, after. However many clients the bus serves by then, and
/// however many file descriptors that takes, it has those to spare.
#[derive(Debug)]
pub struct Reserve {
    /// For the duplicate of the socket that the bus listens on.
    listener: OwnedFd,
    /// For the state, and the two ends of the pipe of the barrier.
    storing: [OwnedFd; 3],
}

impl Reserve {
    /// Four file descriptors to spare: the ends of two pipes.
    pub fn new() -> Result<Self> {
        let (listener, storing) = pipe_with(PipeFlags::CLOEXEC)?;
        let (more, still_more) = pipe_with(PipeFlags::CLOEXEC)?;

        Ok(Self {
            listener,
            storing: [storing, more, still_more],
        })
    }
}

/// Keep those of `clients` that fit in `room` file descriptors, in order: each takes those of its
/// connection, its socket and those that came with the messages that the bus didn't take.
///
/// A client that doesn't fit is left out whole, as the next instance couldn't serve it without all
/// of them, while those after it, of fewer file descriptors, may still fit.
fn fit(clients: &mut Vec<ClientHandover>, mut room: usize) {
    clients.retain(|client| {
        let Some(left) = room.checked_sub(client.fds.len()) else {
            return false;
        };
        room = left;

        true
    });
}

/// The duplicates among the file descriptors of the connections of `clients`: where each one is,
/// and where the file descriptor that it duplicates is.
///
/// A duplicate is of the same open file description as a file descriptor before it, which systemd
/// stores alone. Only file descriptors of the same file can be of the same open file description,
/// so only those are compared, and only the kernel tells whether they are
/// ([`systemd::same_file`]). Where it doesn't, they are stored as they are, rather than one taken
/// for a duplicate of the other, whose place-holder would get a duplicate of the other in its
/// place, sharing its offset. systemd may then take them for the same and store the first alone,
/// as it guesses that two file descriptors of one file with the same flags are: the client lacks
/// the other then, and the next instance fails to serve it, and cleans up after it, as after a
/// client that went.
fn duplicates(clients: &[ClientHandover]) -> HashMap<Place, Place> {
    let mut by_file = HashMap::<_, Vec<_>>::new();
    let mut duplicates = HashMap::new();
    for client in clients {
        for (n, fd) in client.fds.iter().enumerate() {
            // A file descriptor whose file can't be told is stored all the same.
            let Ok(stat) = fstat(fd) else {
                continue;
            };
            let originals: &mut Vec<_> = by_file.entry((stat.st_dev, stat.st_ino)).or_default();
            let original = originals.iter().find(|(_, original)| {
                systemd::same_file(fd.as_fd(), *original).is_ok_and(|same| same)
            });
            match original {
                Some((place, _)) => {
                    duplicates.insert((client.id, n), *place);
                }
                None => originals.push(((client.id, n), fd.as_fd())),
            }
        }
    }

    duplicates
}

/// Warn if `bus`, which took over from its previous instance, listens on another socket file than
/// at `address`, the one that busd is to listen at, as when that changed: busd only gets there once
/// it starts afresh.
fn warn_if_moved(bus: &Bus, address: Option<&str>) {
    let Some(configured) = address.and_then(|address| Address::from_str(address).ok()) else {
        return;
    };
    let (Some(path), Some(configured)) = (socket_file(bus.address()), socket_file(&configured))
    else {
        return;
    };
    if path != configured {
        warn!(
            "Listening at `{}`, as the previous instance of the bus did, rather than at `{}`: \
             busd only gets there once it starts afresh, after a stop.",
            path.display(),
            configured.display(),
        );
    }
}

/// The socket file of `address`, if it's at one.
fn socket_file(address: &Address) -> Option<&Path> {
    match address.transport() {
        Transport::Unix(unix) => match unix.path() {
            UnixSocket::File(path) => Some(path),
            _ => None,
        },
        _ => None,
    }
}

/// Where the file descriptor that `name` tells of is, a name that busd stores a file descriptor of
/// a client under, past [`CLIENT`], if it is one, and if it is the place-holder of a duplicate,
/// where the file descriptor that it is a duplicate of is.
fn client_fd(name: &str) -> Option<(Place, Option<Place>)> {
    let numbers = name
        .split('-')
        .map(|number| number.parse().ok())
        .collect::<Option<Vec<_>>>()?;

    match numbers[..] {
        [id, n] => Some(((id, n), None)),
        [id, n, original_id, original_n] => Some(((id, n), Some((original_id, original_n)))),
        _ => None,
    }
}

/// Where a file descriptor is among those of the connections of the clients: the number of the
/// client, and the place of the file descriptor among those of its connection.
type Place = (usize, usize);

/// The name that the state of the bus is stored under.
const STATE: &str = "busd-state";
/// The name that the socket that the bus listens on is stored under, unless systemd's socket
/// activation passed it.
const LISTENER: &str = "busd-listener";
/// The prefix of the names that the file descriptors of the connections of the clients are stored
/// under, before the number of the client, a `-`, and the number of the file descriptor among those
/// of the connection of the client, from 0, its socket, on, in the order of the hand-over of the
/// connection. The place-holder of a duplicate has the number of the client, and that of the file
/// descriptor, of the one that it is a duplicate of after those, each after a `-` too.
const CLIENT: &str = "busd-client-";
/// How long the bus may take to hand over, before busd gives up on it, and its clients get
/// disconnected as it goes.
///
/// The bus gives each client as long as it has to take a message, `SEND_TIMEOUT`, 10 s, from when
/// the hand-over starts, to take all that the bus sends it meanwhile, so a client that doesn't read
/// holds the hand-over up for no longer than that. This is longer, with room for the rest of the
/// hand-over, and well within the 90 s that systemd gives a service to stop by default.
const HAND_OVER_TIMEOUT: Duration = Duration::from_secs(30);
/// How long busd waits for the service manager to deal with what busd sent it, before it goes on
/// anyway.
const BARRIER_TIMEOUT: Duration = Duration::from_secs(5);
/// The fewest file descriptors that a store has to take for busd to hand over: the state of the bus
/// and one more.
const MIN_STORE: usize = 2;
/// The first file descriptor that systemd passes, `SD_LISTEN_FDS_START`.
const FIRST_PASSED_FD: usize = 3;
/// How many file descriptors the next instance has to spare, under its soft limit, besides those
/// that systemd passes it, to start with if it can't raise the limit.
const SPARE_FDS: usize = 32;

#[cfg(test)]
mod tests {
    use std::{
        fs::File,
        os::fd::{AsRawFd, RawFd},
    };

    use super::*;

    /// The names of the file descriptors that busd stores of a client tell which client each is
    /// of, and where it is among those of the connection of the client.
    #[test]
    fn stored_names() {
        assert_eq!(client_fd("3-0"), Some(((3, 0), None)));
        assert_eq!(client_fd("12-34"), Some(((12, 34), None)));
        assert_eq!(client_fd("12-34-5-6"), Some(((12, 34), Some((5, 6)))));
        for name in [
            "",
            "x",
            "3",
            "3-",
            "-0",
            "3-x",
            "3-fd-0",
            "3-1-2",
            "3-1-2-",
            "3-1-2-3-4",
        ] {
            assert_eq!(client_fd(name), None, "{name:?}");
        }
    }

    /// A name of a client that busd doesn't know, as one that a version of busd that named them
    /// otherwise stored, is removed from the store, rather than taken for the socket of socket
    /// activation.
    #[test]
    fn unknown_client_names_are_removed() {
        let passed = vec![
            (STATE.to_owned(), systemd::memfd(STATE, b"state").unwrap()),
            (LISTENER.to_owned(), null()),
            (format!("{CLIENT}3"), null()),
            (format!("{CLIENT}4-fd-0"), null()),
        ];

        let passed = Passed::new(passed).unwrap();
        assert!(passed.listener.is_none());
        assert_eq!(
            passed.stored,
            [
                STATE,
                LISTENER,
                &format!("{CLIENT}3"),
                &format!("{CLIENT}4-fd-0")
            ]
        );
        assert!(passed.handover.unwrap().clients.is_empty());
    }

    /// The place-holder of a duplicate gets the client a duplicate of the file descriptor that it
    /// says, in its place, which may be one of another client.
    #[test]
    fn duplicates_go_back_to_their_clients() {
        let state = systemd::memfd(STATE, b"state").unwrap();
        let passed = vec![
            (STATE.to_owned(), state),
            (format!("{CLIENT}1-0"), null()),
            (format!("{CLIENT}1-1"), null()),
            (format!("{CLIENT}1-2-1-1"), null()),
            (format!("{CLIENT}2-0"), null()),
            (format!("{CLIENT}2-1-1-1"), null()),
        ];

        let passed = Passed::new(passed).unwrap();
        let mut clients = passed.handover.unwrap().clients;
        clients.sort_unstable_by_key(|client| client.id);
        let [first, second] = &clients[..] else {
            panic!("{} clients were passed", clients.len());
        };
        let original = first.fds[1].as_fd();
        let same = |fd: &OwnedFd| systemd::same_file(original, fd.as_fd()).unwrap();
        assert_eq!(
            first.fds.iter().map(same).collect::<Vec<_>>(),
            [false, true, true]
        );
        assert_eq!(
            second.fds.iter().map(same).collect::<Vec<_>>(),
            [false, true]
        );
        assert_eq!(passed.stored.len(), 6);
    }

    /// A file descriptor of the same open file description as one before it, among those of the
    /// same client or of another one, is a duplicate of the first, while one of another open file
    /// description of the same file isn't.
    #[test]
    fn duplicates_are_found() {
        let shared = null();
        let duplicate = || shared.try_clone().unwrap();
        let clients = vec![
            ClientHandover {
                id: 1,
                fds: vec![null(), duplicate(), null(), duplicate()],
            },
            ClientHandover {
                id: 2,
                fds: vec![null(), duplicate()],
            },
        ];

        let duplicates = duplicates(&clients);
        let expected = HashMap::from([((1, 3), (1, 1)), ((2, 1), (1, 1))]);
        assert_eq!(duplicates, expected);
    }

    /// The file descriptors of the connection of a client go back to it in the order that they
    /// were stored in, its socket first, whatever the order that systemd passes them in, and those
    /// of a client whose socket is missing close.
    #[test]
    fn passed_fds_go_back_to_their_clients() {
        let state = systemd::memfd(STATE, b"state").unwrap();
        let fds = [null(), null(), null()];
        let raw: Vec<RawFd> = fds.iter().map(AsRawFd::as_raw_fd).collect();
        let [socket, first, second] = fds;
        let passed = vec![
            (format!("{CLIENT}1-2"), second),
            (STATE.to_owned(), state),
            (format!("{CLIENT}2-1"), null()),
            (format!("{CLIENT}1-0"), socket),
            (format!("{CLIENT}1-1"), first),
        ];

        let passed = Passed::new(passed).unwrap();
        let handover = passed.handover.unwrap();
        let [client] = &handover.clients[..] else {
            panic!("{} clients were passed", handover.clients.len());
        };
        assert_eq!(client.id, 1);
        let got: Vec<RawFd> = client.fds.iter().map(AsRawFd::as_raw_fd).collect();
        assert_eq!(got, raw);
        assert!(passed.listener.is_none());
    }

    /// A client takes the file descriptors of its connection, its socket and those that came with
    /// the messages that the bus didn't take, and one that doesn't fit is left out whole, while
    /// those after it may still fit.
    #[test]
    fn clients_fit_whole() {
        let mut clients: Vec<_> = [1, 3, 1, 2].into_iter().enumerate().map(client).collect();

        fit(&mut clients, 3);

        let ids: Vec<_> = clients.iter().map(|client| client.id).collect();
        assert_eq!(ids, [0, 2]);
    }

    /// The client numbered `id`, with `fds` file descriptors of its connection.
    fn client((id, fds): (usize, usize)) -> ClientHandover {
        ClientHandover {
            id,
            fds: (0..fds).map(|_| null()).collect(),
        }
    }

    /// A file descriptor of no consequence.
    fn null() -> OwnedFd {
        File::open("/dev/null").unwrap().into()
    }
}
