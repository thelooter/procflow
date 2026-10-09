//! Fan-out of the collector's per-poll deltas to live `Watch` streams
//! (ADR-0008). The current minute only exists in the collector's memory, so
//! this is the one path by which a client sees traffic as it happens.

use procflow_ipc::v1::Scope;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

/// Bytes one Identity moved during one poll interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delta {
    pub identity_id: i64,
    pub scope: Scope,
    pub ingress_bytes: u64,
    pub egress_bytes: u64,
}

/// One poll interval's worth of deltas. Published even when empty, so
/// subscribers see rates fall to zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tick {
    pub at_unix_ms: i64,
    pub interval_ms: u32,
    pub deltas: Vec<Delta>,
}

pub enum Event {
    Tick(Arc<Tick>),
    /// The subscriber's own connection asked to stop (Cancel or hang-up).
    Stop,
}

#[derive(Default)]
pub struct Hub {
    subscribers: Mutex<Vec<Sender<Event>>>,
}

impl Hub {
    /// The sender is returned too, so the subscriber can feed its own `Stop`
    /// into the same queue it reads ticks from.
    pub fn subscribe(&self) -> (Sender<Event>, Receiver<Event>) {
        let (tx, rx) = channel();
        self.subscribers
            .lock()
            .expect("hub mutex poisoned")
            .push(tx.clone());
        (tx, rx)
    }

    /// Deliver to every subscriber, forgetting the ones that hung up.
    pub fn publish(&self, tick: Tick) {
        let tick = Arc::new(tick);
        self.subscribers
            .lock()
            .expect("hub mutex poisoned")
            .retain(|tx| tx.send(Event::Tick(tick.clone())).is_ok());
    }
}
