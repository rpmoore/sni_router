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

use std::sync::Mutex;

use tokio_util::sync::CancellationToken;

/// The single point where a connection commits to the proxy stage.
///
/// Shutdown must treat every connection as either "still handshaking"
/// (cancel now) or "proxying" (let it drain). Both [`ProxyGate::enter`] and
/// [`ProxyGate::close`] run under one mutex, so a connection racing shutdown
/// lands on exactly one side: never both, never neither.
#[derive(Debug)]
pub(super) struct ProxyGate {
    state: Mutex<GateState>,
    cancel_handshakes: CancellationToken,
}

#[derive(Debug, Default)]
struct GateState {
    closed: bool,
    proxying: usize,
}

/// Held for the lifetime of a proxied connection.
#[derive(Debug)]
pub(super) struct ProxyPass<'a> {
    gate: &'a ProxyGate,
}

impl ProxyGate {
    pub(super) fn new() -> Self {
        Self {
            state: Mutex::new(GateState::default()),
            cancel_handshakes: CancellationToken::new(),
        }
    }

    /// Cancelled once shutdown starts; every pre-proxy await observes it.
    pub(super) fn handshakes_cancelled(&self) -> &CancellationToken {
        &self.cancel_handshakes
    }

    /// Commits a connection to the proxy stage, or `None` if shutdown has
    /// already started.
    pub(super) fn enter(&self) -> Option<ProxyPass<'_>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return None;
        }
        state.proxying += 1;
        Some(ProxyPass { gate: self })
    }

    /// Stops admitting connections to the proxy stage and cancels every
    /// connection that hasn't reached it. Returns how many are proxying.
    pub(super) fn close(&self) -> usize {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        self.cancel_handshakes.cancel();
        state.proxying
    }

    #[cfg(test)]
    pub(super) fn proxying(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .proxying
    }
}

impl Drop for ProxyPass<'_> {
    fn drop(&mut self) {
        let mut state = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        state.proxying -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn enter_before_close_is_admitted_and_counted() {
        let gate = ProxyGate::new();
        let pass = gate.enter().unwrap();
        assert_eq!(gate.close(), 1);
        assert!(gate.handshakes_cancelled().is_cancelled());
        drop(pass);
        assert_eq!(gate.proxying(), 0);
    }

    #[test]
    fn enter_after_close_is_refused() {
        let gate = ProxyGate::new();
        gate.close();
        assert!(gate.enter().is_none());
    }

    #[test]
    fn racing_enter_and_close_always_land_on_one_side() {
        for _ in 0..500 {
            let gate = Arc::new(ProxyGate::new());
            let barrier = Arc::new(Barrier::new(2));
            let entering = {
                let gate = Arc::clone(&gate);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    // Report admission and hold the pass until counted.
                    gate.enter().map(std::mem::forget).is_some()
                })
            };
            barrier.wait();
            let counted = gate.close();
            let admitted = entering.join().unwrap();
            // Either admitted before close (and counted by it), or refused.
            // A pass admitted after close would show admitted && counted == 0.
            assert_eq!(
                admitted,
                counted == 1,
                "admitted={admitted} counted={counted}"
            );
        }
    }
}
