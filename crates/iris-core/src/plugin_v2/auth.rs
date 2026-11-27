//! One-shot loopback OAuth transactions. The browser remains outside the sandbox.
mod callback;
use super::{
    actor::{Completion, Executor, IoOutcome, Operation, TaskContext},
    instance::{Blob, next_id},
    ledger::{Activity, Ledger, Reservation, Resource},
    network::{NetworkGrant, TransportKind, normalize_url},
};
use crate::{
    concurrency::{Deadline, ThreadRole, spawn_thread},
    vfs::HttpCancel,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use iris_api::v2::{ErrorCode, Generation, Id, Role, task::Task};
use mio::{Interest, Poll, Token, Waker};
use ring::{
    digest,
    rand::{SecureRandom, SystemRandom},
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use url::Url;

/// The UI hook only queues a bounded, user-visible browser-open request and wakes
/// the UI loop. It must not open a browser or wait on the calling Store thread.
pub type BrowserOpen = Arc<dyn Fn(Id, Generation, &str) -> Result<(), ErrorCode> + Send + Sync>;
struct WindowScope {
    generation: Option<Generation>,
    browser: BrowserOpen,
}
struct State {
    instance: Option<Id>,
    global_generation: Option<Generation>,
    windows: BTreeMap<Id, WindowScope>,
    stopped: bool,
    transactions: BTreeMap<u64, Arc<Transaction>>,
}
struct Inner {
    network: NetworkGrant,
    ledger: Ledger,
    browser: BrowserOpen,
    state: Mutex<State>,
    ttl: Duration,
    activity: Activity,
}
struct Transaction {
    id: Id,
    window: Option<Id>,
    instance: Id,
    generation: Generation,
    redirect: Url,
    state: String,
    verifier: String,
    challenge: String,
    issuer: Option<String>,
    expires: Deadline,
    cancel: HttpCancel,
    wake: Arc<Waker>,
    opened: AtomicBool,
    finished: AtomicBool,
    completion: Mutex<Option<Completion>>,
    _reservation: Reservation,
}
#[derive(Clone)]
pub struct AuthExecutor(Arc<Inner>);
impl AuthExecutor {
    pub fn new(network: NetworkGrant, ledger: Ledger, browser: BrowserOpen) -> Self {
        Self(Arc::new(Inner {
            network,
            ledger,
            browser,
            ttl: Duration::from_secs(300),
            activity: Activity::default(),
            state: Mutex::new(State {
                instance: None,
                global_generation: None,
                windows: BTreeMap::new(),
                stopped: false,
                transactions: BTreeMap::new(),
            }),
        }))
    }
    pub(crate) fn open_window(&self, window: Id, generation: Generation, browser: BrowserOpen) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.windows.insert(
            window,
            WindowScope {
                generation: Some(generation),
                browser,
            },
        );
    }
    fn bind(&self, context: &TaskContext) -> Result<(), ErrorCode> {
        if context.role != Role::Control || context.owner != self.0.network.owner() {
            return Err(ErrorCode::Denied);
        }
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.stopped || context.cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        if state.instance.is_some_and(|i| i != context.instance_id) {
            return Err(ErrorCode::Denied);
        }
        state.instance = Some(context.instance_id);
        match context.window {
            None => {
                if state
                    .global_generation
                    .is_some_and(|g| g.media != context.generation.media)
                {
                    return Err(ErrorCode::StaleHandle);
                }
                state.global_generation = Some(context.generation);
            }
            Some(win) => {
                let scope = state.windows.get_mut(&win).ok_or(ErrorCode::StaleHandle)?;
                if scope
                    .generation
                    .is_some_and(|g| g.media != context.generation.media)
                {
                    return Err(ErrorCode::StaleHandle);
                }
                scope.generation = Some(context.generation);
            }
        }
        Ok(())
    }
    fn begin(
        &self,
        path: String,
        issuer: Option<String>,
        context: TaskContext,
    ) -> Result<Operation, ErrorCode> {
        if path.len() > 128
            || !path.starts_with('/')
            || path.starts_with("//")
            || !path
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"/_-".contains(&c))
        {
            return Err(ErrorCode::InvalidArgument);
        }
        let issuer = issuer
            .map(|i| {
                let parsed = normalize_url(&i, TransportKind::Http)?;
                if parsed.scheme() != "https" || parsed.query().is_some() || i.contains('#') {
                    return Err(ErrorCode::InvalidArgument);
                }
                Ok(i)
            })
            .transpose()?;
        let reservation = self.0.ledger.reserve(
            &context.owner,
            &[
                (Resource::Auth, 1),
                (Resource::Handle, 1),
                (Resource::HostBytes, 65536),
            ],
        )?;
        let this = self.clone();
        Ok(Operation::blocking(move || {
            this.bind(&context)?;
            let mut listener = mio::net::TcpListener::bind(
                "127.0.0.1:0".parse().map_err(|_| ErrorCode::Internal)?,
            )
            .map_err(|_| ErrorCode::IoError)?;
            let address = listener.local_addr().map_err(|_| ErrorCode::IoError)?;
            let poll = Poll::new().map_err(|_| ErrorCode::IoError)?;
            poll.registry()
                .register(&mut listener, Token(0), Interest::READABLE)
                .map_err(|_| ErrorCode::IoError)?;
            let wake =
                Arc::new(Waker::new(poll.registry(), Token(1)).map_err(|_| ErrorCode::IoError)?);
            let cancel_wake = context
                .cancel
                .register_shared_wake(wake.clone())
                .map_err(|_| ErrorCode::Cancelled)?;
            let state = random()?;
            let verifier = random()?;
            let challenge =
                URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes()));
            let transaction = Arc::new(Transaction {
                id: Id(next_id()?),
                window: context.window,
                instance: context.instance_id,
                generation: context.generation,
                redirect: Url::parse(&format!("http://{address}{path}"))
                    .map_err(|_| ErrorCode::Internal)?,
                state,
                verifier,
                challenge,
                issuer,
                expires: Deadline::after(this.0.ttl),
                cancel: context.cancel,
                wake,
                opened: AtomicBool::new(false),
                finished: AtomicBool::new(false),
                completion: Mutex::new(None),
                _reservation: reservation,
            });
            let result = IoOutcome::new(
                serde_json::json!({"transaction":transaction.id,"redirect_uri":transaction.redirect.as_str(),
                "state":transaction.state,"code_challenge":transaction.challenge,"code_challenge_method":"S256","expires_in":this.0.ttl.as_secs()}),
                None,
            );
            let id = transaction.id.0;
            {
                let mut state = this.0.state.lock().unwrap_or_else(|e| e.into_inner());
                let scope_gen = match transaction.window {
                    None => state.global_generation,
                    Some(win) => match state.windows.get(&win) {
                        Some(scope) => scope.generation,
                        None => return Err(ErrorCode::Cancelled),
                    },
                };
                if state.stopped
                    || transaction.cancel.is_cancelled()
                    || scope_gen.is_some_and(|g| g.media != transaction.generation.media)
                {
                    return Err(ErrorCode::Cancelled);
                }
                state.transactions.insert(id, transaction.clone());
            }
            let weak = Arc::downgrade(&this.0);
            let activity = this.0.activity.start();
            if spawn_thread(ThreadRole::PluginAuthCallback, move || {
                let _activity = activity;
                let _cancel_wake = cancel_wake;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    callback::listen(listener, poll, &transaction)
                }))
                .unwrap_or(Err(ErrorCode::Internal));
                transaction.finish(result);
                if let Some(inner) = weak.upgrade() {
                    inner
                        .state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .transactions
                        .remove(&id);
                }
                drop(transaction);
            })
            .is_err()
            {
                this.close_in(context.window, id)?;
                return Err(ErrorCode::Internal);
            }
            Ok(result)
        }))
    }
    fn open(
        &self,
        id: Id,
        url: String,
        origin: Option<Id>,
        context: TaskContext,
    ) -> Result<Operation, ErrorCode> {
        let (transaction, browser) = {
            let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
            let transaction = state
                .transactions
                .get(&id.0)
                .cloned()
                .ok_or(ErrorCode::StaleHandle)?;
            let browser = match context.window {
                None => self.0.browser.clone(),
                Some(win) => state
                    .windows
                    .get(&win)
                    .ok_or(ErrorCode::StaleHandle)?
                    .browser
                    .clone(),
            };
            (transaction, browser)
        };
        if transaction.window != context.window
            || transaction.instance != context.instance_id
            || transaction.generation.media != context.generation.media
        {
            return Err(ErrorCode::StaleHandle);
        }
        let grant = self.0.network.request(&url, TransportKind::Http, origin)?;
        let url = normalize_url(&url, TransportKind::Http)?;
        if url.scheme() != "https" {
            return Err(ErrorCode::Denied);
        }
        let pairs = callback::query(&url)?;
        if pairs.get("state") != Some(&transaction.state)
            || pairs.get("redirect_uri").map(String::as_str) != Some(transaction.redirect.as_str())
            || pairs.get("code_challenge") != Some(&transaction.challenge)
            || pairs.get("code_challenge_method").map(String::as_str) != Some("S256")
            || pairs.get("response_type").map(String::as_str) != Some("code")
            || pairs
                .get("client_id")
                .is_none_or(|id| id.is_empty() || id.len() > 256)
            || pairs.get("response_mode").is_some_and(|m| m != "query")
        {
            return Err(ErrorCode::InvalidArgument);
        }
        Ok(Operation::deferred(move |done| {
            grant.check_live()?;
            context
                .cancel
                .link(&transaction.cancel)
                .map_err(|_| ErrorCode::Cancelled)?;
            grant
                .cancel()
                .link(&transaction.cancel)
                .map_err(|_| ErrorCode::Cancelled)?;
            let mut completion = transaction
                .completion
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if transaction.finished.load(Ordering::Acquire) || transaction.cancel.is_cancelled() {
                return Err(ErrorCode::Cancelled);
            }
            if transaction.expires.is_expired() {
                return Err(ErrorCode::Timeout);
            }
            if transaction.opened.swap(true, Ordering::AcqRel) {
                return Err(ErrorCode::BadState);
            }
            *completion = Some(Box::new(move |result| {
                let _grant = grant;
                done(result);
            }));
            drop(completion);
            if let Err(error) = browser(id, context.generation, url.as_str()) {
                transaction.finish(Err(error));
                transaction.cancel.cancel();
            }
            let _ = transaction.wake.wake();
            Ok(())
        }))
    }
}
impl Executor for AuthExecutor {
    fn is_quiescent(&self) -> bool {
        self.0.activity.is_idle()
    }
    fn prepare(
        &self,
        task: Task,
        _blobs: &BTreeMap<u64, Arc<Blob>>,
        context: TaskContext,
    ) -> Result<Operation, ErrorCode> {
        self.bind(&context)?;
        match task {
            Task::BeginAuth {
                callback_path,
                issuer,
            } => self.begin(callback_path, issuer, context),
            Task::OpenAuth {
                transaction,
                url,
                origin_grant,
            } => self.open(transaction, url, origin_grant, context),
            _ => Err(ErrorCode::Unsupported),
        }
    }
    fn close(&self, id: u64) -> Result<(), ErrorCode> {
        self.close_in(None, id)
    }
    fn close_in(&self, window: Option<Id>, id: u64) -> Result<(), ErrorCode> {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .transactions
            .get(&id)
            .is_none_or(|tx| tx.window != window)
        {
            return Err(ErrorCode::StaleHandle);
        }
        let transaction = state.transactions.remove(&id).unwrap();
        drop(state);
        transaction.cancel.cancel();
        Ok(())
    }
    fn stop(&self) {
        let transactions = {
            let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
            state.stopped = true;
            state.windows.clear();
            std::mem::take(&mut state.transactions)
        };
        for (_, transaction) in transactions {
            transaction.cancel.cancel();
        }
    }
    fn generation_changed(&self, generation: Generation) {
        let cancelled = {
            let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
            let media_changed = state
                .global_generation
                .is_some_and(|old| old.media != generation.media);
            state.global_generation = Some(generation);
            if media_changed {
                let ids: Vec<_> = state
                    .transactions
                    .iter()
                    .filter_map(|(&id, tx)| tx.window.is_none().then_some(id))
                    .collect();
                ids.into_iter()
                    .filter_map(|id| state.transactions.remove(&id))
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            }
        };
        for transaction in cancelled {
            transaction.cancel.cancel();
        }
    }
    fn window_changed(&self, window: Id, generation: Option<Generation>) {
        let cancelled = {
            let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
            let cancel_all = match generation {
                Some(next) => {
                    if let Some(scope) = state.windows.get_mut(&window) {
                        let media_changed =
                            scope.generation.is_some_and(|old| old.media != next.media);
                        scope.generation = Some(next);
                        media_changed
                    } else {
                        false
                    }
                }
                None => {
                    state.windows.remove(&window);
                    true
                }
            };
            if cancel_all {
                let ids: Vec<_> = state
                    .transactions
                    .iter()
                    .filter_map(|(&id, tx)| (tx.window == Some(window)).then_some(id))
                    .collect();
                ids.into_iter()
                    .filter_map(|id| state.transactions.remove(&id))
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            }
        };
        for transaction in cancelled {
            transaction.cancel.cancel();
        }
    }
}
impl Drop for Inner {
    fn drop(&mut self) {
        for transaction in self
            .state
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .transactions
            .values()
        {
            transaction.cancel.cancel();
        }
    }
}
impl Transaction {
    fn finish(&self, result: Result<IoOutcome, ErrorCode>) {
        let done = {
            let mut completion = self.completion.lock().unwrap_or_else(|e| e.into_inner());
            self.finished.store(true, Ordering::Release);
            completion.take()
        };
        if let Some(done) = done {
            done(result);
        }
    }
}
fn random() -> Result<String, ErrorCode> {
    let mut bytes = [0; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| ErrorCode::Internal)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
mod tests;
