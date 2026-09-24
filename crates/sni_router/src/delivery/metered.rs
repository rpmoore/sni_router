// Copyright 2026 Ryan Moore
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

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::Instant;

/// Last time any byte moved on a proxied connection, as milliseconds since
/// the connection started. Only maintained when an idle timeout is set.
#[derive(Clone, Debug)]
pub(super) struct Activity {
    start: Instant,
    last_millis: Arc<AtomicU64>,
}

impl Activity {
    pub(super) fn new(start: Instant) -> Self {
        Self {
            start,
            last_millis: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Marks now as the last activity.
    pub(super) fn touch(&self) {
        let millis = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        // Only the connection's own task writes, so a plain store suffices;
        // the watchdog reads it from the same task.
        self.last_millis.store(millis, Ordering::Relaxed);
    }

    pub(super) fn last(&self) -> Instant {
        self.start + Duration::from_millis(self.last_millis.load(Ordering::Relaxed))
    }
}

/// Client byte totals, shared between the metered stream and the
/// connection's close record (which reports them even if the task panics).
/// Only the connection's own task writes them, so the atomics are
/// uncontended; the per-chunk cost is one relaxed add, with no metrics call.
#[derive(Debug, Default)]
pub(super) struct ByteCounters {
    read: AtomicU64,
    written: AtomicU64,
}

impl ByteCounters {
    /// Bytes read from and written to the client so far.
    pub(super) fn totals(&self) -> (u64, u64) {
        (
            self.read.load(Ordering::Relaxed),
            self.written.load(Ordering::Relaxed),
        )
    }
}

/// Wraps the client stream, from accept onward, to count bytes in each
/// direction and, when an idle timeout is set, stamp activity.
///
/// Wrapping only the client side is enough: every byte proxied is either
/// read from the client or written to it.
pub(super) struct MeteredStream<S> {
    inner: S,
    counters: Arc<ByteCounters>,
    activity: Option<Activity>,
}

impl<S> MeteredStream<S> {
    pub(super) fn new(inner: S, counters: Arc<ByteCounters>, activity: Option<Activity>) -> Self {
        Self {
            inner,
            counters,
            activity,
        }
    }

    fn touch(&self) {
        if let Some(activity) = &self.activity {
            activity.touch();
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for MeteredStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let poll = Pin::new(&mut this.inner).poll_read(cx, buf);
        let read = buf.filled().len() - before;
        if read > 0 {
            this.counters.read.fetch_add(read as u64, Ordering::Relaxed);
            this.touch();
        }
        poll
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for MeteredStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(written)) = poll
            && written > 0
        {
            this.counters
                .written
                .fetch_add(written as u64, Ordering::Relaxed);
            this.touch();
        }
        poll
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}
