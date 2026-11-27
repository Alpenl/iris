use super::IoOutcome;
use crate::pool::IoPool;
use iris_api::v2::{ErrorCode, Role};
use std::sync::{Arc, Mutex};

pub type Completion = Box<dyn FnOnce(Result<IoOutcome, ErrorCode>) + Send>;
type Blocking = Box<dyn FnOnce() -> Result<IoOutcome, ErrorCode> + Send>;
type Deferred = Box<dyn FnOnce(Completion) -> Result<(), ErrorCode> + Send>;

/// Deferred operations only enqueue bounded work and wake an existing reactor.
/// They must never perform I/O, wait, or call guest code on the submitting thread.
pub enum Operation {
    Blocking(Blocking),
    Deferred(Deferred),
}
impl Operation {
    pub fn blocking(f: impl FnOnce() -> Result<IoOutcome, ErrorCode> + Send + 'static) -> Self {
        Self::Blocking(Box::new(f))
    }
    pub fn deferred(f: impl FnOnce(Completion) -> Result<(), ErrorCode> + Send + 'static) -> Self {
        Self::Deferred(Box::new(f))
    }
    pub(crate) fn dispatch(
        self,
        io: &IoPool,
        role: Role,
        owner: &str,
        done: Completion,
    ) -> Result<(), ErrorCode> {
        match self {
            Self::Blocking(work) => io.submit(
                role,
                owner,
                Box::new(move || {
                    done(
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                            .unwrap_or(Err(ErrorCode::Internal)),
                    );
                }),
            ),
            Self::Deferred(start) => {
                let terminal = Arc::new(Mutex::new(Some(done)));
                let target = terminal.clone();
                let callback = Box::new(move |result| {
                    let done = target.lock().unwrap_or_else(|e| e.into_inner()).take();
                    if let Some(done) = done {
                        done(result);
                    }
                });
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| start(callback)))
                        .unwrap_or(Err(ErrorCode::Internal));
                if let Err(error) = result {
                    let unclaimed = terminal.lock().unwrap_or_else(|e| e.into_inner()).take();
                    if unclaimed.is_some() {
                        return Err(error);
                    }
                }
                Ok(())
            }
        }
    }
}
