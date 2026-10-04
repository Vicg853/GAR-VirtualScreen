//! Bounded immutable packet fanout with inter-frame codec resynchronization.
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::mpsc;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    Queued,
    AwaitingKeyframe,
    Overflow,
    Closed,
}
pub struct Subscriber<T> {
    sender: mpsc::Sender<Arc<T>>,
    waiting: bool,
    pub dropped: Arc<AtomicU64>,
}
impl<T> Subscriber<T> {
    pub fn new(sender: mpsc::Sender<Arc<T>>) -> Self {
        Self {
            sender,
            waiting: true,
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }
    pub fn publish(&mut self, packet: Arc<T>, keyframe: bool) -> Delivery {
        if self.waiting && !keyframe {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Delivery::AwaitingKeyframe;
        }
        match self.sender.try_send(packet) {
            Ok(()) => {
                self.waiting = false;
                Delivery::Queued
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.waiting = true;
                self.dropped.fetch_add(1, Ordering::Relaxed);
                Delivery::Overflow
            }
            Err(_) => Delivery::Closed,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overflow_requires_keyframe_and_memory_is_shared() {
        let (tx, mut rx) = mpsc::channel(1);
        let mut q = Subscriber::new(tx);
        let frame = Arc::new(vec![1u8; 4096]);
        assert_eq!(q.publish(frame.clone(), false), Delivery::AwaitingKeyframe);
        assert_eq!(q.publish(frame.clone(), true), Delivery::Queued);
        assert_eq!(Arc::strong_count(&frame), 2);
        assert_eq!(q.publish(frame.clone(), false), Delivery::Overflow);
        assert!(Arc::ptr_eq(&frame, &rx.try_recv().unwrap()));
        assert_eq!(q.publish(frame.clone(), false), Delivery::AwaitingKeyframe);
        assert_eq!(q.publish(frame.clone(), true), Delivery::Queued);
        assert_eq!(q.dropped.load(Ordering::Relaxed), 3);
        drop(rx);
        assert_eq!(q.publish(frame, true), Delivery::Closed);
    }
}
