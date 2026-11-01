//! Undo history for JMAP: a place is `{"mailboxIds", "keywords"}`. See
//! `crate::mail::place` and docs/BACKEND.md "Undo history" for the contract
//! every provider keeps.
//!
//! Not built yet: every entry point refuses with `undo_unsupported`.
use crate::mail::Account;
use crate::mail::place::{Located, PlaceItem, PlaceOutcome};
use serde_json::Value;

/// The exact shape of a JMAP place; unknown keys and out-of-range values are
/// refused (`invalid_params` or an `undo_*` code), never guessed at.
pub(crate) fn validate_place(_place: &Value) -> Result<(), &'static str> {
    Err("undo_unsupported")
}

impl super::Session {
    /// Read-only, live from the server: one entry per id, in order. `ids` are
    /// unique and already pass `crate::mail::action::validate_message_id`.
    pub(crate) async fn locate(
        &self,
        _account: &Account,
        _ids: &[String],
    ) -> Result<Vec<Located>, &'static str> {
        Err("undo_unsupported")
    }

    /// Moves each message to its place: one entry per item, in order. Places
    /// have passed `validate_place`; a message already there is
    /// `Placed { new_id: id, moved: false }` with no write.
    pub(crate) async fn place(
        &self,
        _account: &Account,
        _items: &[PlaceItem],
    ) -> Result<Vec<PlaceOutcome>, &'static str> {
        Err("undo_unsupported")
    }
}
