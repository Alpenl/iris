//! Shortcut recording normalization, reserved-chord protection, scope-aware
//! conflict detection, scheme switching, and binding reset on [`super::WorkbenchController`].

use iris_core::keymap::{self, ConflictTarget, KeymapScheme};

use super::{WorkbenchController, WorkbenchDomainChanges};

/// Typed domain result of recording one normalized keystroke for `target_id`.
#[derive(Clone, Debug, PartialEq)]
pub enum ShortcutRecordOutcome {
    /// Chord could not be normalized (e.g. bare modifier press); recorder stays armed.
    Ignored,
    /// `Escape` pressed; cancel active recording without modifying bindings.
    Cancelled,
    /// Reserved emergency shortcut (`reset_appearance`) pressed; pass through to action system.
    PassThroughReserved(&'static str),
    /// Chord conflicts with an existing reserved/action/preset/plugin shortcut in overlapping scope.
    Conflict {
        display: String,
        target: ConflictTarget,
    },
    /// Chord bound and persisted to `keymap.json`; caller should rebind GPUI keys.
    Bound {
        display: String,
        changes: WorkbenchDomainChanges,
    },
}

impl WorkbenchController {
    /// Returns the formatted display label for `action_id` under the active `Keymap`, if bound.
    pub fn key_hint_string(&self, action_id: &str, is_macos: bool) -> Option<String> {
        let canonical = match action_id {
            "open_stream_url" => "open_url",
            other => other,
        };
        let raw = self.keymap.effective_key(canonical);
        if raw.is_empty() {
            return None;
        }
        let disp = keymap::format_keystroke_display(&raw, is_macos);
        (!disp.is_empty()).then_some(disp)
    }

    /// Evaluates a raw GPUI keystroke against the active shortcut recorder target.
    #[allow(clippy::too_many_arguments)]
    pub fn record_shortcut_keystroke(
        &mut self,
        target_id: &str,
        context_pred: &str,
        key: &str,
        modifiers_cmd: bool,
        modifiers_ctrl: bool,
        modifiers_alt: bool,
        modifiers_shift: bool,
        is_macos: bool,
    ) -> ShortcutRecordOutcome {
        let Some(spec) = keymap::normalize_gpui_keystroke(
            key,
            modifiers_cmd,
            modifiers_ctrl,
            modifiers_alt,
            modifiers_shift,
        ) else {
            return ShortcutRecordOutcome::Ignored;
        };
        self.record_normalized_shortcut(target_id, context_pred, &spec, is_macos)
    }

    /// Evaluates an already-normalized portable shortcut spec (e.g. `"secondary-k"`, `"escape"`).
    pub fn record_normalized_shortcut(
        &mut self,
        target_id: &str,
        context_pred: &str,
        spec: &str,
        is_macos: bool,
    ) -> ShortcutRecordOutcome {
        let Some(canonical) = keymap::canonical_binding(spec) else {
            return ShortcutRecordOutcome::Ignored;
        };
        if canonical == "escape" {
            return ShortcutRecordOutcome::Cancelled;
        }
        if keymap::reserved_shortcut(&canonical) == Some("reset_appearance") {
            return ShortcutRecordOutcome::PassThroughReserved("reset_appearance");
        }
        let display = keymap::format_keystroke_display(&canonical, is_macos);
        if let Some(conflict) = self
            .keymap
            .find_conflict(context_pred, &canonical, target_id)
        {
            return ShortcutRecordOutcome::Conflict {
                display,
                target: conflict,
            };
        }
        self.keymap.set_binding(target_id, canonical);
        ShortcutRecordOutcome::Bound {
            display,
            changes: Self::keymap_domain_changes(),
        }
    }

    pub fn reset_shortcut_binding(&mut self, target_id: &str) -> WorkbenchDomainChanges {
        self.keymap.reset_binding(target_id);
        Self::keymap_domain_changes()
    }

    pub fn reset_all_shortcuts(&mut self) -> WorkbenchDomainChanges {
        self.keymap.reset_all();
        Self::keymap_domain_changes()
    }

    pub fn set_keymap_scheme(&mut self, scheme: KeymapScheme) -> WorkbenchDomainChanges {
        self.keymap.set_scheme(scheme);
        Self::keymap_domain_changes()
    }

    const fn keymap_domain_changes() -> WorkbenchDomainChanges {
        WorkbenchDomainChanges {
            settings_changed: true,
            sidebar_changed: true,
            dock_changed: true,
            titlebar_changed: true,
            keymap_changed: true,
            ..WorkbenchDomainChanges::empty()
        }
    }
}
