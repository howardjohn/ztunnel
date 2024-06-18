// Copyright Istio Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use drain::Watch;
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::time::timeout;

use tracing::{debug, error, info, trace, warn, Instrument};

use crate::config::ProxyMode;

use crate::proxy::metrics::Reporter;
use crate::proxy::Error;
use crate::proxy::{metrics, util, ProxyInputs};
use crate::state::workload::NetworkAddress;
use crate::{assertions, copy, rbac, strng};
use crate::{proxy, socket};

pub(super) struct InboundPassthrough {
    listener: socket::Listener,
    pi: Arc<ProxyInputs>,
    drain: Watch,
    enable_orig_src: bool,
}

impl InboundPassthrough {
    pub(super) async fn new(
        pi: Arc<ProxyInputs>,
        drain: Watch,
    ) -> Result<InboundPassthrough, Error> {
        let listener = pi
            .socket_factory
            .tcp_bind(pi.cfg.inbound_plaintext_addr)
            .map_err(|e| Error::Bind(pi.cfg.inbound_plaintext_addr, e))?;

        let enable_orig_src = super::maybe_set_transparent(&pi, &listener)?;

        info!(
            address=%listener.local_addr(),
            component="inbound plaintext",
            transparent=enable_orig_src,
            "listener established",
        );
        Ok(InboundPassthrough {
            listener,
            pi,
            drain,
            enable_orig_src,
        })
    }

    pub(super) async fn run(self) {
        let (sub_drain_signal, sub_drain) = drain::channel();
        let deadline = self.pi.cfg.self_termination_deadline;
        let (trigger_force_shutdown, force_shutdown) = watch::channel(());
        let accept = async move {
            loop {
                // Asynchronously wait for an inbound socket.
                let socket = self.listener.accept().await;
                let start = Instant::now();
                let mut force_shutdown = force_shutdown.clone();
                let drain = sub_drain.clone();
                let pi = self.pi.clone();
                match socket {
                    Ok((stream, remote)) => {
                        let serve_client = async move {
                            debug!(dur=?start.elapsed(), "inbound passthrough connection started");
                            // Since this task is spawned, make sure we are guaranteed to terminate
                            tokio::select! {
                                _ = force_shutdown.changed() => {
                                    debug!("inbound passthrough connection forcefully terminated signaled");
                                }
                                _ = Self::proxy_inbound_plaintext(
                            pi, // pi cloned above; OK to move
                            socket::to_canonical(remote),
                            stream,
                            self.enable_orig_src,
                        ) => {}
                            }
                            // Mark we are done with the connection, so drain can complete
                            drop(drain);
                            debug!(dur=?start.elapsed(), "inbound passthrough connection completed");
                        }
                        .in_current_span();

                        assertions::size_between_ref(1500, 2500, &serve_client);
                        tokio::spawn(serve_client);
                    }
                    Err(e) => {
                        if util::is_runtime_shutdown(&e) {
                            return;
                        }
                        error!("Failed TCP handshake {}", e);
                    }
                }
            }
        }
        .in_current_span();

        // Stop accepting once we drain.
        // We will then allow connections up to `deadline` to terminate on their own.
        // After that, they will be forcefully terminated.
        tokio::select! {
            res = accept => { res }
            res = self.drain.signaled() => {
                debug!("inbound passthrough drained, waiting {:?} for any outbound connections to close", deadline);
                if let Err(e) = timeout(deadline, sub_drain_signal.drain()).await {
                    // Not all connections completed within time, we will force shut them down
                    warn!("drain duration expired with pending connections, forcefully shutting down: {e:?}");
                }
                // Trigger force shutdown. In theory, this is only needed in the timeout case. However,
                // it doesn't hurt to always trigger it.
                let _ = trigger_force_shutdown.send(());

                info!("outbound drain complete");
                drop(res);
            }
        }
    }

    async fn proxy_inbound_plaintext(
        pi: Arc<ProxyInputs>,
        source_addr: SocketAddr,
        inbound_stream: TcpStream,
        enable_orig_src: bool,
    ) {
        let start = Instant::now();
        let dest_addr = socket::orig_dst_addr_or_default(&inbound_stream);
        // Check if it is an illegal call to ourself, which could trampoline to illegal addresses or
        // lead to infinite loops
        let illegal_call = if pi.cfg.inpod_enabled {
            // User sent a request to pod:15006. This would forward to pod:15006 infinitely
            pi.cfg.illegal_ports.contains(&dest_addr.port())
        } else {
            // User sent a request to the ztunnel directly. This isn't allowed
            pi.cfg.proxy_mode == ProxyMode::Shared && Some(dest_addr.ip()) == pi.cfg.local_ip
        };
        if illegal_call {
            metrics::log_early_deny(
                source_addr,
                dest_addr,
                Reporter::destination,
                Error::SelfCall,
            );
            return;
        }
        let network_addr = NetworkAddress {
            network: strng::new(&pi.cfg.network), // inbound request must be on our network
            address: dest_addr.ip(),
        };
        let Some((upstream, upstream_service)) =
            pi.state.fetch_workload_services(&network_addr).await
        else {
            metrics::log_early_deny(
                source_addr,
                dest_addr,
                Reporter::destination,
                Error::UnknownDestination(dest_addr.ip()),
            );
            return;
        };

        let rbac_ctx = crate::state::ProxyRbacContext {
            conn: rbac::Connection {
                src_identity: None,
                src: source_addr,
                // inbound request must be on our network since this is passthrough
                // rather than HBONE, which can be tunneled across networks through gateways.
                // by definition, without the gateway our source must be on our network.
                dst_network: strng::new(&pi.cfg.network),
                dst: dest_addr,
            },
            dest_workload_info: pi.proxy_workload_info.clone(),
        };

        // Find source info. We can lookup by XDS or from connection attributes
        let source_workload = {
            let network_addr_srcip = NetworkAddress {
                // inbound request must be on our network since this is passthrough
                // rather than HBONE, which can be tunneled across networks through gateways.
                // by definition, without the gateway our source must be on our network.
                network: pi.cfg.network.as_str().into(),
                address: source_addr.ip(),
            };
            pi.state.fetch_workload(&network_addr_srcip).await
        };
        let derived_source = metrics::DerivedWorkload {
            identity: rbac_ctx.conn.src_identity.clone(),
            ..Default::default()
        };
        let ds = proxy::guess_inbound_service(&rbac_ctx.conn, &None, upstream_service, &upstream);
        let result_tracker = Arc::new(metrics::ConnectionResult::new(
            source_addr,
            dest_addr,
            None,
            start,
            metrics::ConnectionOpen {
                reporter: Reporter::destination,
                source: source_workload,
                derived_source: Some(derived_source),
                destination: Some(upstream),
                connection_security_policy: metrics::SecurityPolicy::unknown,
                destination_service: ds,
            },
            pi.metrics.clone(),
        ));

        let conn_guard = match pi
            .connection_manager
            .assert_rbac(&pi.state, &rbac_ctx, None)
            .await
        {
            Ok(cg) => cg,
            Err(e) => {
                Arc::into_inner(result_tracker)
                    .expect("arc is not shared yet")
                    .record_with_flag(Err(e), metrics::ResponseFlags::AuthorizationPolicyDenied);
                return;
            }
        };

        let orig_src = if enable_orig_src {
            Some(source_addr.ip())
        } else {
            None
        };

        let send = async {
            let result_tracker = result_tracker.clone();
            trace!(%source_addr, %dest_addr, component="inbound plaintext", "connecting...");

            let outbound = super::freebind_connect(orig_src, dest_addr, pi.socket_factory.as_ref())
                .await
                .map_err(Error::ConnectionFailed)?;

            trace!(%source_addr, destination=%dest_addr, component="inbound plaintext", "connected");
            copy::copy_bidirectional(
                copy::TcpStreamSplitter(inbound_stream),
                copy::TcpStreamSplitter(outbound),
                &result_tracker,
            )
            .await
        };

        let res = conn_guard.handle_connection(send).await;
        result_tracker.record(res);
    }
}
