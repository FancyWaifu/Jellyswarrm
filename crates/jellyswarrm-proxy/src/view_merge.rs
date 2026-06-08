//! Library-view merge registry.
//!
//! With movie dedup on, same-type libraries across backends (e.g. each backend's
//! "Movies") are collapsed into one canonical view in `/Views`. This registry
//! remembers, for each kept canonical view id, the per-backend views it stands
//! in for — so when the user browses that merged library (a `ParentId` request),
//! the federation can fan out across every member backend and dedup, instead of
//! routing to a single backend.
//!
//! Members are keyed by `(server_id, that backend's view virtual id)`; the view
//! ids are global (per media-mapping, not per-user), so one registry serves all
//! users. It's rebuilt every time `/Views` is federated.

use std::collections::HashMap;
use std::sync::RwLock;

#[derive(Default)]
pub struct ViewMergeRegistry {
    /// canonical (kept) view id -> [(server_id, that backend's view virtual id)]
    groups: RwLock<HashMap<String, Vec<(i64, String)>>>,
}

impl ViewMergeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a merge group keyed by its canonical view id. A single-member
    /// group (library only on one backend) is not worth fanning out, so it's
    /// dropped from the registry.
    pub fn register(&self, canonical_view_id: String, members: Vec<(i64, String)>) {
        let mut groups = self.groups.write().unwrap();
        if members.len() > 1 {
            groups.insert(canonical_view_id, members);
        } else {
            groups.remove(&canonical_view_id);
        }
    }

    /// The member backends for a merged view, or `None` if `view_id` isn't a
    /// merged canonical view (browse it normally on its single backend).
    pub fn members(&self, view_id: &str) -> Option<Vec<(i64, String)>> {
        self.groups.read().unwrap().get(view_id).cloned()
    }
}
