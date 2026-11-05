//! Long-lived clients for checking that a bus restart drops no one and loses nothing.
//!
//! - `serve`: owns `org.zbus.HandoverTest`, answers `Ping`, and emits `Tick(n)` every 10 ms.
//! - `listen`: receives the ticks and reports any that went missing or came twice.
//! - `ping`: calls `Ping` in a loop and reports errors and the slowest round trip each second.
//!
//! Each prints its unique name first, which has to stay the same across a restart, and reports
//! once a second. They all connect to the bus that `DBUS_SESSION_BUS_ADDRESS` names.

use std::{
    env,
    time::{Duration, Instant},
};

use futures_util::StreamExt;
use zbus::{Connection, interface, object_server::SignalEmitter, proxy};

const NAME: &str = "org.zbus.HandoverTest";
const PATH: &str = "/org/zbus/HandoverTest";

fn main() -> zbus::Result<()> {
    let mode = env::args().nth(1).unwrap_or_default();

    zbus::block_on(async {
        match mode.as_str() {
            "serve" => serve().await,
            "listen" => listen().await,
            "ping" => ping().await,
            _ => {
                eprintln!("Usage: handover-check serve|listen|ping");
                std::process::exit(2);
            }
        }
    })
}

struct Service;

#[interface(name = "org.zbus.HandoverTest")]
impl Service {
    fn ping(&self, n: u64) -> u64 {
        n
    }

    #[zbus(signal)]
    async fn tick(emitter: &SignalEmitter<'_>, n: u64) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.zbus.HandoverTest",
    default_service = "org.zbus.HandoverTest",
    default_path = "/org/zbus/HandoverTest"
)]
trait HandoverTest {
    fn ping(&self, n: u64) -> zbus::Result<u64>;

    #[zbus(signal)]
    fn tick(&self, n: u64) -> zbus::Result<()>;
}

async fn serve() -> zbus::Result<()> {
    let conn = zbus::connection::Builder::session()
        .name(NAME)
        .serve_at(PATH, Service)
        .build()
        .await?;
    print_unique_name(&conn);
    let emitter = SignalEmitter::new(&conn, PATH)?;
    let mut last_report = Instant::now();
    for n in 0u64.. {
        if let Err(e) = Service::tick(&emitter, n).await {
            println!("tick {n}: failed to emit: {e}");
            if conn.is_closed() {
                println!("the connection to the bus is gone");
                return Ok(());
            }
        }
        if last_report.elapsed() >= Duration::from_secs(1) {
            println!("emitted up to tick {n}");
            last_report = Instant::now();
        }
        async_io::Timer::after(Duration::from_millis(10)).await;
    }

    Ok(())
}

async fn listen() -> zbus::Result<()> {
    let conn = Connection::session().await?;
    print_unique_name(&conn);
    let proxy = HandoverTestProxy::new(&conn).await?;
    let mut ticks = proxy.receive_tick().await?;
    let mut expected = None;
    let (mut received, mut missing, mut repeated) = (0u64, 0u64, 0u64);
    let mut last_report = Instant::now();
    while let Some(tick) = ticks.next().await {
        let n = tick.args()?.n;
        match expected {
            Some(e) if n > e => {
                println!("MISSING ticks {e}..{n}");
                missing += n - e;
            }
            Some(e) if n < e => {
                println!("REPEATED or reordered tick {n}, expected {e}");
                repeated += 1;
            }
            _ => (),
        }
        expected = Some(n.max(expected.unwrap_or(0)) + 1);
        received += 1;
        if last_report.elapsed() >= Duration::from_secs(1) {
            println!("received {received} ticks, last {n}; missing {missing}, repeated {repeated}");
            last_report = Instant::now();
        }
    }
    println!("the stream of ticks ended: the connection to the bus is gone");

    Ok(())
}

async fn ping() -> zbus::Result<()> {
    let conn = Connection::session().await?;
    print_unique_name(&conn);
    let proxy = HandoverTestProxy::new(&conn).await?;
    let (mut calls, mut errors) = (0u64, 0u64);
    let mut slowest = Duration::ZERO;
    let mut last_report = Instant::now();
    for n in 0u64.. {
        let start = Instant::now();
        match proxy.ping(n).await {
            Ok(r) if r == n => (),
            Ok(r) => {
                println!("WRONG reply to ping {n}: {r}");
                errors += 1;
            }
            Err(e) => {
                println!("ERROR on ping {n}: {e}");
                errors += 1;
                if conn.is_closed() {
                    println!("the connection to the bus is gone");
                    return Ok(());
                }
            }
        }
        calls += 1;
        slowest = slowest.max(start.elapsed());
        if last_report.elapsed() >= Duration::from_secs(1) {
            println!("{calls} calls, {errors} errors, slowest round trip {slowest:?}");
            slowest = Duration::ZERO;
            last_report = Instant::now();
        }
    }

    Ok(())
}

fn print_unique_name(conn: &Connection) {
    match conn.unique_name() {
        Some(name) => println!("connected as {name}"),
        None => println!("connected"),
    }
}
