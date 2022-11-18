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

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use clap_verbosity_flag::ErrorLevel;
use hyper::{Body, Client, Method, Request};
use once_cell::sync::Lazy;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time;
use tracing::error;

use ztunnel::test_helpers::app as testapp;
use ztunnel::test_helpers::*;
use ztunnel::*;

fn test_config() -> config::Config {
    config::Config {
        xds_address: None,
        local_xds_path: Some("examples/localhost.yaml".to_string()),
        socks5_addr: SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
        inbound_addr: SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
        admin_addr: SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
        outbound_addr: SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
        inbound_plaintext_addr: SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
        ..Default::default()
    }
}

#[tokio::test]
async fn test_shutdown_lifecycle() {
    helpers::initialize_telemetry();

    let app = ztunnel::app::build(test_config()).await.unwrap();

    let shutdown = app.shutdown.trigger().clone();
    let (app, _shutdown) = tokio::join!(
        time::timeout(Duration::from_secs(1), app.spawn()),
        shutdown.shutdown_now()
    );
    app.expect("app shuts down")
        .expect("app exits without error")
}

#[tokio::test]
async fn test_quit_lifecycle() {
    helpers::initialize_telemetry();

    let app = ztunnel::app::build(test_config()).await.unwrap();
    let addr = app.admin_address;

    let (app, _shutdown) = tokio::join!(
        time::timeout(Duration::from_secs(1), app.spawn()),
        admin_shutdown(addr)
    );
    app.expect("app shuts down")
        .expect("app exits without error");
}

#[tokio::test]
async fn test_healthz() {
    testapp::with_app(test_config(), |app| async move {
        let resp = app.admin_request("healthz/ready").await;
        assert_eq!(resp.status(), hyper::StatusCode::OK);
    })
    .await;
}

#[track_caller]
async fn run_request_test(target: &str) {
    // Test a round trip outbound call (via socks5)
    let echo = echo::TestServer::new().await;
    let echo_addr = echo.address();
    tokio::spawn(echo.run());
    testapp::with_app(test_config(), |app| async move {
        let dst = helpers::with_ip(echo_addr, target.parse().unwrap());
        let mut stream = app.socks5_connect(dst).await;

        const BODY: &[u8] = "hello world".as_bytes();
        stream.write_all(BODY).await.unwrap();

        // Echo server should reply back with the same data
        let mut buf: [u8; BODY.len()] = [0; BODY.len()];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(BODY, buf);
    })
    .await;
}

#[tokio::test]
async fn test_hbone_request() {
    run_request_test("127.0.0.1").await;
}

#[tokio::test]
async fn test_tcp_request() {
    run_request_test("127.0.0.2").await;
}

/// admin_shutdown triggers a shutdown - from the admin server
async fn admin_shutdown(addr: SocketAddr) {
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("http://localhost:{}/quitquitquit", addr.port()))
        .header("content-type", "application/json")
        .body(Body::default())
        .unwrap();
    let client = Client::new();
    let resp = client.request(req).await.expect("admin shutdown request");
    assert_eq!(resp.status(), hyper::StatusCode::OK);
}

#[tokio::test]
async fn test_throughput() {
    helpers::initialize_telemetry();
    initialize_netperf().await;

    run_client(NETPERF_PORT).await;
}

const NETPERF_PORT: u16 = 17559;
static THROUGHPUT_SERVER_INIT: Lazy<()> = Lazy::new(|| {
    spawn_netperf_server(NETPERF_PORT);
});

async fn wait_for_port(port: u16) {
    for _ in 0..100 {
        if let Ok(_) = TcpStream::connect(format!("127.0.0.1:{}", port)).await {
            return
        }
        time::sleep(Duration::from_millis(10)).await;
    }
}

async fn initialize_netperf() {
    Lazy::force(&THROUGHPUT_SERVER_INIT);
    wait_for_port(NETPERF_PORT).await;
}

fn spawn_netperf_server(port: u16) {
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(run_netperf_server(port));
    });
}

async fn run_netperf_server(port: u16) {
    netperf::server::run_server(netperf_opts(port))
        .await
        .unwrap()
}

fn netperf_opts(port: u16) -> netperf::common::opts::Opts {
    use netperf::common::opts::ClientOpts;
    use netperf::common::opts::CommonOpts;
    use netperf::common::opts::Opts;
    use netperf::common::opts::ServerOpts;
    Opts {
        server_opts: ServerOpts {
            server: true,
            one_off: true,
        },
        client_opts: ClientOpts {
            client: Some("localhost".to_string()),
            length: None,
            socket_buffers: None,
            time: 2,
            parallel: 1,
            bidir: false,
            reverse: false,
            no_delay: false,
        },
        common_opts: CommonOpts { port, interval: 1 },
        verbose: clap_verbosity_flag::Verbosity::<ErrorLevel>::new(0, 0),
    }
}
async fn run_client(port: u16) {
    use netperf::common::opts::ClientOpts;
    use netperf::common::opts::CommonOpts;
    use netperf::common::opts::Opts;
    use netperf::common::opts::ServerOpts;
    netperf::client::run_client(netperf_opts(port))
        .await
        .unwrap();
    error!("client done");
}
