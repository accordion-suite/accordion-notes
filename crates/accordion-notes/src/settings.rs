//! Each person's Notes settings: where a new note's caret starts, where a
//! list's new items go, and whether checked items move to the bottom, as
//! in Google Keep. They're kept on the server, so they follow the person
//! to every device. Someone who never changes them has no row, and gets
//! the defaults.
//!
//! They change only how the web app writes and shows notes. A note's file
//! keeps its items in the order they were written.

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

pub(crate) const MIGRATION_8: Migration = Migration {
    version: 8,
    name: "Add where a new note starts",
    sql: "
    ALTER TABLE notes_settings ADD COLUMN start_in TEXT NOT NULL DEFAULT 'text'
        CHECK (start_in IN ('text', 'title'));
    ",
};

/// Where the caret starts in a new note.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum StartIn {
    /// The note itself, under "Take a note…", as in Keep.
    #[default]
    Text,
    Title,
}

impl StartIn {
    fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Title => "title",
        }
    }

    fn from_str(text: &str) -> Self {
        match text {
            "title" => Self::Title,
            _ => Self::Text,
        }
    }
}

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
    /// Missing from a web app older than the setting, which keeps it as it
    /// was.
    #[serde(default)]
    pub start_in: Option<StartIn>,
    pub new_items: NewItems,
    /// Whether checked items show below the others, under "checked items".
    /// Otherwise they stay where they are.
    pub checked_at_bottom: bool,
}

impl Default for NotesSettings {
    fn default() -> Self {
        Self {
            start_in: Some(StartIn::Text),
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
                        "SELECT start_in, new_items, checked_at_bottom FROM notes_settings
                         WHERE user_id = ?1",
                        [user.as_uuid()],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, bool>(2)?,
                            ))
                        },
                    )
                    .optional()?;
                Ok(match found {
                    Some((start_in, new_items, checked_at_bottom)) => NotesSettings {
                        start_in: Some(StartIn::from_str(&start_in)),
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
                    "INSERT INTO notes_settings (user_id, start_in, new_items, checked_at_bottom)
                     VALUES (?1, coalesce(?4, 'text'), ?2, ?3)
                     ON CONFLICT (user_id) DO UPDATE SET
                         start_in = coalesce(?4, start_in),
                         new_items = excluded.new_items,
                         checked_at_bottom = excluded.checked_at_bottom",
                    (
                        user.as_uuid(),
                        settings.new_items.as_str(),
                        settings.checked_at_bottom,
                        settings.start_in.map(StartIn::as_str),
                    ),
                )?;
                Ok(())
            })
            .await
    }
}
