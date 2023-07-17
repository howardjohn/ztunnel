use std::net::{Ipv4Addr, SocketAddr};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};
use drain::Watch;
use etherparse::IpHeader;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio_tun::Tun;
use tracing::{error, info, info_span, Instrument, warn};
use crate::proxy::{Error, ProxyInputs, TraceParent, util};
use crate::proxy::outbound::OutboundConnection;

pub struct OutboundIP {
    pi: ProxyInputs,
    tun: Tun,
    drain: Watch,
}

impl OutboundIP {
    pub(super) async fn new(mut pi: ProxyInputs, drain: Watch) -> Result<OutboundIP, Error> {
        let tun = Tun::builder()
            .name("")            // if name is empty, then it is set by kernel.
            .tap(false)          // false (default): TUN, true: TAP.
            .packet_info(false)  // false: IFF_NO_PI, default is true.
            .mtu(1350)
            .up()                // or set it up manually using `sudo ip link set <tun-name> up`.
            .address(Ipv4Addr::new(172, 31, 0, 1))
            .destination(Ipv4Addr::new(172, 31, 200, 1))
            .broadcast(Ipv4Addr::BROADCAST)
            .netmask(Ipv4Addr::new(255, 255, 255, 0))
            .try_build()         // or `.try_build_mq(queues)` for multi-queue support.
            .unwrap();


        info!(
            component="outbound ip",
            fd=?tun.as_raw_fd(),
            addr=?tun.address(),
            name=tun.name(),
            "tun created",
        );
        Ok(OutboundIP {
            pi,
            tun,
            drain,
        })
    }

    pub(super) async fn run(self) {
        let (mut reader, mut _writer) = tokio::io::split(self.tun);

        let mut buf = [0u8; 1024];
        loop {
            let n = reader.read(&mut buf).await.unwrap();
            info!("reading {} bytes: {:?}", n, &buf[..n]);
            let ip = IpHeader::from_slice(&buf[..n]);
            info!("read {ip:?}");
        }
        // Stop accepting once we drain.
        // Note: we are *not* waiting for all connections to be closed. In the future, we may consider
        // this, but will need some timeout period, as we have no back-pressure mechanism on connections.
        tokio::select! {
            _ = self.drain.signaled() => {
                info!("outbound IP drained");
            }
        }
    }
}
