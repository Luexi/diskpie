//! Bounded, nonblocking bridge from the UI thread to local diagnostics.
//!
//! The shell only ever calls [`DiagnosticSink::emit`], which is a `try_send`
//! on a bounded channel: a full queue drops the event and counts the drop, so
//! the egui callback never waits on the writer. One root-owned thread drains
//! the channel into the recorder (normally `LocalDiagnostics::record`). The
//! composition root joins that thread with a deadline after the event loop
//! has returned; nothing here is joined from a callback.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use diskpie_app::diagnostics::DiagnosticEvent;

/// Events the UI may queue ahead of the recorder thread.
pub const SINK_CAPACITY: usize = 256;

/// Cloneable, path-free event producer held by the shell.
#[derive(Clone, Debug)]
pub struct DiagnosticSink {
    sender: Option<SyncSender<DiagnosticEvent>>,
    dropped: Arc<AtomicU64>,
}

impl DiagnosticSink {
    /// A sink that discards everything; used by headless tests.
    #[cfg(test)]
    #[must_use]
    pub fn disconnected() -> Self {
        Self { sender: None, dropped: Arc::new(AtomicU64::new(0)) }
    }

    /// Queues one event without waiting. A full queue or stopped bridge drops
    /// the event and increments the drop counter.
    pub fn emit(&self, event: DiagnosticEvent) {
        let Some(sender) = &self.sender else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        match sender.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Events dropped by every clone of this sink so far.
    #[cfg(test)]
    #[must_use]
    pub fn dropped_events(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Root-owned drain thread.
pub struct DiagnosticBridge {
    root_sender: Option<SyncSender<DiagnosticEvent>>,
    dropped: Arc<AtomicU64>,
    handle: Option<JoinHandle<()>>,
}

impl DiagnosticBridge {
    /// Starts the drain thread; `record` runs on that thread for every event.
    pub fn start(
        mut record: impl FnMut(DiagnosticEvent) + Send + 'static,
    ) -> std::io::Result<Self> {
        let (sender, receiver) = sync_channel::<DiagnosticEvent>(SINK_CAPACITY);
        let handle = thread::Builder::new()
            .name("diskpie-diagnostic-bridge".to_owned())
            .spawn(move || drain(&receiver, &mut record))?;
        Ok(Self {
            root_sender: Some(sender),
            dropped: Arc::new(AtomicU64::new(0)),
            handle: Some(handle),
        })
    }

    /// A producer handle for the shell.
    #[must_use]
    pub fn sink(&self) -> DiagnosticSink {
        DiagnosticSink { sender: self.root_sender.clone(), dropped: Arc::clone(&self.dropped) }
    }

    /// Events the producers dropped so far.
    #[must_use]
    pub fn dropped_events(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Closes the root's sender and waits at most `timeout` for the drain to
    /// finish. Returns whether the thread was joined. Every sink clone must
    /// have been dropped (the shell drops its clone when eframe drops the
    /// application) for the drain to observe the channel closing.
    pub fn finish(mut self, timeout: Duration) -> bool {
        self.root_sender.take();
        let Some(handle) = self.handle.take() else {
            return true;
        };
        let deadline = Instant::now() + timeout;
        while !handle.is_finished() {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(2));
        }
        handle.join().is_ok()
    }
}

fn drain(receiver: &Receiver<DiagnosticEvent>, record: &mut impl FnMut(DiagnosticEvent)) {
    while let Ok(event) = receiver.recv() {
        record(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskpie_app::diagnostics::DiagnosticCode;
    use std::sync::Mutex;

    #[test]
    fn events_reach_the_recorder_and_the_bridge_joins_after_the_sinks_drop() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder_seen = Arc::clone(&seen);
        let bridge = DiagnosticBridge::start(move |event| {
            recorder_seen.lock().unwrap().push(event.code());
        })
        .expect("bridge starts");
        let sink = bridge.sink();
        sink.emit(DiagnosticEvent::new(DiagnosticCode::ScanRequested));
        sink.emit(DiagnosticEvent::new(DiagnosticCode::ScanStarted));
        drop(sink);
        assert!(bridge.finish(Duration::from_secs(5)));
        assert_eq!(
            *seen.lock().unwrap(),
            vec![DiagnosticCode::ScanRequested, DiagnosticCode::ScanStarted]
        );
    }

    #[test]
    fn a_full_queue_drops_and_counts_instead_of_blocking() {
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock().unwrap();
        let recorder_gate = Arc::clone(&gate);
        let bridge = DiagnosticBridge::start(move |_event| {
            // Block the drain until the test releases the gate.
            drop(recorder_gate.lock().unwrap());
        })
        .expect("bridge starts");
        let sink = bridge.sink();
        for _ in 0..(SINK_CAPACITY + 8) {
            sink.emit(DiagnosticEvent::new(DiagnosticCode::ScanProgress));
        }
        // The drain thread may have taken one event off the queue before
        // blocking, so at least seven of the surplus were dropped.
        assert!(sink.dropped_events() >= 7);
        assert_eq!(sink.dropped_events(), bridge.dropped_events());
        drop(held);
        drop(sink);
        assert!(bridge.finish(Duration::from_secs(5)));
    }

    #[test]
    fn disconnected_sink_counts_every_event_as_dropped() {
        let sink = DiagnosticSink::disconnected();
        sink.emit(DiagnosticEvent::new(DiagnosticCode::ScanRequested));
        assert_eq!(sink.dropped_events(), 1);
    }
}
