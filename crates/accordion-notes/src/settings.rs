//! Each person's Notes settings: where a list's new items go, and whether
//! checked items move to the bottom, as in Google Keep. They're kept on the
//! server, so they follow the person to every device. Someone who never
//! changes them has no row, and gets the defaults.
//!
//! They change only how lists show and where the web app puts a new item.
//! A note's file keeps its items in the order they were written.

use accordion_core::db::rusqlite::OptionalExtension;
use accordion_core::db::{Database, Migration};
use accordion_core::storage::Error;
use accordion_types::UserId;
use serde::{Deserialize, Serialize};

pub(crate) const MIGRATION: Migration = Migration {
    version: 3,
    name: "Create settings",
    sql: "
    CREATE TABLE notes_settings (
        user_id BLOB PRIMARY KEY NOT NULL CHECK (length(user_id) = 16),
        new_items TEXT NOT NULL CHECK (new_items IN ('bottom', 'top')),
        checked_at_bottom INTEGER NOT NULL CHECK (checked_at_bottom IN (0, 1))
    ) STRICT, WITHOUT ROWID;
    ",
};

/// Where a list's new items go.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum NewItems {
    #[default]
    Bottom,
    Top,
}

impl NewItems {
    fn as_str(self) -> &'static str {
        match self {
            Self::Bottom => "bottom",
            Self::Top => "top",
        }
    }

    fn from_str(text: &str) -> Self {
        match text {
            "top" => Self::Top,
            _ => Self::Bottom,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NotesSettings {
    pub new_items: NewItems,
    /// Whether checked items show below the others, under "checked items".
    /// Otherwise they stay where they are.
    pub checked_at_bottom: bool,
}

impl Default for NotesSettings {
    fn default() -> Self {
        Self {
            new_items: NewItems::Bottom,
            checked_at_bottom: true,
        }
    }
}

#[derive(Clone)]
pub(crate) struct Settings {
    db: Database,
}

impl Settings {
    pub(crate) fn new(db: Database) -> Self {
        Self { db }
    }

    pub(crate) async fn get(&self, user: UserId) -> Result<NotesSettings, Error> {
        self.db
            .read(move |conn| {
                let found = conn
                    .query_row(
                        "SELECT new_items, checked_at_bottom FROM notes_settings
                         WHERE user_id = ?1",
                        [user.as_uuid()],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)),
                    )
                    .optional()?;
                Ok(match found {
                    Some((new_items, checked_at_bottom)) => NotesSettings {
                        new_items: NewItems::from_str(&new_items),
                        checked_at_bottom,
                    },
                    None => NotesSettings::default(),
                })
            })
            .await
    }

    pub(crate) async fn set(&self, user: UserId, settings: NotesSettings) -> Result<(), Error> {
        self.db
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO notes_settings (user_id, new_items, checked_at_bottom)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT (user_id) DO UPDATE SET
                         new_items = excluded.new_items,
                         checked_at_bottom = excluded.checked_at_bottom",
                    (
                        user.as_uuid(),
                        settings.new_items.as_str(),
                        settings.checked_at_bottom,
                    ),
                )?;
                Ok(())
            })
            .await
    }
}
