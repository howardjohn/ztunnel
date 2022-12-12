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

use std::fmt::Debug;
use std::future::Future;
use std::net::SocketAddr;
use std::ops::Add;
use std::str::FromStr;
use std::time::{Duration, SystemTime};

use hyper::{Body, Client, Method, Request};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::time;
use tokio::time::timeout;
use tracing::trace;

use ztunnel::test_helpers::app as testapp;
use ztunnel::test_helpers::app::TestApp;
use ztunnel::test_helpers::*;

#[tokio::test]
async fn test_shutdown_lifecycle() {
    helpers::initialize_telemetry();

    let app = ztunnel::app::build(test_config()).await.unwrap();

    let shutdown = app.shutdown.trigger().clone();
    let (app, _shutdown) = tokio::join!(
        time::timeout(Duration::from_secs(5), app.wait_termination()),
        shutdown.shutdown_now()
    );
    app.expect("app shuts down")
        .expect("app exits without error")
}

// Check that port conflicts on any address results in the app failing instead of silently failing
async fn test_bind_conflict<F: FnOnce(&mut ztunnel::config::Config) -> &mut SocketAddr>(f: F) {
    helpers::initialize_telemetry();
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut cfg = test_config();
    let sa = f(&mut cfg);
    *sa = l.local_addr().unwrap();

    assert!(ztunnel::app::build(cfg).await.is_err());
}

#[tokio::test]
async fn test_conflicting_bind_error_inbound() {
    test_bind_conflict(|c| &mut c.inbound_addr).await;
}

#[tokio::test]
async fn test_conflicting_bind_error_inbound_plaintext() {
    test_bind_conflict(|c| &mut c.inbound_plaintext_addr).await;
}

#[tokio::test]
async fn test_conflicting_bind_error_outbound() {
    test_bind_conflict(|c| &mut c.outbound_addr).await;
}

#[tokio::test]
async fn test_conflicting_bind_error_socks5() {
    test_bind_conflict(|c| &mut c.socks5_addr).await;
}

#[tokio::test]
async fn test_conflicting_bind_error_admin() {
    test_bind_conflict(|c| &mut c.admin_addr).await;
}

#[tokio::test]
async fn test_shutdown_drain() {
    helpers::initialize_telemetry();

    let app = ztunnel::app::build(test_config()).await.unwrap();
    let ta = TestApp {
        admin_address: app.admin_address,
        proxy_addresses: app.proxy_addresses,
    };
    let echo = tcp::TestServer::new(tcp::Mode::ReadWrite).await;
    let echo_addr = echo.address();
    tokio::spawn(echo.run());
    let shutdown = app.shutdown.trigger().clone();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        app.wait_termination().await.unwrap();
        // Notify we shut down
        shutdown_tx.send(()).unwrap();
    });
    // we shouldn't be shutdown yet
    assert!(shutdown_rx.try_recv().is_err());
    let dst = helpers::with_ip(echo_addr, "127.0.0.1".parse().unwrap());
    let mut stream = ta.socks5_connect(dst).await;
    read_write_stream(&mut stream).await;
    // Since we are connected, the app shouldn't shutdown
    shutdown.shutdown_now().await;
    assert!(shutdown_rx.try_recv().is_err());
    // Give it some time, make sure we still aren't shut down
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(shutdown_rx.try_recv().is_err());

    // Now close the stream, we should shutdown
    drop(stream);
    timeout(Duration::from_secs(1), shutdown_rx)
        .await
        .expect("app should shutdown")
        .unwrap();
}

#[tokio::test]
async fn test_shutdown_forced_drain() {
    helpers::initialize_telemetry();

    let mut cfg = test_config();
    cfg.termination_grace_period = Duration::from_millis(10);
    let app = ztunnel::app::build(cfg).await.unwrap();
    let ta = TestApp {
        admin_address: app.admin_address,
        proxy_addresses: app.proxy_addresses,
    };
    let echo = tcp::TestServer::new(tcp::Mode::ReadWrite).await;
    let echo_addr = echo.address();
    tokio::spawn(echo.run());
    let shutdown = app.shutdown.trigger().clone();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        app.wait_termination().await.unwrap();
        // Notify we shut down
        shutdown_tx.send(()).unwrap();
    });
    // we shouldn't be shutdown yet
    assert!(shutdown_rx.try_recv().is_err());
    let dst = helpers::with_ip(echo_addr, "127.0.0.1".parse().unwrap());
    let mut stream = ta.socks5_connect(dst).await;
    const BODY: &[u8] = "hello world".as_bytes();
    stream.write_all(BODY).await.unwrap();

    // Since we are connected, the app shouldn't shutdown... but it will hit the max time and forcefully exit
    shutdown.shutdown_now().await;
    // It shouldn't shut down immediately, but checking that will cause flakes. Just make sure it exits within 1s.
    timeout(Duration::from_secs(1), shutdown_rx)
        .await
        .expect("app should shutdown")
        .unwrap();
}

#[tokio::test]
async fn test_quit_lifecycle() {
    helpers::initialize_telemetry();

    let app = ztunnel::app::build(test_config()).await.unwrap();
    let addr = app.admin_address;

    let (app, _shutdown) = tokio::join!(
        time::timeout(Duration::from_secs(5), app.wait_termination()),
        admin_shutdown(addr)
    );
    app.expect("app shuts down")
        .expect("app exits without error");
}

#[track_caller]
async fn run_request_test(target: &str) {
    // Test a round trip outbound call (via socks5)
    let echo = tcp::TestServer::new(tcp::Mode::ReadWrite).await;
    let echo_addr = echo.address();
    tokio::spawn(echo.run());
    testapp::with_app(test_config_with_port(echo_addr.port()), |app| async move {
        let dst = SocketAddr::from_str(target)
            .unwrap_or_else(|_| helpers::with_ip(echo_addr, target.parse().unwrap()));
        let mut stream = app.socks5_connect(dst).await;
        read_write_stream(&mut stream).await;
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

#[tokio::test]
async fn test_vip_request() {
    run_request_test("127.10.0.1:80").await;
}

#[tokio::test]
async fn test_stats_exist() {
    testapp::with_app(test_config(), |app| async move {
        let metrics = app.metrics().await;
        for metric in &[
            ("istio_build"),
            ("istio_connection_terminations"),
            ("istio_tcp_connections_opened"),
            ("istio_tcp_connections_closed"),
        ] {
            assert!(
                metrics.query(metric, Default::default()).is_some(),
                "expected metric {}",
                metric
            );
        }
    })
    .await;
}

#[tokio::test]
async fn test_tcp_metrics() {
    // Test a round trip outbound call (via socks5)
    let echo = tcp::TestServer::new(tcp::Mode::ReadWrite).await;
    let echo_addr = echo.address();
    tokio::spawn(echo.run());
    testapp::with_app(test_config(), |app| async move {
        let dst = helpers::with_ip(echo_addr, "127.0.0.2".parse().unwrap());
        let mut stream = app.socks5_connect(dst).await;
        read_write_stream(&mut stream).await;

        // We should have 1 open connection but 0 closed connections
        let metrics = app.metrics().await;
        assert_eq!(
            metrics.query_sum("istio_tcp_connections_opened_total", Default::default()),
            1,
            "metrics: {}",
            metrics.dump()
        );
        assert_eq!(
            metrics.query_sum("istio_tcp_connections_closed_total", Default::default()),
            0,
            "metrics: {}",
            metrics.dump()
        );

        // Drop the connection...
        drop(stream);

        // Eventually we should also have 1 closed connection
        assert_eventually(
            Duration::from_secs(2),
            || async {
                app.metrics()
                    .await
                    .query_sum("istio_tcp_connections_opened_total", Default::default())
            },
            1,
        )
        .await;
        assert_eventually(
            Duration::from_secs(2),
            || async {
                app.metrics()
                    .await
                    .query_sum("istio_tcp_connections_closed_total", Default::default())
            },
            1,
        )
        .await;
    })
    .await;
}

async fn assert_eventually<F, T, Fut>(dur: Duration, f: F, expected: T)
where
    F: Fn() -> Fut,
    Fut: Future<Output = T>,
    T: Eq + Debug,
{
    let mut delay = Duration::from_millis(10);
    let end = SystemTime::now().add(dur);
    let mut last: T;
    let mut attempts = 0;
    loop {
        attempts += 1;
        last = f().await;
        if last == expected {
            return;
        }
        trace!("attempt {attempts} with delay {delay:?}");
        if SystemTime::now().add(delay) > end {
            panic!("assert_eventually failed after {attempts}: last response: {last:?}")
        }
        tokio::time::sleep(delay).await;
        delay *= 2;
    }
}

async fn read_write_stream(stream: &mut TcpStream) {
    const BODY: &[u8] = "hello world".as_bytes();
    stream.write_all(BODY).await.unwrap();
    let mut buf: [u8; BODY.len()] = [0; BODY.len()];
    stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(BODY, buf);
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
