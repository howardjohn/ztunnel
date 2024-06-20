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

use std::cmp;
use std::future::Future;
use std::io::{Error, ErrorKind, IoSlice};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use pin_project_lite::pin_project;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_util::io::{InspectReader};

pub fn is_runtime_shutdown(e: &Error) -> bool {
    if e.kind() == ErrorKind::Other
        && e.to_string() == "A Tokio 1.x context was found, but it is being shutdown."
    {
        return true;
    }
    false
}

#[pin_project::pin_project] // This generates a `project` method
pub struct TimedWrapper<Fut: Future> {
    // For each field, we need to choose whether `project` returns an
    // unpinned (&mut T) or pinned (Pin<&mut T>) reference to the field.
    // By default, it assumes unpinned:
    start: Option<Instant>,
    total_runtime: std::time::Duration,
    total_executions: usize,
    // Opt into pinned references with this attribute:
    #[pin]
    future: Fut,
}

impl<Fut: Future> TimedWrapper<Fut> {
    pub fn new(future: Fut) -> Self {
        Self {
            future,
            total_runtime: Duration::new(0, 0),
            total_executions: 0,
            start: None,
        }
    }
}

impl<Fut: Future> Future for TimedWrapper<Fut> {
    type Output = Fut::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let mut this = self.project();
        // Call the inner poll, measuring how long it took.
        let t0 = Instant::now();
        if this.start.is_none() {
            *this.start = Some(t0)
        }
        *this.total_executions += 1;
        let inner_poll = this.future.as_mut().poll(cx);
        *this.total_runtime += t0.elapsed();

        match inner_poll {
            // The inner future needs more time, so this future needs more time too
            Poll::Pending => Poll::Pending,
            // Success!
            Poll::Ready(output) => {
                tracing::error!(
                    wall_time=?this.start.unwrap().elapsed(),
                    cpu_time=?this.total_runtime,
                    executions=this.total_executions,
                    "future complete",
                );
                Poll::Ready(output)
            }
        }
    }
}

pub fn new_logging_io<IO>(name: &str, io: IO) -> impl AsyncRead + AsyncWrite
where
    IO: AsyncWrite + AsyncRead,
{
    let name2 = name.to_string();
    let read = InspectReader::new(io, move|bytes| {
        tracing::error!(name=name2, size = bytes.len(), "read");
    });
    new_logging_writer(&name, read)
}

pub fn new_logging_writer<IO>(name: &str, io: IO) -> InspectWriter<IO, impl FnMut(Vec<&[u8]>)>
where
    IO: AsyncWrite,
{
    let name = name.to_string();
    let write = InspectWriter::new(io, move |bytes| {
        let total: usize = bytes.iter().map(|b| b.len()).sum();
        // let peek =String::from_utf8_lossy(&bytes[..cmp::min(100, bytes.len()-1)]);
        tracing::error!(name, size = total, chunks = bytes.len(), "write");
    });
    write
}


pin_project! {
    /// An adapter that lets you inspect the data that's being written.
    ///
    /// This is useful for things like hashing data as it's written out.
    pub struct InspectWriter<W, F> {
        #[pin]
        writer: W,
        f: F,
    }
}

// Forked to expose vectorization
impl<W, F> InspectWriter<W, F> {
    /// Create a new `InspectWriter`, wrapping `write` and calling `f` for the
    /// data successfully written by each write call.
    ///
    /// The closure `f` will never be called with an empty slice. A vectored
    /// write can result in multiple calls to `f` - at most one call to `f` per
    /// buffer supplied to `poll_write_vectored`.
    pub fn new(writer: W, f: F) -> InspectWriter<W, F>
    where
        W: AsyncWrite,
        F: FnMut(Vec<&[u8]>),
    {
        InspectWriter { writer, f }
    }

    /// Consumes the `InspectWriter`, returning the wrapped writer
    pub fn into_inner(self) -> W {
        self.writer
    }
}

impl<W: AsyncWrite, F: FnMut(Vec<&[u8]>)> AsyncWrite for InspectWriter<W, F> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        let me = self.project();
        let res = me.writer.poll_write(cx, buf);
        if let Poll::Ready(Ok(count)) = res {
            if count != 0 {
                (me.f)(vec![&buf[..count]]);
            }
        }
        res
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let me = self.project();
        me.writer.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let me = self.project();
        me.writer.poll_shutdown(cx)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let me = self.project();
        let res = me.writer.poll_write_vectored(cx, bufs);
        if let Poll::Ready(Ok(mut count)) = res {
            let mut written = vec![];
            for buf in bufs {
                if count == 0 {
                    break;
                }
                let size = count.min(buf.len());
                if size != 0 {
                    written.push(&buf[..size]);
                    count -= size;
                }
            }
            (me.f)(written);
        }
        res
    }

    fn is_write_vectored(&self) -> bool {
        self.writer.is_write_vectored()
    }
}

impl<W: AsyncRead, F> AsyncRead for InspectWriter<W, F> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.project().writer.poll_read(cx, buf)
    }
}
