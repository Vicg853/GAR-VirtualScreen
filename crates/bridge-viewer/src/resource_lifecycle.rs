//! A native producer must not be destroyed until its consumers release resources.
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
    mpsc,
};

/// Clones retain their generation; advancing invalidates old network producers.
#[derive(Clone, Default)]
pub struct InputEpoch {
    active: Arc<AtomicU64>,
    local: u64,
}
impl InputEpoch {
    pub fn advance(&mut self) -> u64 {
        self.local = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        self.local
    }
    pub fn current(&self) -> Option<u64> {
        (self.active.load(Ordering::Acquire) == self.local).then_some(self.local)
    }
    pub fn invalidate(&self) {
        self.active.fetch_add(1, Ordering::AcqRel);
    }
}

pub struct ReleaseGuard<F: FnOnce()> {
    released: mpsc::Receiver<()>,
    on_stopped: Option<F>,
}

impl<F: FnOnce()> ReleaseGuard<F> {
    pub fn new(released: mpsc::Receiver<()>, on_stopped: F) -> Self {
        Self {
            released,
            on_stopped: Some(on_stopped),
        }
    }
}

impl<F: FnOnce()> Drop for ReleaseGuard<F> {
    fn drop(&mut self) {
        // Wake the consumer before waiting. This guard is declared AFTER the
        // native backend, so it also fences early returns and Rust unwinding.
        if let Some(on_stopped) = self.on_stopped.take() {
            on_stopped();
        }
        let _ = self.released.recv();
    }
}

/// Requests consumer release and waits for an explicit success acknowledgement.
/// Disconnection/error never grants permission to replace a live native backend.
pub fn release_barrier<T>(
    output: &mpsc::SyncSender<T>,
    event: impl FnOnce(mpsc::Sender<Result<(), String>>) -> T,
    wake: impl Fn(),
) -> Result<(), String> {
    let (ack, released) = mpsc::channel();
    wake(); // Output may already be full: wake its owner before blocking send.
    output
        .send(event(ack))
        .map_err(|_| "Frame consumer closed during release".to_owned())?;
    wake();
    released
        .recv()
        .map_err(|_| "Frame release acknowledgement was cancelled".to_owned())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Arc, Mutex},
        thread,
        time::Duration,
    };

    struct Native(Arc<Mutex<Vec<&'static str>>>);
    impl Drop for Native {
        fn drop(&mut self) {
            self.0.lock().unwrap().push("native destroyed");
        }
    }

    fn check_release(unwind: bool) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (permit, released) = mpsc::channel();
        let (stopped, notified) = mpsc::channel();
        let worker_events = events.clone();
        let worker = thread::spawn(move || {
            let _native = Native(worker_events);
            let _guard = ReleaseGuard::new(released, move || {
                stopped.send(()).unwrap();
            });
            if unwind {
                panic!("simulated decoder panic");
            }
        });
        notified.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(events.lock().unwrap().is_empty());
        events.lock().unwrap().push("consumer released");
        permit.send(()).unwrap();
        assert_eq!(worker.join().is_err(), unwind);
        assert_eq!(
            *events.lock().unwrap(),
            vec!["consumer released", "native destroyed"]
        );
    }

    #[test]
    fn native_backend_waits_for_consumer_release() {
        check_release(false);
    }
    #[test]
    fn unwind_waits_for_consumer_release() {
        check_release(true);
    }
    #[test]
    fn disconnected_consumer_unblocks_worker() {
        let (permit, released) = mpsc::channel();
        drop(permit);
        let worker = thread::spawn(move || {
            let _guard = ReleaseGuard::new(released, || {});
        });
        worker.join().unwrap();
    }
    enum Event {
        Frame,
        Release(mpsc::Sender<Result<(), String>>),
    }
    #[test]
    fn replacement_waits_for_queued_frames_and_acknowledgement() {
        let (output, input) = mpsc::sync_channel(1);
        output.send(Event::Frame).unwrap();
        let (wake, wake_rx) = mpsc::channel();
        let (done, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = release_barrier(&output, Event::Release, || {
                wake.send(()).unwrap();
            });
            done.send(result).unwrap();
        });
        wake_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(input.recv().unwrap(), Event::Frame));
        let Event::Release(ack) = input.recv().unwrap() else {
            panic!("expected barrier");
        };
        assert!(done_rx.try_recv().is_err());
        ack.send(Ok(())).unwrap();
        assert!(
            done_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_ok()
        );
        worker.join().unwrap();
    }
    #[test]
    fn close_unblocks_full_queue_release() {
        let (output, input) = mpsc::sync_channel(1);
        output.send(Event::Frame).unwrap();
        let (wake, wake_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            release_barrier(&output, Event::Release, || {
                let _ = wake.send(());
            })
        });
        wake_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(input);
        assert!(worker.join().unwrap().is_err());
    }
    #[test]
    fn failed_gpu_release_prevents_replacement() {
        let (output, input) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || release_barrier(&output, Event::Release, || {}));
        let Event::Release(ack) = input.recv().unwrap() else {
            panic!("expected barrier");
        };
        ack.send(Err("device lost".into())).unwrap();
        assert_eq!(worker.join().unwrap().unwrap_err(), "device lost");
    }
    #[test]
    fn replacing_generation_invalidates_existing_producers() {
        let mut current = InputEpoch::default();
        let old = current.clone();
        let next = current.advance();
        assert_eq!(old.current(), None);
        assert_eq!(current.current(), Some(next));
        let producer = current.clone();
        current.invalidate();
        assert_eq!(producer.current(), None);
    }
}
