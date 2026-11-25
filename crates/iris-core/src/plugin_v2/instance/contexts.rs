use super::{Blob, Instance};
use crate::plugin_v2::ledger::Reservation;
use iris_api::v2::{Generation, Id};
use std::{collections::BTreeMap, sync::Arc};
pub(super) struct BlobContext {
    generation: Generation,
    blobs: BTreeMap<u64, Arc<Blob>>,
    handles: BTreeMap<u64, Reservation>,
}
fn same_session(a: Generation, b: Generation) -> bool {
    a.media == b.media
}
impl Instance {
    /// Park host handles between window callbacks. A guest cannot read another
    /// window's buffers just by keeping its numeric handles in shared Wasm memory.
    /// StreamStore is dedicated to a single playback session, so its session-level
    /// key/context Blobs survive ordinary seeks within the same media generation.
    pub(crate) fn select_context(&mut self, window: Option<Id>, generation: Generation) {
        let (previous, old) = self.current_context;
        let changed = match (previous, window) {
            (None, None) => !same_session(old, generation),
            (Some(a), Some(b)) => a != b || old != generation,
            _ => true,
        };
        if changed {
            let state = self.store.data_mut();
            let parked = BlobContext {
                generation: old,
                blobs: std::mem::take(&mut state.blobs),
                handles: std::mem::take(&mut state.blob_handles),
            };
            if !parked.blobs.is_empty() {
                self.parked.insert(previous, parked);
            }
            if let Some(saved) = self.parked.remove(&window).filter(|saved| {
                if window.is_some() {
                    saved.generation == generation
                } else {
                    same_session(saved.generation, generation)
                }
            }) {
                state.blobs = saved.blobs;
                state.blob_handles = saved.handles;
            }
        }
        self.current_context = (window, generation);
        self.store.data_mut().services.on_window(window);
    }
    pub(crate) fn retain_contexts(
        &mut self,
        global: Generation,
        windows: &BTreeMap<Id, Generation>,
    ) {
        let live = |window: Option<Id>, generation: Generation| match window {
            None => same_session(global, generation),
            Some(window) => windows.get(&window).is_some_and(|g| *g == generation),
        };
        self.parked
            .retain(|window, scope| live(*window, scope.generation));
        if !live(self.current_context.0, self.current_context.1) {
            self.store.data_mut().blobs.clear();
            self.store.data_mut().blob_handles.clear();
        }
    }
}
