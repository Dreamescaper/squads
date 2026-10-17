//! Realtime event fan-out (milestone **M4**).
//!
//! A [`tokio::sync::broadcast`] channel decouples the collector task (which
//! drains the process-global [`ost::event_hub`](ost::event_hub)) from any number
//! of consumers (the UI, presence tracking, unread counters, …). Every
//! subscriber gets its own cursor.
//!
//! ## Lag behaviour
//!
//! The channel is **bounded**. A subscriber that is slower than the producer
//! more than `capacity` events behind will miss the oldest ones and next receive
//! [`tokio::sync::broadcast::error::RecvError::Lagged`]. That is the correct
//! trade-off for a live feed: the bus never blocks the collector and never grows
//! without bound.
//!
//! ## Resync
//!
//! A lag means the consumer's view is stale and it should **re-fetch** rather
//! than silently render with holes. [`recv`] makes that automatic: it converts a
//! lag into [`RealtimeEvent::Resync`], so consumers can handle one signal for
//! both server-side message loss and local bus lag (re-read history for the
//! affected chats; M5 implements the refresh).
//!
//! [`EventBus::publish`] never blocks and never errors; it returns how many
//! subscribers received the event (`0` when nobody is listening).

use std::fmt;

use tokio::sync::broadcast;

use super::event::RealtimeEvent;

/// Default number of buffered events per subscriber.
pub const DEFAULT_BUS_CAPACITY: usize = 1024;

/// A cheap-to-clone broadcast fan-out for [`RealtimeEvent`]s.
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<RealtimeEvent>,
    capacity: usize,
}

impl EventBus {
    /// Create a bus buffering up to `capacity` events per subscriber.
    ///
    /// A `capacity` of `0` is clamped to `1` (`broadcast::channel` requires a
    /// non-zero bound).
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx, capacity }
    }

    /// Subscribe to the bus. Each caller gets an independent cursor.
    pub fn subscribe(&self) -> broadcast::Receiver<RealtimeEvent> {
        self.tx.subscribe()
    }

    /// Publish an event to every current subscriber.
    ///
    /// Returns the number of subscribers that received it (`0` when nobody is
    /// listening, in which case the event is dropped). Never blocks, never
    /// errors.
    pub fn publish(&self, event: RealtimeEvent) -> usize {
        self.tx.send(event).unwrap_or(0)
    }

    /// Number of live subscribers.
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }

    /// The per-subscriber buffer bound this bus was built with.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(DEFAULT_BUS_CAPACITY)
    }
}

impl fmt::Debug for EventBus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventBus")
            .field("capacity", &self.capacity)
            .field("receivers", &self.tx.receiver_count())
            .finish()
    }
}

/// Await the next event, translating a broadcast lag into
/// [`RealtimeEvent::Resync`].
///
/// Returns `None` once every sender is gone (the service stopped). This is the
/// recommended way for consumers to read the bus: a `Lagged` error becomes a
/// single resync signal instead of an error the caller has to special-case.
pub async fn recv(rx: &mut broadcast::Receiver<RealtimeEvent>) -> Option<RealtimeEvent> {
    match rx.recv().await {
        Ok(event) => Some(event),
        Err(broadcast::error::RecvError::Lagged(dropped)) => Some(RealtimeEvent::Resync {
            reason: format!("event bus lagged: {dropped} event(s) dropped"),
        }),
        Err(broadcast::error::RecvError::Closed) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typing(chat_id: &str) -> RealtimeEvent {
        RealtimeEvent::Typing {
            chat_id: chat_id.into(),
            sender_mri: "8:orgid:aaaa".into(),
            sender_name: "Alice".into(),
        }
    }

    #[tokio::test]
    async fn publish_reaches_every_subscriber() {
        let bus = EventBus::new(8);
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();
        assert_eq!(bus.receiver_count(), 2);

        assert_eq!(bus.publish(typing("c1")), 2);
        assert_eq!(a.recv().await.unwrap(), typing("c1"));
        assert_eq!(b.recv().await.unwrap(), typing("c1"));
    }

    #[tokio::test]
    async fn publish_without_subscribers_returns_zero() {
        let bus = EventBus::new(8);
        assert_eq!(bus.publish(typing("c1")), 0);
    }

    #[tokio::test]
    async fn slow_subscriber_lags_into_resync() {
        let bus = EventBus::new(2);
        let mut rx = bus.subscribe();
        for i in 0..5 {
            assert_eq!(bus.publish(typing(&format!("c{i}"))), 1);
        }
        // The subscriber fell behind by at least three events; `recv` surfaces
        // that as a single Resync instead of an error.
        match recv(&mut rx).await {
            Some(RealtimeEvent::Resync { reason }) => {
                assert!(reason.contains("lagged"), "reason = {reason}");
            }
            other => panic!("expected Resync, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn recv_returns_none_when_all_senders_drop() {
        let bus = EventBus::new(4);
        let mut rx = bus.subscribe();
        drop(bus);
        assert!(recv(&mut rx).await.is_none());
    }

    #[test]
    fn capacity_is_clamped_and_reported() {
        assert_eq!(EventBus::new(0).capacity(), 1);
        assert_eq!(EventBus::default().capacity(), DEFAULT_BUS_CAPACITY);
        let dbg = format!("{:?}", EventBus::new(4));
        assert!(dbg.contains("capacity"));
    }
}
