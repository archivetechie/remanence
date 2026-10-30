//! Session-owned timer threads: cancellation wakes and joins every waiter.

use super::DriveCommand;
use std::{
    future::Future,
    sync::{Arc, Condvar, Mutex},
    task::{Context, Poll, Wake, Waker},
    thread::JoinHandle,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

#[derive(Default)]
pub(super) struct SessionTimers {
    cancelled: Arc<(Mutex<bool>, Condvar)>,
    threads: Vec<JoinHandle<()>>,
}
impl SessionTimers {
    pub(super) fn arm(
        &mut self,
        name: String,
        sender: mpsc::Sender<DriveCommand>,
        delay: Duration,
        command: DriveCommand,
    ) -> std::io::Result<()> {
        let mut running = Vec::new();
        for thread in self.threads.drain(..) {
            if thread.is_finished() {
                let _ = thread.join();
            } else {
                running.push(thread);
            }
        }
        self.threads = running;
        let cancelled = Arc::clone(&self.cancelled);
        self.threads
            .push(std::thread::Builder::new().name(name).spawn(move || {
                let (lock, wake) = &*cancelled;
                let deadline = Instant::now() + delay;
                let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                while !*guard {
                    let now = Instant::now();
                    if now >= deadline {
                        break;
                    }
                    guard = wake
                        .wait_timeout(guard, deadline - now)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
                if *guard {
                    return;
                }
                drop(guard);
                // Keep Tokio's FIFO mailbox reservation while waiting for space.
                // Cancellation unparks this waiter and drops its queued reservation;
                // a blocking_send here could deadlock session teardown on a full inbox.
                let mut reservation = std::pin::pin!(sender.reserve());
                let waker = Waker::from(Arc::new(TimerWake(std::thread::current())));
                let mut context = Context::from_waker(&waker);
                loop {
                    if *lock.lock().unwrap_or_else(|e| e.into_inner()) {
                        return;
                    }
                    match reservation.as_mut().poll(&mut context) {
                        Poll::Ready(Ok(permit)) => {
                            permit.send(command);
                            return;
                        }
                        Poll::Ready(Err(_)) => return,
                        Poll::Pending => std::thread::park(),
                    }
                }
            })?);
        Ok(())
    }
}
struct TimerWake(std::thread::Thread);
impl Wake for TimerWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}
impl Drop for SessionTimers {
    fn drop(&mut self) {
        let (lock, wake) = &*self.cancelled;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        wake.notify_all();
        for thread in &self.threads {
            thread.thread().unpark();
        }
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellation_joins_sleeping_and_full_mailbox_timers_without_retaining_sender() {
        let (tx, mut rx) = mpsc::channel(1);
        let command = || DriveCommand::TimerIdleClose {
            session_id: uuid::Uuid::new_v4(),
            checkpoint_batch_id: uuid::Uuid::new_v4(),
        };
        tx.blocking_send(command()).unwrap();
        let mut timers = SessionTimers::default();
        timers
            .arm(
                "sleeping-timer".into(),
                tx.clone(),
                Duration::from_secs(3600),
                command(),
            )
            .unwrap();
        timers
            .arm(
                "full-mailbox-timer".into(),
                tx.clone(),
                Duration::ZERO,
                command(),
            )
            .unwrap();
        let start = Instant::now();
        drop(timers);
        assert!(start.elapsed() < Duration::from_secs(2));
        drop(tx);
        assert!(rx.blocking_recv().is_some());
        assert!(rx.blocking_recv().is_none());
    }
}
