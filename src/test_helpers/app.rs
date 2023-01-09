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

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::ops::Deref;
use std::time::Duration;

use hyper::{body, Body, Client, Method, Request, Response};
use prometheus_parse::Scrape;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};
use tracing::info;

use crate::identity::mock::MockCaClient;
use crate::identity::SecretManager;
use crate::config::Config;
use crate::test_helpers::{netns, TEST_WORKLOAD_SOURCE};
use crate::*;

use super::helpers::*;

#[derive(Clone)]
pub struct TestApp {
    pub admin_address: SocketAddr,
    pub readiness_address: SocketAddr,
    pub proxy_addresses: proxy::Addresses,
    pub cert_manager: SecretManager<MockCaClient>,
}

pub async fn with_app<F, Fut, FO>(cfg: config::Config, f: F)
where
    F: Fn(TestApp) -> Fut,
    Fut: Future<Output = FO>,
{
    initialize_telemetry();
    let cert_manager = MockCaClient::new(Duration::from_secs(10));
    let app = app::build_with_cert(cfg, cert_manager.clone())
        .await
        .unwrap();
    let shutdown = app.shutdown.trigger().clone();

    let ta = TestApp {
        admin_address: app.admin_address,
        proxy_addresses: app.proxy_addresses,
        readiness_address: app.readiness_address,
        cert_manager,
    };
    let run_and_shutdown = async {
        ta.ready().await;
        f(ta).await;
        shutdown.shutdown_now().await;
    };
    let (app, _shutdown) = tokio::join!(app.wait_termination(), run_and_shutdown);
    app.expect("app exits without error");
}

impl TestApp {
    pub async fn admin_request(&self, path: &str) -> hyper::Result<Response<Body>> {
        let req = Request::builder()
            .method(Method::GET)
            .uri(format!(
                "http://localhost:{}/{path}",
                self.admin_address.port()
            ))
            .header("content-type", "application/json")
            .body(Body::default())
            .unwrap();
        let client = Client::new();
        client.request(req).await
    }

    pub async fn readiness_request(&self) -> anyhow::Result<()> {
        let req = Request::builder()
            .method(Method::GET)
            .uri(format!(
                "http://localhost:{}/healthz/ready",
                self.readiness_address
            ))
            .body(Body::default())
            .unwrap();
        let client = Client::new();
        let resp = client
            .request(req)
            .await
            .expect("error sending ready healthcheck request");
        match resp.status() {
            hyper::StatusCode::OK => Ok(()),
            other => Err(anyhow::anyhow!(
                "non-200 status code from readiness request: received {}",
                other
            )),
        }
    }

    pub async fn admin_request_string(&self, path: &str) -> String {
        let body = self.admin_request(path).await.expect("request").into_body();
        let body = body::to_bytes(body).await.expect("read read body");
        let s = std::str::from_utf8(&body).expect("to string");
        s.to_string()
    }

    pub async fn metrics(&self) -> ParsedMetrics {
        let body = self.admin_request_string("metrics").await;
        let iter = body
            .lines()
            .into_iter()
            .map(|x| Ok::<_, io::Error>(x.to_string()));
        let scrape = prometheus_parse::Scrape::parse(iter).unwrap();
        ParsedMetrics { scrape }
    }

    pub async fn ready(&self) {
        let mut last_err: anyhow::Result<()> = Ok(());
        for _ in 0..200 {
            last_err = self.readiness_request().await;
            if last_err.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("failed to get ready (last: {last_err:?})");
    }

    pub async fn socks5_connect(&self, addr: SocketAddr) -> TcpStream {
        // Always use IPv4 address. In theory, we can resolve `localhost` to pick to support any machine
        // However, we need to make sure the WorkloadStore knows about both families then.
        let socks_addr = with_ip(
            self.proxy_addresses.socks5,
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        );

        // Set source IP to TEST_WORKLOAD_SOURCE
        let socket = TcpSocket::new_v4().unwrap();
        socket
            .bind(SocketAddr::from((
                TEST_WORKLOAD_SOURCE.parse::<IpAddr>().unwrap(),
                0,
            )))
            .unwrap();

        let mut stream = socket.connect(socks_addr).await.expect("must connect");
        stream.set_nodelay(true).unwrap();

        let addr_type = if addr.ip().is_ipv4() { 0x01u8 } else { 0x04u8 };
        stream
            .write_all(&[
                0x05u8, // socks5
                0x1u8,  // 1 auth method
                0x0u8,  // unauthenticated auth method
            ])
            .await
            .unwrap();
        let mut auth = [0u8; 2];
        stream.read_exact(&mut auth).await.unwrap();

        let mut cmd = vec![
            0x05u8, // socks5
            0x1u8,  // establish tcp stream
            0x0u8,  // RSV
            addr_type,
        ];
        match socket::to_canonical(addr).ip() {
            IpAddr::V6(ip) => cmd.extend_from_slice(&ip.octets()),
            IpAddr::V4(ip) => cmd.extend_from_slice(&ip.octets()),
        };
        cmd.extend_from_slice(&addr.port().to_be_bytes());
        stream.write_all(&cmd).await.unwrap();

        // We don't care about response but need to clear out the stream
        let mut resp = [0u8; 10];
        stream.read_exact(&mut resp).await.unwrap();

        stream
    }
}

pub struct Ztunnel {
    cfg: Config,
}

impl Ztunnel {
    pub fn new() -> Self {
        Self {
            cfg: Config {
                xds_address: None,
                fake_ca: true,
                local_xds_config: None,
                local_node: Some("local".to_string()),
                ..config::parse_config().unwrap()
            },
        }
    }

    pub async fn run_in_namespace(self, ready: netns::Ready) -> anyhow::Result<()> {
        info!("Running ztunnel");
        let cert_manager = identity::mock::MockCaClient::new(Duration::from_secs(10));
        let app = app::build_with_cert(self.cfg, cert_manager).await.unwrap();

        let ta = TestApp {
            admin_address: app.admin_address,
            proxy_addresses: app.proxy_addresses,
            readiness_address: app.readiness_address,
        };
        info!("initialized");
        ta.ready().await;
        info!("ready");
        ready.set_ready();

        app.wait_termination().await
    }
}

pub struct ParsedMetrics {
    scrape: Scrape,
}

impl ParsedMetrics {
    pub fn query(
        &self,
        metric: &str,
        labels: HashMap<String, String>,
    ) -> Option<Vec<&prometheus_parse::Sample>> {
        if !self
            .scrape
            .docs
            .contains_key(metric.strip_suffix("_total").unwrap_or(metric))
        {
            return None;
        }
        Some(
            self.scrape
                .samples
                .iter()
                .filter(|s| s.metric == metric)
                .filter(|s| superset_of(s.labels.deref(), &labels))
                .collect(),
        )
    }

    pub fn query_sum(&self, metric: &str, labels: HashMap<String, String>) -> u64 {
        let res = self.query(metric, labels);
        res.map(|streams| {
            streams
                .into_iter()
                .map(|sample| {
                    match sample.value {
                        prometheus_parse::Value::Counter(f) => f,
                        // TOOD(https://github.com/ccakes/prometheus-parse-rs/issues/5) remove this
                        prometheus_parse::Value::Untyped(f) => f,
                        _ => panic!("query_sum({metric}) must be a counter"),
                    }
                })
                .map(|f| f as u64)
                .sum()
        })
        .unwrap_or(0)
    }
    pub fn dump(&self) -> String {
        format!("{:?}", self.scrape.samples)
    }
}

fn superset_of(base: &HashMap<String, String>, check: &HashMap<String, String>) -> bool {
    for (k, v) in check {
        if base.get(k) != Some(v) {
            return false;
        }
    }
    true
}
