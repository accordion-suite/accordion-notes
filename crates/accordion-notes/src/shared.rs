//! Notes other people shared (decision 31): finding them, and what each
//! person chooses for one. A note's color belongs to the note, in its file,
//! as in Keep. Pins, the archive, and labels are each person's own, kept
//! here, and never written to the owner's file.

use std::collections::{HashMap, HashSet};

use accordion_core::db::{Database, Migration};
use accordion_core::storage::{Access, Error, Item, ItemKind, Storage};
use accordion_types::{ItemId, UserId};

use crate::note;
use crate::store::PLACE;

pub(crate) const MIGRATION: Migration = Migration {
    version: 4,
    name: "Keep each person's choices for notes shared with them",
    sql: "
    CREATE TABLE notes_choices (
        user_id BLOB NOT NULL CHECK (length(user_id) = 16),
        item_id BLOB NOT NULL CHECK (length(item_id) = 16),
        pinned INTEGER NOT NULL CHECK (pinned IN (0, 1)),
        archived INTEGER NOT NULL CHECK (archived IN (0, 1)),
        -- A JSON list.
        tags TEXT NOT NULL,
        PRIMARY KEY (user_id, item_id)
    ) STRICT, WITHOUT ROWID;
    ",
};

/// What someone chose for a note another person shared with them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Choices {
    pub pinned: bool,
    pub archived: bool,
    pub tags: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct Shared {
    db: Database,
    storage: Storage,
}

impl Shared {
    pub(crate) fn new(db: Database, storage: Storage) -> Self {
        Self { db, storage }
    }

    /// The notes others shared with `user`, directly or inside a shared
    /// folder of their Notes, each with how far `user` can go. Only files in
    /// someone's Notes place count, so a Markdown file shared from My Drive
    /// stays in Drive.
    pub(crate) async fn notes(&self, user: UserId) -> Result<Vec<(Item, Access)>, Error> {
        let mut found = Vec::new();
        let mut seen = HashSet::new();
        let mut places: HashMap<UserId, Option<ItemId>> = HashMap::new();
        for shared in self.storage.shared_with(user).await? {
            let item = shared.item;
            let owner = item.owner;
            let place = match places.get(&owner) {
                Some(place) => *place,
                None => {
                    let place = self.notes_place(owner).await?;
                    places.insert(owner, place);
                    place
                }
            };
            let Some(place) = place else { continue };
            if self.storage.path_within(place, item.id).await?.is_none() {
                continue;
            }
            let access = Access::from(shared.share.level);
            let mut waiting = vec![item];
            while let Some(item) = waiting.pop() {
                if !seen.insert(item.id) {
                    continue;
                }
                match item.kind {
                    ItemKind::Folder => waiting.extend(self.storage.children(item.id).await?),
                    ItemKind::File { .. } if note::is_note(&item.name) => {
                        found.push((item, access));
                    }
                    ItemKind::File { .. } => {}
                }
            }
        }
        Ok(found)
    }

    /// One note someone else shared with `user`, or `None`.
    pub(crate) async fn note(
        &self,
        user: UserId,
        id: ItemId,
    ) -> Result<Option<(Item, Access)>, Error> {
        let Some(item) = self.storage.item(id).await? else {
            return Ok(None);
        };
        if item.owner == user
            || !matches!(item.kind, ItemKind::File { .. })
            || !note::is_note(&item.name)
        {
            return Ok(None);
        }
        let Some(access) = self.storage.access(user, id).await? else {
            return Ok(None);
        };
        let Some(place) = self.notes_place(item.owner).await? else {
            return Ok(None);
        };
        if self.storage.path_within(place, id).await?.is_none() {
            return Ok(None);
        }
        Ok(Some((item, access)))
    }

    /// Someone's Notes place, if they have one. Unlike
    /// `Storage::place_folder`, this never makes one.
    async fn notes_place(&self, owner: UserId) -> Result<Option<ItemId>, Error> {
        Ok(self
            .storage
            .own_places(owner)
            .await?
            .into_iter()
            .find(|place| place.place.as_deref() == Some(PLACE.id))
            .map(|place| place.id))
    }

    /// What `user` chose for each note they have choices for.
    pub(crate) async fn choices(&self, user: UserId) -> Result<HashMap<ItemId, Choices>, Error> {
        self.db
            .read(move |conn| {
                let mut statement = conn.prepare(
                    "SELECT item_id, pinned, archived, tags FROM notes_choices
                     WHERE user_id = ?1",
                )?;
                let rows = statement.query_map([user.as_uuid()], |row| {
                    Ok((
                        ItemId::from_uuid(row.get(0)?),
                        Choices {
                            pinned: row.get(1)?,
                            archived: row.get(2)?,
                            tags: tags_from(&row.get::<_, String>(3)?),
                        },
                    ))
                })?;
                Ok(rows.collect::<Result<_, _>>()?)
            })
            .await
    }

    pub(crate) async fn set(
        &self,
        user: UserId,
        item: ItemId,
        choices: Choices,
    ) -> Result<(), Error> {
        let tags = serde_json::to_string(&choices.tags).unwrap_or_else(|_| "[]".into());
        self.db
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO notes_choices (user_id, item_id, pinned, archived, tags)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (user_id, item_id) DO UPDATE SET
                         pinned = excluded.pinned,
                         archived = excluded.archived,
                         tags = excluded.tags",
                    (
                        user.as_uuid(),
                        item.as_uuid(),
                        choices.pinned,
                        choices.archived,
                        tags,
                    ),
                )?;
                Ok(())
            })
            .await
    }
}

fn tags_from(json: &str) -> Vec<String> {
    serde_json::from_str(json).unwrap_or_default()
}
