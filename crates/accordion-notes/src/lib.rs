//! Accordion Notes: quick notes, checklists, labels, and colors, as in
//! Google Keep (decision 31 in the `accordion` repo's `docs/decisions`).
//!
//! Each note is a Markdown file in its owner's Notes place, which Drive
//! shows as a tab beside My Drive (decision 32), so notes sync to computers
//! and open in Obsidian or any editor. What the
//! cards show (color, pin, labels, archive) is in each file's front matter,
//! and Notes keeps an index of it in its own tables so lists are quick.

mod api;
pub mod note;
mod store;

use accordion_core::db::Migration;
use accordion_core::identity::Identity;
use accordion_core::storage::Storage;
use accordion_module::{BoxFuture, Module, Routes, Services, StartError, Started, Stopping};
use axum::extract::FromRef;

/// The name Notes' migrations are recorded under.
const OWNER: &str = "notes";

/// Notes' tables, in the order they were added.
static MIGRATIONS: &[Migration] = &[store::MIGRATION, store::MIGRATION_2];

/// The Notes module. Add it to the server's list of modules.
pub struct Notes;

impl Module for Notes {
    fn id(&self) -> &'static str {
        "notes"
    }

    fn name(&self) -> &'static str {
        "Notes"
    }

    /// Notes are files in Drive.
    fn requires(&self) -> &'static [&'static str] {
        &["drive"]
    }

    /// Everyone's Notes places, which are also part of Drive's files.
    /// Before Notes was first turned on, its tables don't exist, and it has
    /// none.
    fn data_size(&self, services: &Services) -> BoxFuture<Option<u64>> {
        let store = store::Store::new(services.db.clone(), services.storage.clone());
        Box::pin(async move { Some(store.size().await.unwrap_or(0)) })
    }

    fn start(
        &self,
        services: Services,
        _stopping: Stopping,
    ) -> BoxFuture<Result<Started, StartError>> {
        Box::pin(async move {
            services.db.migrate(OWNER, MIGRATIONS).await?;
            services.storage.add_place(store::PLACE);
            let state = NotesState {
                store: store::Store::new(services.db, services.storage.clone()),
                identity: services.identity,
                storage: services.storage,
            };
            Ok(Started {
                routes: Routes {
                    api: Some(api::routes(state)),
                    ..Routes::default()
                },
                background: None,
            })
        })
    }
}

#[derive(Clone)]
struct NotesState {
    identity: Identity,
    storage: Storage,
    store: store::Store,
}

impl FromRef<NotesState> for Identity {
    fn from_ref(state: &NotesState) -> Self {
        state.identity.clone()
    }
}
