use crate::stats::StatCollector;
use crate::{cache::ClientCache, GEXEC};
use anyhow::Context;
use governor::Quota;
use once_cell::sync::Lazy;
use pnet_packet::{
    ipv4::Ipv4Packet,
    tcp::{TcpFlags, TcpPacket},
    Packet,
};
use smol::channel::{Receiver, Sender};
use smol::prelude::*;
use smol_timeout::TimeoutExt;
use std::{
    io::{Stdin, Stdout},
    num::NonZeroU32,
    time::Duration,
};
use std::{sync::Arc, time::Instant};
use vpn_structs::StdioMsg;

/// An "actor" that keeps a client session alive.
pub struct Keepalive {
    open_socks5_conn: Sender<(String, Sender<sosistab::mux::RelConn>)>,
    get_stats: Sender<Sender<sosistab::SessionStats>>,
    _task: smol::Task<anyhow::Result<()>>,
}

impl Keepalive {
    /// Creates a new keepalive.
    pub fn new(
        stats: Arc<StatCollector>,
        exit_host: &str,
        use_bridges: bool,
        stdio_vpn: bool,
        ccache: Arc<ClientCache>,
    ) -> Self {
        let (send, recv) = smol::channel::unbounded();
        let (send_stats, recv_stats) = smol::channel::unbounded();
        Keepalive {
            open_socks5_conn: send,
            get_stats: send_stats,
            _task: GEXEC.spawn(keepalive_actor(
                stats,
                exit_host.to_string(),
                use_bridges,
                stdio_vpn,
                ccache,
                recv,
                recv_stats,
            )),
        }
    }

    /// Opens a connection
    pub async fn connect(&self, remote: &str) -> anyhow::Result<sosistab::mux::RelConn> {
        let (send, recv) = smol::channel::bounded(1);
        self.open_socks5_conn
            .send((remote.to_string(), send))
            .await?;
        Ok(recv.recv().await?)
    }

    /// Gets session statistics
    pub async fn get_stats(&self) -> anyhow::Result<sosistab::SessionStats> {
        let (send, recv) = smol::channel::bounded(1);
        self.get_stats.send(send).await?;
        Ok(recv.recv().await?)
    }
}

async fn keepalive_actor(
    stats: Arc<StatCollector>,
    exit_host: String,
    use_bridges: bool,
    stdio_vpn: bool,
    ccache: Arc<ClientCache>,
    recv_socks5_conn: Receiver<(String, Sender<sosistab::mux::RelConn>)>,
    recv_get_stats: Receiver<Sender<sosistab::SessionStats>>,
) -> anyhow::Result<()> {
    loop {
        if let Err(err) = keepalive_actor_once(
            stats.clone(),
            exit_host.clone(),
            use_bridges,
            stdio_vpn,
            ccache.clone(),
            recv_socks5_conn.clone(),
            recv_get_stats.clone(),
        )
        .await
        {
            log::warn!("keepalive_actor restarting: {}", err);
            smol::Timer::after(Duration::from_secs(1)).await;
        }
    }
}

async fn keepalive_actor_once(
    stats: Arc<StatCollector>,
    exit_host: String,
    use_bridges: bool,
    stdio_vpn: bool,
    ccache: Arc<ClientCache>,
    recv_socks5_conn: Receiver<(String, Sender<sosistab::mux::RelConn>)>,
    recv_get_stats: Receiver<Sender<sosistab::SessionStats>>,
) -> anyhow::Result<()> {
    stats.set_exit_descriptor(None);

    // find the exit
    let mut exits = ccache.get_exits().await.context("can't get exits")?;
    if exits.is_empty() {
        anyhow::bail!("no exits found")
    }
    exits.sort_by(|a, b| {
        strsim::damerau_levenshtein(&a.hostname, &exit_host)
            .cmp(&strsim::damerau_levenshtein(&b.hostname, &exit_host))
    });
    let exit_host = exits[0].hostname.clone();

    let bridge_sess_async = async {
        let bridges = ccache
            .get_bridges(&exit_host)
            .await
            .context("can't get bridges")?;
        log::debug!("got {} bridges", bridges.len());
        if bridges.is_empty() {
            anyhow::bail!("absolutely no bridges found")
        }
        // spawn a task for *every* bridge
        let (send, recv) = smol::channel::unbounded();
        let _tasks: Vec<_> = bridges
            .into_iter()
            .map(|desc| {
                let send = send.clone();
                GEXEC.spawn(async move {
                    log::debug!("connecting through {}...", desc.endpoint);
                    drop(
                        send.send((
                            desc.endpoint,
                            sosistab::connect(desc.endpoint, desc.sosistab_key).await,
                        ))
                        .await,
                    )
                })
            })
            .collect();
        // wait for a successful result
        loop {
            let (saddr, res) = recv.recv().await.context("ran out of bridges")?;
            if let Ok(res) = res {
                log::info!("{} is our fastest bridge", saddr);
                break Ok(res);
            }
        }
    };
    let exit_info = exits.iter().find(|v| v.hostname == exit_host).unwrap();
    let connected_sess_async = async {
        if use_bridges {
            bridge_sess_async.await
        } else {
            async {
                Ok(infal(
                    sosistab::connect(
                        smol::net::resolve(format!("{}:19831", exit_info.hostname))
                            .await
                            .context("can't resolve hostname of exit")?[0],
                        exit_info.sosistab_key,
                    )
                    .await,
                )
                .await)
            }
            .or(async {
                smol::Timer::after(Duration::from_secs(5)).await;
                log::warn!("turning on bridges because we couldn't get a direct connection");
                bridge_sess_async.await
            })
            .await
        }
    };
    let session: anyhow::Result<sosistab::Session> = connected_sess_async
        .or(async {
            smol::Timer::after(Duration::from_secs(10)).await;
            anyhow::bail!("initial connection timeout after 10");
        })
        .await;
    let session = session?;
    let mux = Arc::new(sosistab::mux::Multiplex::new(session));
    let scope = smol::Executor::new();
    // now let's authenticate
    let token = ccache.get_auth_token().await?;
    authenticate_session(&mux, &token)
        .timeout(Duration::from_secs(5))
        .await
        .ok_or_else(|| anyhow::anyhow!("authentication timed out"))??;
    // TODO actually authenticate
    log::info!(
        "KEEPALIVE MAIN LOOP for exit_host={}, use_bridges={}",
        exit_host,
        use_bridges
    );
    stats.set_exit_descriptor(Some(exits[0].clone()));
    scope
        .spawn(async {
            loop {
                smol::Timer::after(Duration::from_secs(200)).await;
                if mux
                    .open_conn(None)
                    .timeout(Duration::from_secs(60))
                    .await
                    .is_none()
                {
                    log::warn!("watchdog conn didn't work!");
                }
            }
        })
        .detach();

    // VPN mode
    let mut _nuunuu = None;
    if stdio_vpn {
        _nuunuu = Some(GEXEC.spawn(run_vpn(stats.clone(), mux.clone())));
    }

    let (send_death, recv_death) = smol::channel::unbounded::<anyhow::Error>();
    scope
        .run(
            async {
                loop {
                    let (conn_host, conn_reply) = recv_socks5_conn
                        .recv()
                        .await
                        .context("cannot get socks5 connect request")?;
                    let mux = &mux;
                    let send_death = send_death.clone();
                    scope
                        .spawn(async move {
                            let start = Instant::now();
                            let remote = (&mux).open_conn(Some(conn_host)).await;
                            match remote {
                                Ok(remote) => {
                                    let sess_stats = mux.get_session().get_stats().await;
                                    log::debug!(
                                        "opened connection in {} ms; loss = {:.2}% => {:.2}%; overhead = {:.2}%",
                                        start.elapsed().as_millis(),
                                        sess_stats.down_loss * 100.0,
                                        sess_stats.down_recovered_loss * 100.0,
                                        sess_stats.down_redundant * 100.0,
                                    );
                                    conn_reply.send(remote).await?;
                                    Ok::<(), anyhow::Error>(())
                                }
                                Err(err) => {
                                    send_death
                                        .send(anyhow::anyhow!(
                                            "conn open error {} in {}s",
                                            err,
                                            start.elapsed().as_secs_f64()
                                        ))
                                        .await?;
                                    Ok(())
                                }
                            }
                        })
                        .detach();
                }
            }
            .or(async {
                let e = recv_death.recv().await?;
                anyhow::bail!(e)
            })
            .or(async {
                loop {
                    let stat_send = recv_get_stats.recv().await?;
                    let stats = mux.get_session().get_stats().await;
                    drop(stat_send.send(stats).await);
                }
            }),
        )
        .await
}

async fn infal<T, E>(v: Result<T, E>) -> T {
    if let Ok(v) = v {
        v
    } else {
        smol::future::pending().await
    }
}

/// authenticates a muxed session
async fn authenticate_session(
    session: &sosistab::mux::Multiplex,
    token: &crate::cache::Token,
) -> anyhow::Result<()> {
    let mut auth_conn = session.open_conn(None).await?;
    log::debug!("sending auth info...");
    aioutils::write_pascalish(
        &mut auth_conn,
        &(
            &token.unblinded_digest,
            &token.unblinded_signature,
            &token.level,
        ),
    )
    .await?;
    let _: u8 = aioutils::read_pascalish(&mut auth_conn).await?;
    Ok(())
}

/// runs a vpn session
async fn run_vpn(
    stats: Arc<StatCollector>,
    mux: Arc<sosistab::mux::Multiplex>,
) -> anyhow::Result<()> {
    static STDIN: Lazy<async_dup::Arc<async_dup::Mutex<smol::Unblock<Stdin>>>> = Lazy::new(|| {
        async_dup::Arc::new(async_dup::Mutex::new(smol::Unblock::with_capacity(
            1024 * 1024,
            std::io::stdin(),
        )))
    });
    static STDOUT: Lazy<async_dup::Arc<async_dup::Mutex<smol::Unblock<Stdout>>>> =
        Lazy::new(|| {
            async_dup::Arc::new(async_dup::Mutex::new(smol::Unblock::with_capacity(
                64 * 1024,
                std::io::stdout(),
            )))
        });
    let mut stdin = STDIN.clone();
    let mut stdout = STDOUT.clone();
    // first we negotiate the vpn
    let client_id: u128 = rand::random();
    log::info!("negotiating VPN with client id {}...", client_id);
    let client_ip = loop {
        let hello = vpn_structs::Message::ClientHello { client_id };
        mux.send_urel(bincode::serialize(&hello)?.into()).await?;
        let resp = mux.recv_urel().timeout(Duration::from_secs(1)).await;
        if let Some(resp) = resp {
            let resp = resp?;
            let resp: vpn_structs::Message = bincode::deserialize(&resp)?;
            match resp {
                vpn_structs::Message::ServerHello { client_ip, .. } => break client_ip,
                _ => continue,
            }
        }
    };
    log::info!("negotiated IP address {}!", client_ip);
    let msg = StdioMsg {
        verb: 1,
        body: format!("{}/10", client_ip).as_bytes().to_vec().into(),
    };
    msg.write(&mut stdout).await?;
    stdout.flush().await?;

    let vpn_up_fut = {
        let mux = mux.clone();
        let stats = stats.clone();
        async move {
            let ack_rate_limits: Vec<_> = (0..16)
                .map(|_| {
                    governor::RateLimiter::direct(Quota::per_second(
                        NonZeroU32::new(500u32).unwrap(),
                    ))
                })
                .collect();

            loop {
                let msg = StdioMsg::read(&mut stdin).await?;
                // ACK decimation
                if let Some(hash) = ack_decimate(&msg.body) {
                    let limiter = &(ack_rate_limits[(hash % 16) as usize]);
                    if limiter.check().is_err() {
                        continue;
                    }
                }
                stats.incr_total_tx(msg.body.len() as u64);
                drop(
                    mux.send_urel(
                        bincode::serialize(&vpn_structs::Message::Payload(msg.body))
                            .unwrap()
                            .into(),
                    )
                    .await,
                );
            }
        }
    };
    let vpn_down_fut = {
        let stats = stats.clone();
        async move {
            for count in 0u64.. {
                if count % 1000 == 0 {
                    let sess_stats = mux.get_session().get_stats().await;
                    log::debug!(
                    "VPN received {} pkts; ping {} ms; loss = {:.2}% => {:.2}%; overhead = {:.2}%",
                    count,
                    sess_stats.ping.as_millis(),
                    sess_stats.down_loss * 100.0,
                    sess_stats.down_recovered_loss * 100.0,
                    sess_stats.down_redundant * 100.0,
                );
                }
                let bts = mux.recv_urel().await?;
                if let vpn_structs::Message::Payload(bts) = bincode::deserialize(&bts)? {
                    stats.incr_total_rx(bts.len() as u64);
                    let msg = StdioMsg { verb: 0, body: bts };
                    msg.write(&mut stdout).await?;
                    stdout.flush().await?
                }
            }
            unreachable!()
        }
    };
    smol::future::race(GEXEC.spawn(vpn_up_fut), GEXEC.spawn(vpn_down_fut)).await
}

/// returns ok if it's an ack that needs to be decimated
fn ack_decimate(bts: &[u8]) -> Option<u16> {
    let parsed = Ipv4Packet::new(bts)?;
    let parsed = TcpPacket::new(parsed.payload())?;
    let flags = parsed.get_flags();
    if flags & TcpFlags::ACK != 0 && flags & TcpFlags::SYN == 0 && parsed.payload().is_empty() {
        let hash = parsed.get_destination() ^ parsed.get_source();
        Some(hash)
    } else {
        None
    }
}
