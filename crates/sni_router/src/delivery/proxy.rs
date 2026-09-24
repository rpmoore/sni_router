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

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::metered::Activity;

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
/// `copy_bidirectional` propagates half-close: when one side stops sending,
/// the other side's write half is shut down while the reverse direction
/// keeps flowing until it finishes too.
pub(super) async fn proxy<C, U>(
    client: &mut C,
    upstream: &mut U,
    activity: Option<&Activity>,
    idle_timeout: Option<Duration>,
    buffer_size: usize,
    force: &CancellationToken,
) -> ProxyEnd
where
    C: AsyncRead + AsyncWrite + Unpin,
    U: AsyncRead + AsyncWrite + Unpin,
{
    tokio::select! {
        biased;
        _ = force.cancelled() => ProxyEnd::Forced,
        _ = idle_expired(activity, idle_timeout) => ProxyEnd::Idle,
        result = tokio::io::copy_bidirectional_with_sizes(client, upstream, buffer_size, buffer_size) => match result {
            Ok(_) => ProxyEnd::Finished,
            Err(error) => ProxyEnd::Error(error),
        },
    }
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
