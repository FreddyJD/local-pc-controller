use crate::transport::Lifetime;
use anyhow::Result;
use std::{
    sync::{Condvar, Mutex},
    time::Duration,
};

struct State<T> {
    pending: Option<T>,
    closed: bool,
    error: Option<String>,
    replaced: u64,
}

pub struct Latest<T> {
    state: Mutex<State<T>>,
    wake: Condvar,
}

impl<T> Default for Latest<T> {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                pending: None,
                closed: false,
                error: None,
                replaced: 0,
            }),
            wake: Condvar::new(),
        }
    }
}

impl<T> Latest<T> {
    pub fn publish(&self, value: T) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return false;
        }
        let old = state.pending.replace(value);
        if old.is_some() {
            state.replaced += 1;
        }
        drop(state);
        self.wake.notify_one();
        drop(old);
        true
    }

    pub fn take(&self, life: &Lifetime) -> Result<Option<T>> {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(error) = &state.error {
                anyhow::bail!("{error}");
            }
            if state.closed || life.stopped() {
                return Ok(None);
            }
            if let Some(value) = state.pending.take() {
                return Ok(Some(value));
            }
            // Socket cancellation also interrupts a mailbox-only wait within this bounded poll.
            state = self
                .wake
                .wait_timeout(state, Duration::from_millis(50))
                .unwrap()
                .0;
        }
    }

    pub fn close(&self, error: Option<String>) {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return;
        }
        state.closed = true;
        state.error = error;
        let old = state.pending.take();
        drop(state);
        self.wake.notify_all();
        drop(old);
    }

    pub fn replaced(&self) -> u64 {
        self.state.lock().unwrap().replaced
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, thread};

    #[test]
    fn latest_replaces_stale_frames_and_keeps_terminal_errors() {
        let queue = Latest::default();
        let life = Lifetime::default();
        for value in 0..100 {
            assert!(queue.publish(value));
        }
        assert_eq!(queue.replaced(), 99);
        assert_eq!(queue.take(&life).unwrap(), Some(99));
        queue.publish(100);
        queue.close(Some("capture permission revoked".into()));
        assert!(!queue.publish(101));
        queue.close(None);
        assert!(
            queue
                .take(&life)
                .unwrap_err()
                .to_string()
                .contains("permission revoked")
        );
    }

    #[test]
    fn close_and_cancellation_wake_empty_waiters() {
        for close in [true, false] {
            let queue = Arc::new(Latest::<u8>::default());
            let life = Arc::new(Lifetime::default());
            let (q, l) = (queue.clone(), life.clone());
            let waiter = thread::spawn(move || q.take(&l).unwrap());
            if close {
                queue.close(None);
            } else {
                life.stop();
            }
            assert_eq!(waiter.join().unwrap(), None);
        }
    }

    #[test]
    fn slow_consumer_does_not_block_producer_or_accumulate_backlog() {
        let queue = Arc::new(Latest::default());
        let producer = queue.clone();
        let worker = thread::spawn(move || {
            for n in 0..10000 {
                producer.publish(n);
            }
        });
        worker.join().unwrap();
        assert_eq!(queue.take(&Lifetime::default()).unwrap(), Some(9999));
        assert_eq!(queue.replaced(), 9999);
    }

    #[test]
    fn pipeline_receives_new_frames_while_decoder_is_busy() {
        use std::sync::mpsc;
        let queue = Arc::new(Latest::default());
        let decoder_queue = queue.clone();
        let (began, ready) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        queue.publish(0);
        let decoder = thread::spawn(move || {
            let life = Lifetime::default();
            assert_eq!(decoder_queue.take(&life).unwrap(), Some(0));
            began.send(()).unwrap();
            wait.recv().unwrap();
            assert_eq!(decoder_queue.take(&life).unwrap(), Some(20));
        });
        ready.recv().unwrap();
        for n in 1..=20 {
            queue.publish(n);
        }
        assert_eq!(queue.replaced(), 19);
        release.send(()).unwrap();
        decoder.join().unwrap();
    }
}
