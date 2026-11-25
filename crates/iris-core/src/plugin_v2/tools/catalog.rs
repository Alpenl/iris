//! Host-owned bindings. Inspection never executes a candidate. Approval is a
//! trusted native action; a digest pins identity, not publisher authenticity.
mod receipt;
use super::binding::{ToolKind, TrustedModel, TrustedTool};
use crate::{fs_util, vfs::HttpCancel};
use iris_api::v2::{ErrorCode, Id};
pub use receipt::{Receipt, Selection};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

struct Entry<T> {
    value: Arc<T>,
    cancel: HttpCancel,
}

struct State {
    receipts: Vec<Receipt>,
    tools: BTreeMap<ToolKind, Entry<TrustedTool>>,
    model: Option<(Id, Entry<TrustedModel>)>,
    error: Option<ErrorCode>,
}
struct Inner {
    path: Option<PathBuf>,
    state: Mutex<State>,
    job: Mutex<()>,
    restored: std::sync::atomic::AtomicBool,
}
#[derive(Clone)]
pub struct ToolCatalog(Arc<Inner>);
impl Default for ToolCatalog {
    fn default() -> Self {
        Self::new(None)
    }
}
impl ToolCatalog {
    pub fn new(path: Option<PathBuf>) -> Self {
        Self(Arc::new(Inner {
            path,
            state: Mutex::new(State {
                receipts: Vec::new(),
                tools: BTreeMap::new(),
                model: None,
                error: None,
            }),
            job: Mutex::new(()),
            restored: std::sync::atomic::AtomicBool::new(false),
        }))
    }
    pub fn summary(&self) -> (Vec<Receipt>, Option<ErrorCode>) {
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.receipts.clone(), state.error)
    }
    pub fn inspect(
        &self,
        selection: Selection,
        path: &std::path::Path,
        cancel: &HttpCancel,
    ) -> Result<Receipt, ErrorCode> {
        let _job = self.0.job.try_lock().map_err(|_| ErrorCode::Backpressure)?;
        Receipt::inspect(selection, path, cancel)
    }
    /// Restore only previously confirmed records, on the background coordinator.
    /// Corruption never becomes an empty successful configuration.
    pub fn restore(&self, cwd: &std::path::Path) -> Result<(), ErrorCode> {
        let _job = self.0.job.try_lock().map_err(|_| ErrorCode::Backpressure)?;
        if self
            .0
            .restored
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return Ok(());
        }
        let result = (|| {
            let Some(path) = &self.0.path else {
                return Ok(());
            };
            let bytes = match std::fs::File::open(path).and_then(|file| {
                use std::io::Read;
                let mut bytes = Vec::new();
                file.take((512 << 10) + 1).read_to_end(&mut bytes)?;
                Ok(bytes)
            }) {
                Ok(bytes) if bytes.len() <= 512 << 10 => bytes,
                Ok(_) => return Err(ErrorCode::QuotaExceeded),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(_) => return Err(ErrorCode::IoError),
            };
            let receipts: Vec<Receipt> = crate::plugin_v2::json::decode(&bytes, 512 << 10)?;
            if receipts.len() > 5 {
                return Err(ErrorCode::QuotaExceeded);
            }
            let mut seen = std::collections::BTreeSet::new();
            for receipt in &receipts {
                if !seen.insert(receipt.selection) {
                    return Err(ErrorCode::InvalidArgument);
                }
            }
            for receipt in receipts {
                self.install(receipt, cwd, &HttpCancel::default(), false)?;
            }
            Ok(())
        })();
        self.0.state.lock().unwrap_or_else(|e| e.into_inner()).error = result.err();
        result
    }
    pub fn approve(
        &self,
        receipt: Receipt,
        cwd: &std::path::Path,
        cancel: &HttpCancel,
    ) -> Result<(), ErrorCode> {
        let _job = self.0.job.try_lock().map_err(|_| ErrorCode::Backpressure)?;
        self.install(receipt, cwd, cancel, true)
    }
    fn install(
        &self,
        receipt: Receipt,
        cwd: &std::path::Path,
        cancel: &HttpCancel,
        persist: bool,
    ) -> Result<(), ErrorCode> {
        // Validation and version queries happen only after approval. New data is
        // committed before revoking an existing binding; failed updates preserve it.
        std::fs::create_dir_all(cwd).map_err(|_| ErrorCode::IoError)?;
        let validated = receipt.approve(cwd, cancel)?;
        if cancel.is_cancelled() {
            return Err(ErrorCode::Cancelled);
        }
        let mut next = self
            .0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .receipts
            .clone();
        next.retain(|r| r.selection != receipt.selection);
        next.push(receipt.clone());
        if persist {
            self.save(&next)?;
        }
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        match validated {
            receipt::Approved::Tool(tool) => {
                if let Some(old) = state.tools.insert(
                    tool.kind,
                    Entry {
                        value: tool,
                        cancel: HttpCancel::default(),
                    },
                ) {
                    old.cancel.cancel();
                }
            }
            receipt::Approved::Model(model) => {
                let id = Id(crate::plugin_v2::instance::next_id()?);
                if let Some((_, old)) = state.model.replace((
                    id,
                    Entry {
                        value: model,
                        cancel: HttpCancel::default(),
                    },
                )) {
                    old.cancel.cancel();
                }
            }
        }
        state.receipts = next;
        state.error = None;
        Ok(())
    }
    pub fn remove(&self, selection: Selection) -> Result<(), ErrorCode> {
        let _job = self.0.job.try_lock().map_err(|_| ErrorCode::Backpressure)?;
        let mut next = self
            .0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .receipts
            .clone();
        next.retain(|r| r.selection != selection);
        self.save(&next)?;
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        match selection {
            Selection::Tool(kind) => {
                if let Some(old) = state.tools.remove(&kind) {
                    old.cancel.cancel();
                }
            }
            Selection::WhisperModel => {
                if let Some((_, old)) = state.model.take() {
                    old.cancel.cancel();
                }
            }
        }
        state.receipts = next;
        state.error = None;
        Ok(())
    }
    fn save(&self, receipts: &[Receipt]) -> Result<(), ErrorCode> {
        if let Some(path) = &self.0.path {
            let bytes = crate::plugin_v2::json::encode(&receipts, 512 << 10)?;
            fs_util::atomic_write_private(path, bytes).map_err(|_| ErrorCode::IoError)?;
        }
        Ok(())
    }
    pub(super) fn tool(
        &self,
        kind: ToolKind,
        cancel: &HttpCancel,
    ) -> Result<Arc<TrustedTool>, ErrorCode> {
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        let entry = state.tools.get(&kind).ok_or(ErrorCode::Unsupported)?;
        entry
            .cancel
            .link(cancel)
            .map_err(|_| ErrorCode::Cancelled)?;
        Ok(entry.value.clone())
    }
    pub(super) fn model(
        &self,
        id: Id,
        cancel: &HttpCancel,
    ) -> Result<Arc<TrustedModel>, ErrorCode> {
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        let (actual, entry) = state.model.as_ref().ok_or(ErrorCode::Unsupported)?;
        if id != *actual {
            return Err(ErrorCode::StaleHandle);
        }
        entry
            .cancel
            .link(cancel)
            .map_err(|_| ErrorCode::Cancelled)?;
        Ok(entry.value.clone())
    }
    pub(super) fn availability(&self) -> (Vec<ToolKind>, Vec<Id>) {
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        (
            state.tools.keys().copied().collect(),
            state.model.iter().map(|(id, _)| *id).collect(),
        )
    }
}

#[cfg(test)]
mod tests;
