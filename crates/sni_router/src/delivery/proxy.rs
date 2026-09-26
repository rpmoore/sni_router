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
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::metered::{Activity, MeteredStream};

/// Why proxying stopped.
#[derive(Debug)]
pub(super) enum ProxyEnd {
    /// Both directions reached EOF.
    Finished,
    Error(io::Error),
    Idle,
    Forced,
}

/// Splices `client` and `upstream` until both directions finish, either side
/// errors, the connection idles out, or `force` fires.
///
/// Propagates half-close: when one side stops sending, the other side's
/// write half is shut down while the reverse direction keeps flowing until
/// it finishes too. See [`copy`] for which copy mechanism that is.
pub(super) async fn proxy(
    client: &mut MeteredStream<TcpStream>,
    upstream: &mut TcpStream,
    activity: Option<&Activity>,
    idle_timeout: Option<Duration>,
    buffer_size: usize,
    force: &CancellationToken,
) -> ProxyEnd {
    tokio::select! {
        biased;
        _ = force.cancelled() => ProxyEnd::Forced,
        _ = idle_expired(activity, idle_timeout) => ProxyEnd::Idle,
        result = copy(client, upstream, buffer_size, activity) => match result {
            Ok(()) => ProxyEnd::Finished,
            Err(error) => ProxyEnd::Error(error),
        },
    }
}

/// On Linux, `splice(2)` through an in-kernel pipe (`super::splice`) when
/// it's usable in this environment (`super::splice::available`) and the
/// process-wide fd budget has room (`super::splice::admission`), falling
/// back to the portable userspace copy otherwise, or if setting up its
/// pipes fails (e.g. the process is out of file descriptors despite the
/// budget check — `try_acquire` reserves capacity, it doesn't guarantee the
/// kernel will grant it). That setup happens before either socket is
/// touched, so falling back then never loses or double-counts a byte.
/// Elsewhere, the userspace copy is the only option.
#[cfg(target_os = "linux")]
async fn copy(
    client: &mut MeteredStream<TcpStream>,
    upstream: &mut TcpStream,
    buffer_size: usize,
    activity: Option<&Activity>,
) -> io::Result<()> {
    if super::splice::available()
        && let Some(permit) = super::splice::admission().try_acquire()
    {
        match super::splice::prepare(buffer_size) {
            Ok(pipes) => {
                return super::splice::copy_bidirectional(
                    client.get_ref(),
                    upstream,
                    pipes,
                    client.counters(),
                    activity,
                )
                .await;
            }
            Err(error) => {
                // Not using the reserved fds after all: release them
                // immediately rather than holding them idle for the
                // portable copy below, which doesn't need them.
                drop(permit);
                tracing::debug!(%error, "splice pipe setup failed, using the portable copy");
            }
        }
    }
    tokio::io::copy_bidirectional_with_sizes(client, upstream, buffer_size, buffer_size)
        .await
        .map(|_| ())
}

#[cfg(not(target_os = "linux"))]
async fn copy(
    client: &mut MeteredStream<TcpStream>,
    upstream: &mut TcpStream,
    buffer_size: usize,
    _activity: Option<&Activity>,
) -> io::Result<()> {
    tokio::io::copy_bidirectional_with_sizes(client, upstream, buffer_size, buffer_size)
        .await
        .map(|_| ())
}

/// Resolves once no bytes have moved for `idle_timeout`; never resolves
/// when it's `None`.
async fn idle_expired(activity: Option<&Activity>, idle_timeout: Option<Duration>) {
    let (Some(activity), Some(idle_timeout)) = (activity, idle_timeout) else {
        return std::future::pending().await;
    };
    loop {
        // An idle timeout too large to represent never fires.
        let Some(deadline) = activity.last().checked_add(idle_timeout) else {
            return std::future::pending().await;
        };
        if Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep_until(deadline).await;
    }
}
