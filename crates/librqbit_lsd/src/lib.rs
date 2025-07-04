use std::{
    collections::HashMap,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
    str::{FromStr, from_utf8},
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use futures::Stream;
use librqbit_core::{Id20, spawn_utils::spawn_with_cancel};
use librqbit_dualstack_sockets::MulticastUdpSocket;
use parking_lot::RwLock;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;
use tracing::{error_span, trace};

const LSD_PORT: u16 = 6771;
const LSD_IPV4: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(239, 192, 152, 143), LSD_PORT);
const LSD_IPV6: SocketAddrV6 = SocketAddrV6::new(
    Ipv6Addr::new(0xff15, 0, 0, 0, 0, 0, 0xefc0, 0x988f),
    LSD_PORT,
    0,
    0,
);

struct Announce {
    tx: UnboundedSender<SocketAddr>,
    port: Option<u16>,
}

struct LocalServiceDiscoveryInner {
    socket: MulticastUdpSocket,
    cookie: u32,
    cancel_token: CancellationToken,
    receivers: RwLock<HashMap<Id20, Announce>>,
}

#[derive(Clone)]
pub struct LocalServiceDiscovery {
    inner: Arc<LocalServiceDiscoveryInner>,
}

#[derive(Default)]
pub struct LocalServiceDiscoveryOptions {
    pub cancel_token: CancellationToken,
    pub cookie: Option<u32>,
}

impl LocalServiceDiscovery {
    pub fn new(opts: LocalServiceDiscoveryOptions) -> anyhow::Result<Self> {
        let socket = MulticastUdpSocket::new(LSD_PORT, *LSD_IPV4.ip(), *LSD_IPV6.ip(), None)
            .context("error binding LSD socket")?;
        let cookie = opts.cookie.unwrap_or_else(rand::random);
        let lsd = Self {
            inner: Arc::new(LocalServiceDiscoveryInner {
                socket,
                cookie,
                cancel_token: opts.cancel_token.clone(),
                receivers: Default::default(),
            }),
        };

        spawn_with_cancel(
            error_span!("lsd"),
            opts.cancel_token,
            lsd.clone().task_monitor_recv(),
        );

        Ok(lsd)
    }

    fn gen_announce_msg(&self, info_hash: Id20, port: u16, is_v6: bool) -> bstr::BString {
        let host: SocketAddr = if is_v6 {
            LSD_IPV6.into()
        } else {
            LSD_IPV4.into()
        };
        let cookie = self.inner.cookie;
        let info_hash = info_hash.as_string();
        format!(
            "BT-SEARCH * HTTP/1.1\r
Host: {host}\r
Port: {port}\r
Infohash: {info_hash}\r
cookie: {cookie}\r
\r
\r
"
        )
        .into()
    }

    async fn task_monitor_recv(self) -> anyhow::Result<()> {
        let mut buf = [0u8; 4096];

        loop {
            let mut headers = [httparse::EMPTY_HEADER; 16];
            let (sz, addr) = self.inner.socket.recv_from(&mut buf).await?;
            let buf = bstr::BStr::new(&buf[..sz]);
            match try_parse_bt_search(buf, &mut headers) {
                Ok(bts) => {
                    trace!(?addr, ?bts, "received");
                    if bts.our_cookie == Some(self.inner.cookie) {
                        trace!(?bts, "ignoring our own message");
                        continue;
                    }

                    let reply_port =
                        self.inner
                            .receivers
                            .read()
                            .get(&bts.hash)
                            .and_then(|announce| {
                                let mut addr = addr;
                                addr.set_port(bts.port);
                                announce.tx.send(addr).ok().and(announce.port)
                            });

                    if let Some(port) = reply_port {
                        let reply = self.gen_announce_msg(bts.hash, port, addr.is_ipv6());
                        let addr = if addr.is_ipv6() {
                            LSD_IPV6.into()
                        } else {
                            LSD_IPV4.into()
                        };
                        if let Err(e) = self.inner.socket.send_to(&reply, addr).await {
                            trace!(?addr, ?reply, "error sending reply: {e:#}");
                        } else {
                            trace!(?addr, ?reply, "sent reply");
                        }
                    }
                }
                Err(e) => {
                    trace!(?buf, ?addr, "error parsing message: {e:#}");
                }
            }
        }
    }

    async fn task_announce_periodically(self, info_hash: Id20, port: u16) -> anyhow::Result<()> {
        let mut interval = tokio::time::interval(Duration::from_secs(60 * 5));
        loop {
            interval.tick().await;

            self.inner
                .socket
                .try_send_mcast_everywhere(&|mopts| {
                    self.gen_announce_msg(info_hash, port, mopts.mcast_addr().is_ipv6())
                })
                .await;
        }
    }

    pub fn announce(
        &self,
        info_hash: Id20,
        announce_port: Option<u16>,
    ) -> impl Stream<Item = SocketAddr> + Send + Sync + 'static {
        // 1. Periodically announce the torrent.
        // 2. Stream back the results from received messages.

        let (tx, rx) = unbounded_channel::<SocketAddr>();

        struct AddrStream {
            info_hash: Id20,
            rx: UnboundedReceiver<SocketAddr>,
            lsd: LocalServiceDiscovery,
        }

        impl Stream for AddrStream {
            type Item = SocketAddr;

            fn poll_next(
                mut self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Option<Self::Item>> {
                self.rx.poll_recv(cx)
            }
        }

        impl Drop for AddrStream {
            fn drop(&mut self) {
                let _ = self.lsd.inner.receivers.write().remove(&self.info_hash);
            }
        }

        self.inner.receivers.write().insert(
            info_hash,
            Announce {
                tx,
                port: announce_port,
            },
        );

        if let Some(announce_port) = announce_port {
            let cancel_token = self.inner.cancel_token.child_token();
            spawn_with_cancel(
                error_span!(parent: None, "lsd-announce", ?info_hash, port=announce_port),
                cancel_token,
                self.clone()
                    .task_announce_periodically(info_hash, announce_port),
            );
        }

        AddrStream {
            info_hash,
            rx,
            lsd: self.clone(),
        }
    }
}

#[derive(Debug)]
struct BtSearchAnnounceMessage {
    hash: Id20,
    our_cookie: Option<u32>,
    #[allow(unused)]
    host: SocketAddr,
    port: u16,
}

fn try_parse_bt_search<'a: 'h, 'h>(
    buf: &'a [u8],
    headers: &'a mut [httparse::Header<'h>],
) -> anyhow::Result<BtSearchAnnounceMessage> {
    let mut req = httparse::Request::new(headers);
    req.parse(buf).context("error parsing request")?;

    match req.method {
        Some("BT-SEARCH") => {
            let mut host = None;
            let mut port = None;
            let mut infohash = None;
            let mut our_cookie = None;

            for header in req.headers.iter() {
                if header.name.eq_ignore_ascii_case("host") {
                    host = Some(
                        from_utf8(header.value)
                            .context("invalid utf-8 in host header")?
                            .parse()
                            .context("invalid IP in host header")?,
                    );
                } else if header.name.eq_ignore_ascii_case("port") {
                    port = Some(atoi::atoi::<u16>(header.value).context("port is not a number")?)
                } else if header.name.eq_ignore_ascii_case("infohash") {
                    infohash = Some(
                        Id20::from_str(from_utf8(header.value).context("infohash isn't utf-8")?)
                            .context("invalid infohash header")?,
                    );
                } else if header.name.eq_ignore_ascii_case("cookie") {
                    our_cookie = atoi::atoi::<u32>(header.value);
                }
            }

            match (host, port, infohash) {
                (Some(host), Some(port), Some(hash)) => Ok(BtSearchAnnounceMessage {
                    hash,
                    our_cookie,
                    host,
                    port,
                }),
                _ => anyhow::bail!("not all of host, man and st are set"),
            }
        }
        _ => anyhow::bail!("expecting BT-SEARCH"),
    }
}
