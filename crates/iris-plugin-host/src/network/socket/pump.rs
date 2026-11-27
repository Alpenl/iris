use super::{
    Action, Inner, Pending,
    protocol::{Data, Protocol, ReadStep},
};
use crate::actor::IoOutcome;
use crate::instance::Blob;
use crate::network::RequestGrant;
use iris_api::v2::{CHUNK_LIMIT, ErrorCode, Id};
use mio::{Events, Interest, Poll, Token};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

struct Pump {
    read: Option<Pending>,
    write: Option<Pending>,
    buffered: Option<Data>,
    offset: usize,
    started: bool,
    flush: bool,
    eof: bool,
    controls: u32,
    window: Instant,
}
pub(super) fn run(
    mut protocol: Protocol,
    mut poll: Poll,
    inner: Arc<Inner>,
    grant: Arc<RequestGrant>,
) {
    let mut pump = Pump {
        read: None,
        write: None,
        buffered: None,
        offset: 0,
        started: false,
        flush: false,
        eof: false,
        controls: 0,
        window: Instant::now(),
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pump.run(&mut protocol, &mut poll, &inner, &grant)
    }))
    .unwrap_or(Err(ErrorCode::Internal));
    let error = if inner.cancel.is_cancelled() {
        ErrorCode::Cancelled
    } else {
        result.err().unwrap_or(ErrorCode::IoError)
    };
    inner.cancel.cancel();
    let queued = {
        let mut queue = inner.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.closed = Some(error);
        std::mem::take(&mut queue.requests)
    };
    for request in queued.into_iter().chain(pump.read).chain(pump.write) {
        request.finish(Err(error));
    }
}
impl Pump {
    fn run(
        &mut self,
        protocol: &mut Protocol,
        poll: &mut Poll,
        inner: &Inner,
        grant: &RequestGrant,
    ) -> Result<(), ErrorCode> {
        let mut events = Events::with_capacity(4);
        let mut previous = None;
        loop {
            grant.check_live()?;
            if inner.cancel.is_cancelled() {
                return Err(ErrorCode::Cancelled);
            }
            let queued = {
                let mut queue = inner.queue.lock().unwrap_or_else(|e| e.into_inner());
                std::mem::take(&mut queue.requests)
            };
            for request in queued {
                if matches!(request.action, Action::Read(_)) {
                    self.read = Some(request);
                } else {
                    self.write = Some(request);
                    self.offset = 0;
                    self.started = false;
                }
            }
            if self.read.as_ref().is_some_and(|p| p.deadline.is_expired())
                || self.write.as_ref().is_some_and(|p| p.deadline.is_expired())
            {
                return Err(ErrorCode::Timeout);
            }
            let more = self.step(protocol, inner.ws)?;
            let writing = self.write.is_some() || self.flush || protocol.wire().wants_write();
            let reading =
                (!self.eof && self.buffered.is_none() && (inner.ws || self.read.is_some()))
                    || (writing && protocol.wire().wants_read());
            let interest = match (reading, writing) {
                (true, true) => Some(Interest::READABLE | Interest::WRITABLE),
                (true, false) => Some(Interest::READABLE),
                (false, true) => Some(Interest::WRITABLE),
                (false, false) => None,
            };
            if interest != previous {
                match (previous, interest) {
                    (None, Some(i)) => protocol.wire().register(poll.registry(), Token(0), i),
                    (Some(_), Some(i)) => protocol.wire().reregister(poll.registry(), Token(0), i),
                    (Some(_), None) => protocol.wire().deregister(poll.registry()),
                    _ => Ok(()),
                }
                .map_err(|_| ErrorCode::IoError)?;
                previous = interest;
            }
            let timeout = if more
                && !self.eof
                && self.buffered.is_none()
                && (inner.ws || self.read.is_some())
            {
                Some(Duration::ZERO)
            } else {
                let deadline = self
                    .read
                    .iter()
                    .chain(self.write.iter())
                    .map(|p| p.deadline)
                    .min();
                deadline.map(|d| d.instant().saturating_duration_since(Instant::now()))
            };
            match poll.poll(&mut events, timeout) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(ErrorCode::IoError),
            }
        }
    }
    fn step(&mut self, protocol: &mut Protocol, ws: bool) -> Result<bool, ErrorCode> {
        if let Some(write) = &self.write
            && let Action::Write { blob, text } = &write.action
            && protocol.write(&blob.bytes, *text, &mut self.offset, &mut self.started)?
        {
            let len = blob.bytes.len();
            self.write.take().unwrap().finish(Ok(IoOutcome::new(
                serde_json::json!({"written":Id(len as u64)}),
                None,
            )));
        }
        let mut drained_without_pending = false;
        if self.buffered.is_none() && !self.eof && (ws || self.read.is_some()) {
            let len = match self.read.as_ref().map(|r| &r.action) {
                Some(Action::Read(len)) => *len as usize,
                _ => CHUNK_LIMIT,
            };
            drained_without_pending = true;
            for _ in 0..32 {
                match protocol.read(len)? {
                    ReadStep::Data(data) => {
                        self.eof = data.eof;
                        self.buffered = Some(data);
                        self.flush |= ws;
                        break;
                    }
                    ReadStep::Control => {
                        if self.window.elapsed() >= Duration::from_secs(1) {
                            self.window = Instant::now();
                            self.controls = 0;
                        }
                        self.controls += 1;
                        if self.controls > 256 {
                            return Err(ErrorCode::QuotaExceeded);
                        }
                        self.flush = true;
                    }
                    ReadStep::Pending => {
                        drained_without_pending = false;
                        self.flush |= ws;
                        break;
                    }
                }
            }
        }
        if self.read.is_some() && (self.buffered.is_some() || self.eof) {
            let read = self.read.take().unwrap();
            let data = self.buffered.take().unwrap_or(Data {
                bytes: Vec::new(),
                text: false,
                eof: true,
            });
            let result =
                Blob::new(&read.context.ledger, &read.context.owner, data.bytes).map(|blob| {
                    IoOutcome::new(
                        serde_json::json!({"eof":data.eof,"text":data.text}),
                        Some(Arc::new(blob)),
                    )
                });
            read.finish(result);
        }
        if self.flush || protocol.wire().wants_write() {
            self.flush = !protocol.flush()?;
        }
        Ok(drained_without_pending)
    }
}
