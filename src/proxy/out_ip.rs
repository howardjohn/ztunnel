use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};
use drain::Watch;
use etherparse::IpHeader;
use hyper::upgrade::Upgraded;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tun::Tun;
use tracing::{error, info, info_span, Instrument, warn};
use crate::cert_fetcher::new;
use crate::proxy::{Error, outbound, ProxyInputs, TraceParent, util};
use crate::proxy::outbound::{Direction, OutboundConnection};
use crate::state::workload::NetworkAddress;

pub struct OutboundIP {
    pi: ProxyInputs,
    tunin: Tun,
    drain: Watch,
}

impl OutboundIP {
    pub(super) async fn new(mut pi: ProxyInputs, drain: Watch) -> Result<OutboundIP, Error> {
        let tunin = Tun::builder()
            .name("z_tun0")            // if name is empty, then it is set by kernel.
            .tap(false)          // false (default): TUN, true: TAP.
            .packet_info(false)  // false: IFF_NO_PI, default is true.
            .mtu(1350)
            .up()                // or set it up manually using `sudo ip link set <tun-name> up`.
            .try_build()         // or `.try_build_mq(queues)` for multi-queue support.
            .unwrap();

        info!(
            component="outbound ip",
            fd=?tunin.as_raw_fd(),
            addr=?tunin.address(),
            name=tunin.name(),
            "tun created",
        );
        Ok(OutboundIP {
            pi,
            tunin,
            drain,
        })
    }

    pub(super) async fn run(self) {
        tokio::time::sleep(Duration::from_secs(10)).await;
        info!("Attempting HBONE connections...");
        let mut hbones = std::collections::HashMap::<String, Upgraded>::new();
        let zt = self.pi.state.fetch_ztunnels().await;
        let pi = self.pi.clone();
        let me = zt.get(&self.pi.cfg.local_node.unwrap()).expect("found myself").clone();
        for (node, zt) in zt.into_iter() {
            let mut oc = OutboundConnection {
                pi: pi.clone(),
                id: TraceParent::new(),
            };
            let hbc = oc.hbone(outbound::Request{
                protocol: Default::default(),
                direction: outbound::Direction::Inbound,
                source: me.clone(),
                destination: SocketAddr::from((zt.workload_ips[0], 15008)),
                destination_workload: None,
                expected_identity: Some(zt.identity()),
                gateway: SocketAddr::from((zt.workload_ips[0], 15008)),
                request_type: outbound::RequestType::Direct,
            }).await.expect("hbone connect");
            info!("Established connection to ztunnel on {node}");
            hbones.insert(node, hbc);
        }
        let (mut reader, mut _writer) = tokio::io::split(self.tunin);

        let mut buf = [0u8; 1024];
        loop {
            let n = reader.read(&mut buf).await.unwrap();
            info!("tun reading {} bytes: {:?}", n, &buf[..n]);
            let ip = IpHeader::from_slice(&buf[..n]);
            match ip {
                // TODO: probably need the "rest" part
                Ok((IpHeader::Version4(h, _), _, _)) =>  {
                    let dst = IpAddr::from(h.destination);
                    let src = IpAddr::from(h.source);
                    info!("Read packet {src} -> {dst}");
                    let downstream_network_addr = NetworkAddress {
                        network: self.pi.cfg.network.clone(),
                        address: dst,
                    };
                    let Some(dst_wl) = pi.state.fetch_workload(&downstream_network_addr).await else {
                        error!("unknown destination {dst}");
                        continue
                    };
                    let Some(hbc) = hbones.get_mut(&dst_wl.node) else {
                        error!("unknown node {dst}");
                        continue
                    };
                    let res = hbc.write(&buf[..n]).await;
                    info!("Tun Wrote: {res:?}");
                }
                Ok(_) => {
                    error!("not an ip v4 header");
                },
                Err(e) => {
                    error!("not an ip header");
                },
            };
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
