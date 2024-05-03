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

use std::fmt::Write;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{atomic, Arc};
use std::time::Instant;

use prometheus_client::encoding::{EncodeLabelSet, EncodeLabelValue, LabelValueEncoder};
use prometheus_client::metrics::counter::{Atomic, Counter};
use prometheus_client::metrics::family::Family;
use prometheus_client::registry::Registry;

use tracing::event;

use crate::identity::Identity;
use crate::metrics::{DefaultedUnknown, DeferRecorder, Deferred, IncrementRecorder};

use crate::state::service::ServiceDescription;
use crate::state::workload::Workload;
use crate::strng::{RichStrng, Strng};

pub struct Metrics {
    pub connection_opens: Family<CommonTrafficLabels, Counter>,
    pub connection_close: Family<CommonTrafficLabels, Counter>,
    pub received_bytes: Family<CommonTrafficLabels, Counter>,
    pub sent_bytes: Family<CommonTrafficLabels, Counter>,

    // on-demand DNS is not a part of DNS proxy, but part of ztunnel proxy itself
    pub on_demand_dns: Family<OnDemandDnsLabels, Counter>,
    pub on_demand_dns_cache_misses: Family<OnDemandDnsLabels, Counter>,
}

impl Metrics {
    #[must_use = "metric will be dropped (and thus recorded) immediately if not assigned"]
    /// increment_defer is used to increment a metric now and another metric later once the MetricGuard is dropped
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let connection_open = ConnectionOpen {};
    /// // Record connection opened now
    /// let connection_close = self.metrics.increment_defer::<_, ConnectionClosed>(&connection_open);
    /// // Eventually, report connection closed
    /// drop(connection_close);
    /// ```
    pub fn increment_defer<'a, M1, M2>(
        &'a self,
        event: &'a M1,
    ) -> Deferred<'a, impl FnOnce(&'a Self), Self>
    where
        M1: Clone + 'a,
        M2: From<&'a M1> + 'a,
        Metrics: IncrementRecorder<M1> + IncrementRecorder<M2>,
    {
        self.increment(event);
        let m2: M2 = event.into();
        self.defer_record(move |metrics| {
            metrics.increment(&m2);
        })
    }
}

impl DeferRecorder for Metrics {}

#[derive(Clone, Copy, Default, Debug, Hash, PartialEq, Eq, EncodeLabelValue)]
pub enum Reporter {
    #[default]
    source,
    #[allow(dead_code)]
    destination,
}

#[derive(Clone, Copy, Default, Debug, Hash, PartialEq, Eq, EncodeLabelValue)]
pub enum RequestProtocol {
    #[default]
    tcp,
    #[allow(dead_code)]
    http,
}

#[derive(Default, Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub enum ResponseFlags {
    #[default]
    None,
    // connection denied due to policy
    AuthorizationPolicyDenied,
}

impl EncodeLabelValue for ResponseFlags {
    fn encode(&self, writer: &mut LabelValueEncoder) -> Result<(), std::fmt::Error> {
        match self {
            ResponseFlags::None => writer.write_str("-"),
            ResponseFlags::AuthorizationPolicyDenied => writer.write_str("DENY"),
        }
    }
}

#[derive(Default, Copy, Clone, Debug, Hash, PartialEq, Eq, EncodeLabelValue)]
pub enum SecurityPolicy {
    #[default]
    unknown,
    mutual_tls,
}

#[derive(Clone, Debug, Default)]
pub struct DerivedWorkload {
    pub workload_name: Option<Strng>,
    pub app: Option<Strng>,
    pub revision: Option<Strng>,
    pub namespace: Option<Strng>,
    pub identity: Option<Identity>,
    pub cluster_id: Option<Strng>,
}

#[derive(Clone)]
pub struct ConnectionOpen {
    pub reporter: Reporter,
    pub source: Option<Workload>,
    pub derived_source: Option<DerivedWorkload>,
    pub destination: Option<Workload>,
    pub destination_service: Option<ServiceDescription>,
    pub connection_security_policy: SecurityPolicy,
}

impl CommonTrafficLabels {
    fn new() -> Self {
        Default::default()
    }

    fn with_source(mut self, w: Option<&Workload>) -> Self {
        let Some(w) = w else { return self };
        self.source_workload = w.workload_name.clone().into();
        self.source_canonical_service = w.canonical_name.clone().into();
        self.source_canonical_revision = w.canonical_revision.clone().into();
        self.source_workload_namespace = w.namespace.clone().into();
        self.source_principal = w.identity().into();
        self.source_app = w.canonical_name.clone().into();
        self.source_version = w.canonical_revision.clone().into();
        self.source_cluster = w.cluster_id.to_string().into();
        self
    }

    fn with_derived_source(mut self, w: Option<&DerivedWorkload>) -> Self {
        let Some(w) = w else { return self };
        self.source_workload = w.workload_name.clone().into();
        self.source_canonical_service = w.app.clone().into();
        self.source_canonical_revision = w.revision.clone().into();
        self.source_workload_namespace = w.namespace.clone().into();
        self.source_principal = w.identity.clone().into();
        self.source_app = w.workload_name.clone().into();
        self.source_version = w.revision.clone().into();
        self.source_cluster = w.cluster_id.clone().into();
        self
    }

    fn with_destination(mut self, w: Option<&Workload>) -> Self {
        let Some(w) = w else { return self };
        self.destination_workload = w.workload_name.clone().into();
        self.destination_canonical_service = w.canonical_name.clone().into();
        self.destination_canonical_revision = w.canonical_revision.clone().into();
        self.destination_workload_namespace = w.namespace.clone().into();
        self.destination_principal = w.identity().into();
        self.destination_app = w.canonical_name.clone().into();
        self.destination_version = w.canonical_revision.clone().into();
        self.destination_cluster = w.cluster_id.to_string().into();
        self
    }

    fn with_destination_service(mut self, w: Option<&ServiceDescription>) -> Self {
        let Some(w) = w else { return self };
        self.destination_service = w.hostname.clone().into();
        self.destination_service_name = w.name.clone().into();
        self.destination_service_namespace = w.namespace.clone().into();
        self
    }
}

impl From<ConnectionOpen> for CommonTrafficLabels {
    fn from(c: ConnectionOpen) -> Self {
        CommonTrafficLabels {
            reporter: c.reporter,
            request_protocol: RequestProtocol::tcp,
            response_flags: ResponseFlags::None,
            connection_security_policy: c.connection_security_policy,
            ..CommonTrafficLabels::new()
                // Intentionally before with_source; source is more reliable
                .with_derived_source(c.derived_source.as_ref())
                .with_source(c.source.as_ref())
                .with_destination(c.destination.as_ref())
                .with_destination_service(c.destination_service.as_ref())
        }
    }
}

#[derive(Clone, Hash, Default, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct CommonTrafficLabels {
    reporter: Reporter,

    source_workload: DefaultedUnknown<RichStrng>,
    source_canonical_service: DefaultedUnknown<RichStrng>,
    source_canonical_revision: DefaultedUnknown<RichStrng>,
    source_workload_namespace: DefaultedUnknown<RichStrng>,
    source_principal: DefaultedUnknown<Identity>,
    source_app: DefaultedUnknown<RichStrng>,
    source_version: DefaultedUnknown<RichStrng>,
    source_cluster: DefaultedUnknown<RichStrng>,

    // TODO: never set
    destination_service: DefaultedUnknown<RichStrng>,
    destination_service_namespace: DefaultedUnknown<RichStrng>,
    destination_service_name: DefaultedUnknown<RichStrng>,

    destination_workload: DefaultedUnknown<RichStrng>,
    destination_canonical_service: DefaultedUnknown<RichStrng>,
    destination_canonical_revision: DefaultedUnknown<RichStrng>,
    destination_workload_namespace: DefaultedUnknown<RichStrng>,
    destination_principal: DefaultedUnknown<Identity>,
    destination_app: DefaultedUnknown<RichStrng>,
    destination_version: DefaultedUnknown<RichStrng>,
    destination_cluster: DefaultedUnknown<RichStrng>,

    request_protocol: RequestProtocol,
    response_flags: ResponseFlags,
    connection_security_policy: SecurityPolicy,
}

#[derive(Clone, Hash, Default, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct OnDemandDnsLabels {
    // on-demand DNS client information is just nice-to-have
    source_workload: DefaultedUnknown<RichStrng>,
    source_canonical_service: DefaultedUnknown<RichStrng>,
    source_canonical_revision: DefaultedUnknown<RichStrng>,
    source_workload_namespace: DefaultedUnknown<RichStrng>,
    source_principal: DefaultedUnknown<Identity>,
    source_app: DefaultedUnknown<RichStrng>,
    source_version: DefaultedUnknown<RichStrng>,
    source_cluster: DefaultedUnknown<RichStrng>,

    // on-demand DNS is resolved per hostname, so this is the most interesting part
    hostname: DefaultedUnknown<RichStrng>,
}

impl OnDemandDnsLabels {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn with_source(mut self, w: &Workload) -> Self {
        self.source_workload = w.workload_name.clone().into();
        self.source_canonical_service = w.canonical_name.clone().into();
        self.source_canonical_revision = w.canonical_revision.clone().into();
        self.source_workload_namespace = w.namespace.clone().into();
        self.source_principal = w.identity().into();
        self.source_app = w.canonical_name.clone().into();
        self.source_version = w.canonical_revision.clone().into();
        self.source_cluster = w.cluster_id.to_string().into();
        self
    }

    pub fn with_destination(mut self, w: &Workload) -> Self {
        self.hostname = w.hostname.clone().into();
        self
    }
}

impl Metrics {
    pub fn new(registry: &mut Registry) -> Self {
        let connection_opens = Family::default();
        registry.register(
            "tcp_connections_opened",
            "The total number of TCP connections opened",
            connection_opens.clone(),
        );
        let connection_close = Family::default();
        registry.register(
            "tcp_connections_closed",
            "The total number of TCP connections closed",
            connection_close.clone(),
        );

        let received_bytes = Family::default();
        registry.register(
            "tcp_received_bytes",
            "The size of total bytes received during request in case of a TCP connection",
            received_bytes.clone(),
        );
        let sent_bytes = Family::default();
        registry.register(
            "tcp_sent_bytes",
            "The size of total bytes sent during response in case of a TCP connection",
            sent_bytes.clone(),
        );
        let on_demand_dns = Family::default();
        registry.register(
            "on_demand_dns",
            "The total number of requests that used on-demand DNS (unstable)",
            on_demand_dns.clone(),
        );
        let on_demand_dns_cache_misses = Family::default();
        registry.register(
            "on_demand_dns_cache_misses",
            "The total number of cache misses for requests on-demand DNS (unstable)",
            on_demand_dns_cache_misses.clone(),
        );

        Self {
            connection_opens,
            connection_close,
            received_bytes,
            sent_bytes,
            on_demand_dns,
            on_demand_dns_cache_misses,
        }
    }
}

/// ConnectionResult abstracts recording a metric and emitting an access log upon a connection completion
pub struct ConnectionResult {
    // Src address and name
    src: (SocketAddr, Option<RichStrng>),
    // Dst address and name
    dst: (SocketAddr, Option<RichStrng>),
    hbone_target: Option<SocketAddr>,
    start: Instant,

    // TODO: storing CommonTrafficLabels adds ~600 bytes retained throughout a connection life time.
    // We can pre-fetch the metrics we need at initialization instead of storing this, then keep a more
    // efficient representation for the fields we need to log. Ideally, this would even be optional
    // in case logs were disabled.
    tl: CommonTrafficLabels,
    metrics: Arc<Metrics>,

    // sent records the number of bytes sent on this connection
    sent: AtomicU64,
    // sent_metric records the number of bytes sent on this connection to the aggregated metric counter
    sent_metric: Counter,
    // recv records the number of bytes received on this connection
    recv: AtomicU64,
    // recv_metric records the number of bytes received on this connection to the aggregated metric counter
    recv_metric: Counter,
}

// log_early_deny allows logging a connection is denied before we have enough information to emit proper
// access logs/metrics
pub fn log_early_deny<E: std::error::Error>(
    src: SocketAddr,
    dst: SocketAddr,
    reporter: Reporter,
    err: E,
) {
    event!(
            target: "access",
            parent: None,
            tracing::Level::WARN,

            src.addr = %src,
            dst.addr = %dst,

            direction = if reporter == Reporter::source {
                "outbound"
            } else {
                "inbound"
            },

            error = %err,

            "connection failed"
    );
}

macro_rules! access_log {
    ($res:expr, $($fields:tt)*) => {
        let err = $res.as_ref().err().map(|e| e.to_string());
        match $res {
            Ok(_) => {
                event!(
                    target: "access",
                    parent: None,
                    tracing::Level::INFO,
                    $($fields)*
                    "connection complete"
                );
            }
            Err(_) => {
                event!(
                    target: "access",
                    parent: None,
                    tracing::Level::ERROR,
                    $($fields)*
                    error = err,
                    "connection complete"
                );
            }
        }
    };
}
impl ConnectionResult {
    pub fn new(
        src: SocketAddr,
        dst: SocketAddr,
        // If using hbone, the inner HBONE address
        // That is, dst is the L4 address, while is the :authority.
        hbone_target: Option<SocketAddr>,
        start: Instant,
        conn: ConnectionOpen,
        metrics: Arc<Metrics>,
    ) -> Self {
        // for src and dest, try to get pod name but fall back to "canonical service"
        let mut src = (src, conn.source.as_ref().map(|wl| wl.name.clone().into()));
        let mut dst = (
            dst,
            conn.destination.as_ref().map(|wl| wl.name.clone().into()),
        );
        let tl = CommonTrafficLabels::from(conn);
        metrics.connection_opens.get_or_create(&tl).inc();

        let mtls = tl.connection_security_policy == SecurityPolicy::mutual_tls;

        src.1 = src.1.or(tl.source_canonical_service.clone().inner());
        dst.1 = dst.1.or(tl.destination_canonical_service.clone().inner());
        event!(
            target: "access",
            parent: None,
            tracing::Level::DEBUG,

            src.addr = %src.0,
            src.workload = src.1.as_deref().map(display),
            src.namespace = tl.source_workload_namespace.display(),
            src.identity = tl.source_principal.as_ref().filter(|_| mtls).map(|id| id.to_string()),

            dst.addr = %dst.0,
            dst.hbone_addr = hbone_target.map(display),
            dst.workload = dst.1.as_deref().map(display),
            dst.namespace = tl.destination_canonical_service.display(),
            dst.identity = tl.destination_principal.as_ref().filter(|_| mtls).map(|id| id.to_string()),

            direction = if tl.reporter == Reporter::source {
                "outbound"
            } else {
                "inbound"
            },

            "connection opened"
        );
        // Grab the metrics with our labels now, so we don't need to fetch them each time.
        // The inner metric is an Arc so clone is fine/cheap.
        // With the raw Counter, we increment is a simple atomic add operation (~1ns).
        // Fetching the metric itself is ~300ns; fast, but we call it on each read/write so it would
        // add up.
        let sent_metric = metrics.sent_bytes.get_or_create(&tl).clone();
        let recv_metric = metrics.received_bytes.get_or_create(&tl).clone();
        let sent = atomic::AtomicU64::new(0);
        let recv = atomic::AtomicU64::new(0);
        Self {
            src,
            dst,
            hbone_target,
            start,
            tl,
            metrics,

            sent,
            sent_metric,
            recv,
            recv_metric,
        }
    }

    pub fn increment_send(&self, res: u64) {
        self.sent.inc_by(res);
        self.sent_metric.inc_by(res);
    }

    pub fn increment_recv(&self, res: u64) {
        self.recv.inc_by(res);
        self.recv_metric.inc_by(res);
    }

    pub fn record_with_flag<E: std::error::Error>(
        mut self,
        res: Result<(), E>,
        flag: ResponseFlags,
    ) {
        self.tl.response_flags = flag;
        self.record(res)
    }

    // Record our final result.
    // Ideally, we would save and report from the increment_ functions instead of requiring a report here.
    pub fn record<E: std::error::Error>(&self, res: Result<(), E>) {
        let tl = &self.tl;

        // Unconditionally record the connection was closed
        self.metrics.connection_close.get_or_create(tl).inc();

        // Unconditionally write out an access log
        let mtls = tl.connection_security_policy == SecurityPolicy::mutual_tls;
        let bytes = (
            self.recv.load(Ordering::SeqCst),
            self.sent.load(Ordering::SeqCst),
        );
        let dur = format!("{}ms", self.start.elapsed().as_millis());

        // We use our own macro to allow setting the level dynamically
        access_log!(
            res,

            src.addr = %self.src.0,
            src.workload = self.src.1.as_deref().map(display),
            src.namespace = tl.source_workload_namespace.display(),
            src.identity = tl.source_principal.as_ref().filter(|_| mtls).map(|id| id.to_string()),

            dst.addr = %self.dst.0,
            dst.hbone_addr = self.hbone_target.map(display),
            dst.service = tl.destination_service.display(),
            dst.workload = self.dst.1.as_deref().map(display),
            dst.namespace = tl.destination_canonical_service.display(),
            dst.identity = tl.destination_principal.as_ref().filter(|_| mtls).map(|id| id.to_string()),

            direction = if tl.reporter == Reporter::source {
                "outbound"
            } else {
                "inbound"
            },

            // Istio flips the metric for source: https://github.com/istio/istio/issues/32399
            // Unflip for logs
            bytes_sent = if tl.reporter == Reporter::source {bytes.0} else {bytes.1},
            bytes_recv = if tl.reporter == Reporter::source {bytes.1} else {bytes.0},
            duration = dur,
        );
    }
}
