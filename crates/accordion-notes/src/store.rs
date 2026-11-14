//! Each person's Notes place, and the index of their notes (decisions 31
//! and 32).
//!
//! The notes are files in the person's Notes place, beside their My Drive,
//! and the files are the truth.
//! The index keeps what the list of notes shows, read from each file, so
//! listing thousands of notes on a Raspberry Pi doesn't read thousands of
//! files. It follows the person's changes the way a synced device does,
//! through storage's list of changes, so notes changed in Obsidian, over
//! WebDAV, or in Drive show up too.
//!
//! Notes owns these tables (decision 1), so it names items only by ID and
//! never joins core's tables.

use std::collections::HashSet;
use std::sync::Arc;

use accordion_core::db::rusqlite::{self, OptionalExtension, Row};
use accordion_core::db::{Database, Migration};
use accordion_core::storage::{AppPlace, Error, Item, ItemKind, Storage, SyncChange};
use accordion_types::{ItemId, UserId};
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;

use crate::note::{self, NoteFile};

pub(crate) const MIGRATION: Migration = Migration {
    version: 1,
    name: "Create the Notes folders and index",
    sql: "
    -- Each person's Notes folder, and where the index follows their changes
    -- from. A null cursor lists the folder again.
    CREATE TABLE notes_folders (
        user_id BLOB PRIMARY KEY NOT NULL CHECK (length(user_id) = 16),
        folder_id BLOB NOT NULL CHECK (length(folder_id) = 16),
        cursor TEXT
    ) STRICT, WITHOUT ROWID;

    -- Each note, as its file last said.
    CREATE TABLE notes_index (
        item_id BLOB PRIMARY KEY NOT NULL CHECK (length(item_id) = 16),
        user_id BLOB NOT NULL CHECK (length(user_id) = 16),
        hash TEXT NOT NULL,
        name TEXT NOT NULL,
        color TEXT,
        pinned INTEGER NOT NULL CHECK (pinned IN (0, 1)),
        archived INTEGER NOT NULL CHECK (archived IN (0, 1)),
        -- A JSON list.
        tags TEXT NOT NULL,
        -- The start of the text, for cards and searching.
        body TEXT NOT NULL,
        created_at TEXT NOT NULL,
        modified_at TEXT NOT NULL
    ) STRICT, WITHOUT ROWID;
    CREATE INDEX notes_index_by_user ON notes_index (user_id);

    -- Labels someone made before giving them to any note.
    CREATE TABLE notes_labels (
        user_id BLOB NOT NULL CHECK (length(user_id) = 16),
        label TEXT NOT NULL,
        PRIMARY KEY (user_id, label)
    ) STRICT, WITHOUT ROWID;
    ",
};

pub(crate) const MIGRATION_2: Migration = Migration {
    version: 2,
    name: "Keep notes in a place of their own, and know each note's folder",
    sql: "
    -- The folder each note is in, or null at the top of the Notes place.
    ALTER TABLE notes_index ADD COLUMN folder_id BLOB
        CHECK (folder_id IS NULL OR length(folder_id) = 16);
    -- Notes moved from a folder in My Drive to a place of their own
    -- (decision 32), so each index is made again from there. What was in
    -- the old folder stays in My Drive.
    DELETE FROM notes_index;
    DELETE FROM notes_folders;
    ",
};

/// Notes' place beside My Drive.
pub(crate) const PLACE: AppPlace = AppPlace {
    id: "notes",
    name: "Notes",
};

/// How much of a file is read for the index. Longer notes are searched by
/// their start only.
const READ_LIMIT: u64 = 1024 * 1024;
/// How much of a note's text the index keeps.
const BODY_CHARS: usize = 20_000;

/// A note as the index has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Indexed {
    pub id: ItemId,
    pub hash: String,
    pub name: String,
    /// The folder it's in, which may be the top of the Notes place.
    pub folder: Option<ItemId>,
    pub details: note::Details,
    pub body: String,
    pub created_at: String,
    pub modified_at: String,
}

#[derive(Clone)]
pub(crate) struct Store {
    db: Database,
    storage: Storage,
    /// One person's index changes at a time, so two requests don't both
    /// follow the same changes.
    following: Arc<Mutex<()>>,
}

impl Store {
    pub(crate) fn new(db: Database, storage: Storage) -> Self {
        Self {
            db,
            storage,
            following: Arc::new(Mutex::new(())),
        }
    }

    /// `user`'s Notes place, made the first time they need it.
    pub(crate) async fn folder(&self, user: UserId) -> Result<ItemId, Error> {
        let top = self.storage.place_folder(user, PLACE.id).await?;
        if self.saved_folder(user).await? != Some(top) {
            self.db
                .write(move |tx| {
                    tx.execute(
                        "INSERT INTO notes_folders (user_id, folder_id, cursor)
                         VALUES (?1, ?2, NULL)
                         ON CONFLICT (user_id) DO UPDATE SET
                             folder_id = excluded.folder_id, cursor = NULL",
                        (user.as_uuid(), top.as_uuid()),
                    )?;
                    Ok::<_, Error>(())
                })
                .await?;
        }
        Ok(top)
    }

    async fn saved_folder(&self, user: UserId) -> Result<Option<ItemId>, Error> {
        self.db
            .read(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT folder_id FROM notes_folders WHERE user_id = ?1",
                        [user.as_uuid()],
                        |row| Ok(ItemId::from_uuid(row.get(0)?)),
                    )
                    .optional()?)
            })
            .await
    }

    /// Every folder inside `top`, however deep, outside the trash, each
    /// with the folder it's in, sorted by name within each folder.
    pub(crate) async fn folders(&self, top: ItemId) -> Result<Vec<Item>, Error> {
        let mut found = Vec::new();
        let mut waiting = vec![top];
        while let Some(folder) = waiting.pop() {
            for child in self.storage.children(folder).await? {
                if child.kind == ItemKind::Folder {
                    waiting.push(child.id);
                    found.push(child);
                }
            }
        }
        Ok(found)
    }

    /// Whether `id` is one of `user`'s notes: their file, inside their
    /// Notes folder, outside the trash.
    pub(crate) async fn is_note(
        &self,
        user: UserId,
        folder: ItemId,
        item: &Item,
    ) -> Result<bool, Error> {
        Ok(item.owner == user
            && matches!(item.kind, ItemKind::File { .. })
            && note::is_note(&item.name)
            && self.storage.path_within(folder, item.id).await?.is_some())
    }

    /// Brings `user`'s index up to date with their files.
    pub(crate) async fn follow(&self, user: UserId, folder: ItemId) -> Result<(), Error> {
        let _turn = self.following.lock().await;
        let cursor = self
            .db
            .read(move |conn| {
                Ok::<_, Error>(conn.query_row(
                    "SELECT cursor FROM notes_folders WHERE user_id = ?1",
                    [user.as_uuid()],
                    |row| row.get::<_, Option<String>>(0),
                )?)
            })
            .await?;
        let Some(mut cursor) = cursor else {
            return self.list_again(user, folder).await;
        };
        let mut prune = false;
        loop {
            let page = match self.storage.changes(user, &cursor).await {
                Ok(page) => page,
                Err(Error::StartOver) => return self.list_again(user, folder).await,
                Err(err) => return Err(err),
            };
            for change in page.changes {
                match change {
                    SyncChange::Changed(entry) => {
                        let item = entry.item;
                        if item.kind == ItemKind::Folder {
                            // A folder of notes moved in or out, or was
                            // restored, so everything inside changed.
                            if entry.whole || item.id != folder {
                                prune = true;
                                if self.storage.path_within(folder, item.id).await?.is_some() {
                                    self.add_folder(user, folder, item.id).await?;
                                }
                            }
                        } else if self.is_note(user, folder, &item).await? {
                            self.add(user, &item).await?;
                        } else {
                            self.remove(item.id).await?;
                        }
                    }
                    SyncChange::Gone(id) => {
                        if id == folder {
                            return self.list_again(user, folder).await;
                        }
                        // A folder takes what's in it along, without
                        // naming each item.
                        if !self.remove(id).await? {
                            prune = true;
                        }
                    }
                }
            }
            cursor = page.cursor;
            if !page.more {
                break;
            }
        }
        if prune {
            self.prune(user, folder).await?;
        }
        self.save_cursor(user, Some(cursor)).await
    }

    /// Lists the whole folder again, such as the first time, or when the
    /// changes since the cursor are gone.
    async fn list_again(&self, user: UserId, folder: ItemId) -> Result<(), Error> {
        let start = self.storage.sync_start(user).await?;
        let seen = self.add_folder(user, folder, folder).await?;
        let indexed = self.ids(user).await?;
        for id in indexed.difference(&seen) {
            self.remove(*id).await?;
        }
        self.save_cursor(user, Some(start.cursor)).await
    }

    /// Indexes every note inside `within`, and returns their IDs.
    async fn add_folder(
        &self,
        user: UserId,
        folder: ItemId,
        within: ItemId,
    ) -> Result<HashSet<ItemId>, Error> {
        let mut seen = HashSet::new();
        let mut after = None;
        loop {
            let page = self.storage.sync_list(user, within, after).await?;
            for entry in page.items {
                if self.is_note(user, folder, &entry.item).await? {
                    seen.insert(entry.item.id);
                    self.add(user, &entry.item).await?;
                }
            }
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        Ok(seen)
    }

    /// Takes out notes that aren't in the folder any more.
    async fn prune(&self, user: UserId, folder: ItemId) -> Result<(), Error> {
        for id in self.ids(user).await? {
            if self.storage.path_within(folder, id).await?.is_none() {
                self.remove(id).await?;
            }
        }
        Ok(())
    }

    async fn ids(&self, user: UserId) -> Result<HashSet<ItemId>, Error> {
        self.db
            .read(move |conn| {
                let mut statement =
                    conn.prepare("SELECT item_id FROM notes_index WHERE user_id = ?1")?;
                let ids = statement
                    .query_map([user.as_uuid()], |row| Ok(ItemId::from_uuid(row.get(0)?)))?
                    .collect::<Result<_, _>>()?;
                Ok(ids)
            })
            .await
    }

    async fn save_cursor(&self, user: UserId, cursor: Option<String>) -> Result<(), Error> {
        self.db
            .write(move |tx| {
                tx.execute(
                    "UPDATE notes_folders SET cursor = ?2 WHERE user_id = ?1",
                    (user.as_uuid(), cursor),
                )?;
                Ok(())
            })
            .await
    }

    /// Adds a note to the index, or brings its row up to date. The file is
    /// read only if its contents changed.
    pub(crate) async fn add(&self, user: UserId, item: &Item) -> Result<Indexed, Error> {
        let ItemKind::File { blob, .. } = &item.kind else {
            return Err(Error::NotAFile);
        };
        let hash = blob.hash.to_hex();
        let id = item.id;
        let known = self.get(id).await?;
        let (details, body) = match known {
            Some(known) if known.hash == hash => (known.details, known.body),
            _ => {
                let text = self.read(id).await?;
                let file = NoteFile::parse(&text);
                (file.details(), shorten(file.body(), BODY_CHARS))
            }
        };
        let indexed = Indexed {
            id,
            hash,
            name: item.name.clone(),
            folder: item.parent,
            details,
            body,
            created_at: item.created_at.clone(),
            modified_at: item.modified_at.clone(),
        };
        let row = indexed.clone();
        self.db
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO notes_index (item_id, user_id, hash, name, color, pinned,
                         archived, tags, body, created_at, modified_at, folder_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                     ON CONFLICT (item_id) DO UPDATE SET
                         user_id = excluded.user_id, hash = excluded.hash,
                         name = excluded.name, color = excluded.color,
                         pinned = excluded.pinned, archived = excluded.archived,
                         tags = excluded.tags, body = excluded.body,
                         created_at = excluded.created_at, modified_at = excluded.modified_at,
                         folder_id = excluded.folder_id",
                    rusqlite::params![
                        row.id.as_uuid(),
                        user.as_uuid(),
                        row.hash,
                        row.name,
                        row.details.color,
                        row.details.pinned,
                        row.details.archived,
                        serde_json::to_string(&row.details.tags).expect("a list of strings"),
                        row.body,
                        row.created_at,
                        row.modified_at,
                        row.folder.map(|folder| folder.as_uuid()),
                    ],
                )?;
                Ok::<_, Error>(())
            })
            .await?;
        Ok(indexed)
    }

    /// Takes a note out of the index. Returns whether it was there.
    pub(crate) async fn remove(&self, id: ItemId) -> Result<bool, Error> {
        self.db
            .write(move |tx| {
                Ok(tx.execute("DELETE FROM notes_index WHERE item_id = ?1", [id.as_uuid()])? > 0)
            })
            .await
    }

    pub(crate) async fn get(&self, id: ItemId) -> Result<Option<Indexed>, Error> {
        self.db
            .read(move |conn| {
                Ok(conn
                    .query_row(
                        &format!("{SELECT} WHERE item_id = ?1"),
                        [id.as_uuid()],
                        indexed_from_row,
                    )
                    .optional()?)
            })
            .await
    }

    /// Every note of `user`'s, newest first.
    pub(crate) async fn all(&self, user: UserId) -> Result<Vec<Indexed>, Error> {
        self.db
            .read(move |conn| {
                let mut statement = conn.prepare(&format!(
                    "{SELECT} WHERE user_id = ?1 ORDER BY modified_at DESC, item_id"
                ))?;
                let notes = statement
                    .query_map([user.as_uuid()], indexed_from_row)?
                    .collect::<Result<_, _>>()?;
                Ok(notes)
            })
            .await
    }

    /// A note's file, up to [`READ_LIMIT`].
    pub(crate) async fn read(&self, id: ItemId) -> Result<String, Error> {
        let file = self.storage.open_file(id).await?;
        let mut bytes = Vec::new();
        file.take(READ_LIMIT)
            .read_to_end(&mut bytes)
            .await
            .map_err(Error::Io)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// The labels someone made that no note has yet, and the ones they
    /// gave notes.
    pub(crate) async fn labels(&self, user: UserId) -> Result<Vec<String>, Error> {
        let mut labels: Vec<String> = self
            .db
            .read(move |conn| {
                let mut statement =
                    conn.prepare("SELECT label FROM notes_labels WHERE user_id = ?1")?;
                let labels = statement
                    .query_map([user.as_uuid()], |row| row.get(0))?
                    .collect::<Result<_, _>>()?;
                Ok::<_, Error>(labels)
            })
            .await?;
        for indexed in self.all(user).await? {
            for tag in indexed.details.tags {
                if !labels.iter().any(|label| label.eq_ignore_ascii_case(&tag)) {
                    labels.push(tag);
                }
            }
        }
        labels.sort_by_key(|label| label.to_lowercase());
        Ok(labels)
    }

    /// Keeps a label that no note has yet, or forgets it with `keep` false.
    pub(crate) async fn keep_label(
        &self,
        user: UserId,
        label: String,
        keep: bool,
    ) -> Result<(), Error> {
        self.db
            .write(move |tx| {
                if keep {
                    tx.execute(
                        "INSERT OR IGNORE INTO notes_labels (user_id, label) VALUES (?1, ?2)",
                        (user.as_uuid(), label),
                    )?;
                } else {
                    tx.execute(
                        "DELETE FROM notes_labels WHERE user_id = ?1 AND label = ?2 COLLATE NOCASE",
                        (user.as_uuid(), label),
                    )?;
                }
                Ok(())
            })
            .await
    }

    /// The total size of everyone's notes, for the Modules page.
    pub(crate) async fn size(&self) -> Result<u64, Error> {
        let folders: Vec<ItemId> = self
            .db
            .read(|conn| {
                let mut statement = conn.prepare("SELECT folder_id FROM notes_folders")?;
                let folders = statement
                    .query_map([], |row| Ok(ItemId::from_uuid(row.get(0)?)))?
                    .collect::<Result<_, _>>()?;
                Ok::<_, Error>(folders)
            })
            .await?;
        Ok(self.storage.folder_sizes(folders).await?.values().sum())
    }
}

const SELECT: &str = "SELECT item_id, hash, name, color, pinned, archived, tags, body,
    created_at, modified_at, folder_id FROM notes_index";

fn indexed_from_row(row: &Row<'_>) -> rusqlite::Result<Indexed> {
    let tags: String = row.get(6)?;
    Ok(Indexed {
        id: ItemId::from_uuid(row.get(0)?),
        hash: row.get(1)?,
        name: row.get(2)?,
        details: note::Details {
            color: row.get(3)?,
            pinned: row.get(4)?,
            archived: row.get(5)?,
            tags: serde_json::from_str(&tags).unwrap_or_default(),
        },
        body: row.get(7)?,
        created_at: row.get(8)?,
        modified_at: row.get(9)?,
        folder: row.get::<_, Option<_>>(10)?.map(ItemId::from_uuid),
    })
}

/// The first `chars` characters of `text`.
pub(crate) fn shorten(text: &str, chars: usize) -> String {
    match text.char_indices().nth(chars) {
        Some((at, _)) => text[..at].to_owned(),
        None => text.to_owned(),
    }
}
