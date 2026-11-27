//! Installation-wide submission order across windows. Waiting happens only in
//! the bounded control I/O lane, never in a Store callback or the UI thread.
use crate::instance::next_id;
use iris_api::v2::{ErrorCode, Id};
use iris_net::concurrency::{Deadline, WaitError};
use iris_net::http_client::HttpCancel;
use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

#[derive(Default)]
pub(super) struct Serial {
    queue: Mutex<VecDeque<Id>>,
    changed: Condvar,
}
pub(super) struct Ticket {
    serial: Arc<Serial>,
    id: Id,
}
impl Serial {
    pub fn enter(self: &Arc<Self>) -> Result<Ticket, ErrorCode> {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if queue.len() >= 8 {
            return Err(ErrorCode::Backpressure);
        }
        let id = Id(next_id()?);
        queue.push_back(id);
        Ok(Ticket {
            serial: self.clone(),
            id,
        })
    }
}
impl Ticket {
    pub fn wait(&self, cancel: &HttpCancel) -> Result<(), ErrorCode> {
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        let wake_serial = self.serial.clone();
        let _reg = cancel
            .register_callback(Arc::new(move || {
                let _guard = wake_serial.queue.lock().unwrap_or_else(|e| e.into_inner());
                wake_serial.changed.notify_all();
            }))
            .map_err(|_| ErrorCode::Cancelled)?;
        let deadline = Deadline::after(Duration::from_secs(30));
        let mut queue = self.serial.queue.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let remaining = match cancel.check(Some(deadline)) {
                Ok(Some(rem)) => rem,
                Ok(None) => Duration::from_secs(30),
                Err(WaitError::Cancelled) => return Err(ErrorCode::Cancelled),
                Err(WaitError::TimedOut) => return Err(ErrorCode::Timeout),
            };
            if queue.front() == Some(&self.id) {
                return Ok(());
            }
            queue = self
                .serial
                .changed
                .wait_timeout(queue, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}
impl Drop for Ticket {
    fn drop(&mut self) {
        let mut queue = self.serial.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.retain(|id| *id != self.id);
        self.serial.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dropped_head_and_cancelled_waiter_preserve_order_and_capacity() {
        let serial = Arc::new(Serial::default());
        let head = serial.enter().unwrap();
        let second = serial.enter().unwrap();
        let cancel = HttpCancel::default();
        cancel.cancel();
        assert_eq!(second.wait(&cancel), Err(ErrorCode::Cancelled));
        drop(second);
        let third = serial.enter().unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            third.wait(&HttpCancel::default()).unwrap();
            sent.send(()).unwrap();
        });
        assert!(received.recv_timeout(Duration::from_millis(30)).is_err());
        drop(head);
        received.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
        assert!(serial.queue.lock().unwrap().is_empty());
    }
}
