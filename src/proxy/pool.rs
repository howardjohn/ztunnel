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

use bytes::Bytes;
use futures::channel::oneshot;

use futures_util::FutureExt;
use http_body_util::Empty;
use hyper::body::Incoming;
use hyper::client::conn::http2;
use hyper::http::{Request, Response};

use hyper::upgrade::Upgraded;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::error;

use crate::identity::Identity;
use crate::proxy::Error;

#[derive(Clone)]
pub struct Pool {
    inner: Arc<Mutex<PoolInner>>,
}

#[derive(Clone, Debug)]
struct Pooled {
    value: Client,
    active: Arc<AtomicU64>,
    idle_at: Option<Instant>,
}

const POOL_TIMEOUT: Duration = Duration::from_secs(5); // TODO
const POOL_REQUESTS_PER_CONN: u64 = 5; // TODO

impl Pooled {
    fn is_usable(&self) -> bool {
        self.active.load(Ordering::SeqCst) < POOL_REQUESTS_PER_CONN
    }
    fn is_idle(&self) -> bool {
        if let Some(idle) = self.idle_at {
            assert_eq!(self.active.load(Ordering::SeqCst), 0);
            Instant::now().saturating_duration_since(idle) > POOL_TIMEOUT
        } else {
            false
        }
    }
}

struct PoolInner {
    connecting: HashMap<Key, u64>,
    pooled: HashMap<Key, Vec<Pooled>>,
    waiters: HashMap<Key, VecDeque<oneshot::Sender<Pooled>>>,
}

impl Pool {
    pub fn new() -> Pool {
        Self {
            inner: Arc::new(Mutex::new(PoolInner {
                connecting: HashMap::new(),
                pooled: HashMap::new(),
                waiters: HashMap::new(),
            })),
        }
    }

    // checkout will checkout an existing connection. If one is available, it will be used right away.
    // If not, and a new connection should be established, None will be returned
    // If not, we will wait for another connection to be established by someone else, then return it.
    pub async fn checkout(&self, key: &Key) -> Option<Pooled> {
        error!("checkout");
        let maybe_rx = {
            let mut inner = self.inner.lock().unwrap();
            let resp = inner.pooled.get_mut(key).and_then(|conns| {
                // Drop all idle connections
                conns.retain(|c| !c.is_idle());
                // Find the first usable one, if any
                conns.iter_mut().find(|c| c.is_usable()).map(|v| {
                    v.active.fetch_add(1, Ordering::SeqCst);
                    v.clone()
                })
            });
            error!("checkout {resp:?}");
            if let Some(conns) = inner.pooled.get(key) {
                if conns.is_empty() {
                    inner.pooled.remove(key);
                }
            }
            if resp.is_some() {
                return resp;
            }

            if let Some(num_connecting) = inner.connecting.get(key) {
                // There is already some establishing connections...
                let waiters = inner.waiters.get(key).map(|w| w.len()).unwrap_or_default();
                error!("checkout connecting={num_connecting:?} waiters={waiters}");
                if (waiters as u64) < num_connecting * POOL_REQUESTS_PER_CONN {
                    // the establishing connections can each serve POOL_REQUESTS_PER_CONN.
                    // We have 'waiter' number of clients waiting for a connection
                    // If the in-progress connections can fit us, become a waiter.
                    let (tx, rx) = oneshot::channel();
                    inner.waiters.entry(key.clone()).or_default().push_back(tx);
                    error!("checkout we are a waiter");
                    Some(rx)
                } else {
                    None
                }
            } else {
                None
            }
        };

        if let Some(rx) = maybe_rx {
            error!("checkout waiting...");
            if let Ok(c) = rx.await {
                error!("checkout found!");
                return Some(c);
            } else {
                error!("checkout wait failed..!");
            }
        }

        error!("checkout we need to open our own connection..!");
        // If we got here, the caller should open a connection.
        None
    }

    // connecting marks a key as currently connecting.
    pub fn connecting(&self, key: &Key) {
        let mut inner = self.inner.lock().unwrap();
        inner
            .connecting
            .entry(key.clone())
            .and_modify(|v| *v += 1)
            .or_insert(1);
    }

    // add_to_pool adds a connection to the pool for the key
    pub fn add_to_pool(&self, key: &Key, value: Client) -> Pooled {
        let mut inner = self.inner.lock().unwrap();
        let c = inner
            .connecting
            .get_mut(key)
            .expect("there must be a connecting");
        *c -= 1;
        // Insert our new entry
        let mut entry = Pooled {
            value: value,
            active: Arc::new(AtomicU64::new(1)),
            idle_at: None,
        };
        let to_insert = entry.clone();
        if let Some(waiters) = inner.waiters.remove(key) {
            let mut push_back = VecDeque::new();
            for waiter in waiters {
                if entry.active.load(Ordering::SeqCst) >= POOL_REQUESTS_PER_CONN {
                    push_back.push_back(waiter);
                } else {
                    // For each waiter, mark its in used
                    entry.active.fetch_add(1, Ordering::SeqCst);
                    // and give them a handle to the connection
                    waiter.send(entry.clone()); // TODO handle error
                }
            }
            if !push_back.is_empty() {
                inner.waiters.insert(key.clone(), push_back);
            }
        }
        // Push the new entry, with active count
        inner.pooled.entry(key.clone()).or_default().push(to_insert);
        // Give the original value back
        entry
    }
}

#[derive(Clone)]
pub struct TokioExec;

impl<F> hyper::rt::Executor<F> for TokioExec
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    fn execute(&self, fut: F) {
        tokio::spawn(fut);
    }
}

#[derive(Debug, Clone)]
pub struct Client(http2::SendRequest<Empty<Bytes>>);

#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub struct Key {
    pub src_id: Identity,
    pub dst_id: Vec<Identity>,
    // In theory we can just use src,dst,node. However, the dst has a check that
    // the L3 destination IP matches the HBONE IP. This could be loosened to just assert they are the same identity maybe.
    pub dst: SocketAddr,
    // Because we spoof the source IP, we need to key on this as well. Note: for in-pod its already per-pod
    // pools anyways.
    pub src: IpAddr,
}

impl Client {
    pub fn send_request(
        mut self,
        req: Request<Empty<Bytes>>,
    ) -> impl Future<Output = hyper::Result<Response<Incoming>>> {
        self.0.send_request(req)
    }
}

pub struct Stream {
    stream: Upgraded,
    counter: Arc<AtomicU64>,
}

impl Stream {
    pub fn as_mut(&mut self) -> &mut Upgraded {
        &mut self.stream
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Pool {
    pub async fn connect<F>(
        &self,
        key: Key,
        connect: F,
        req: Request<Empty<Bytes>>,
    ) -> Result<Stream, Error>
    where
        F: Future<Output = Result<http2::SendRequest<Empty<Bytes>>, Error>>,
    {
        let mut connection = {
            if let Some(reused_connection) = self.checkout(&key).await {
                reused_connection
            } else {
                self.connecting(&key);
                let new_connection = Client(connect.await?);
                self.add_to_pool(&key, new_connection)
            }
        };

        let response = connection.value.0.send_request(req).await?;
        let code = response.status();
        if code != 200 {
            return Err(Error::HttpStatus(code));
        }
        let upgraded = hyper::upgrade::on(response).await?;
        Ok(Stream {
            stream: upgraded,
            counter: connection.active,
        })
    }
}
#[cfg(test)]
mod test {
    use std::convert::Infallible;
    use std::net::SocketAddr;

    use hyper::body::Incoming;
    use hyper::service::service_fn;
    use hyper::{Request, Response};
    use tokio::net::{TcpListener, TcpStream};
    use tracing::{error, info};

    use super::*;

    #[tokio::test]
    async fn test_pool() {
        // We'll bind to 127.0.0.1:3000
        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        async fn hello_world(req: Request<Incoming>) -> Result<Response<Empty<Bytes>>, Infallible> {
            info!("got req {req:?}");
            Ok(Response::builder().status(200).body(Empty::new()).unwrap())
        }

        // We create a TcpListener and bind it to 127.0.0.1:3000
        let listener = TcpListener::bind(addr).await.unwrap();

        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // We start a loop to continuously accept incoming connections
            loop {
                let (stream, _) = listener.accept().await.unwrap();

                // Spawn a tokio task to serve multiple connections concurrently
                tokio::task::spawn(async move {
                    // Finally, we bind the incoming connection to our `hello` service
                    if let Err(err) = crate::hyper_util::http2_server()
                        .serve_connection(
                            hyper_util::rt::TokioIo::new(stream),
                            service_fn(hello_world),
                        )
                        .await
                    {
                        println!("Error serving connection: {:?}", err);
                    }
                });
            }
        });
        let pool = Pool::new();
        let key = Key {
            src_id: Identity::default(),
            dst_id: vec![Identity::default()],
            src: IpAddr::from([127, 0, 0, 2]),
            dst: addr,
        };
        let connect = || async {
            let builder = http2::Builder::new(TokioExec);

            let tcp_stream = TcpStream::connect(addr).await?;
            let (request_sender, connection) = builder
                .handshake(hyper_util::rt::TokioIo::new(tcp_stream))
                .await?;
            // spawn a task to poll the connection and drive the HTTP state
            tokio::spawn(async move {
                if let Err(e) = connection.await {
                    error!("Error in connection handshake: {:?}", e);
                }
            });
            Ok(request_sender)
        };
        let req = || {
            hyper::Request::builder()
                .uri(format!("http://{addr}"))
                .method(hyper::Method::GET)
                .version(hyper::Version::HTTP_2)
                .body(Empty::<Bytes>::new())
                .unwrap()
        };
        let mut c1 = pool.connect(key.clone(), connect()).await.unwrap();
        let mut c2 = pool
            .connect(key, async { unreachable!("should use pooled connection") })
            .await
            .unwrap();
        assert_eq!(c1.send_request(req()).await.unwrap().status(), 200);
        assert_eq!(c1.send_request(req()).await.unwrap().status(), 200);
        assert_eq!(c2.send_request(req()).await.unwrap().status(), 200);
    }
}
