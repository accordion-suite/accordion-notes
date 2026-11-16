//! Reminders on notes (decision 31). Each reminder is one person's own,
//! the owner's too, so a note shared with someone never shows them anyone
//! else's. None of it is in the note's file: a reminder is someone's alarm,
//! not part of the note, and Obsidian couldn't ring it anyway.
//!
//! A reminder keeps the clock time it was set for and the time zone it was
//! set in, such as 09:00 in `Europe/Amsterdam`, so one that repeats every
//! Monday rings at 09:00 there before and after the clocks change. On a day
//! the clocks skip that time, it rings once they've moved on; when they go
//! back over it, it rings the first time it comes.
//!
//! Notes checks for reminders that are due at the start of every minute,
//! and right after starting, so a reminder due while the server was off
//! rings once it's back. A repeating one rings once for the times it
//! missed, then waits for its next.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use accordion_core::db::rusqlite::{OptionalExtension, Row};
use accordion_core::db::{Database, Migration};
use accordion_core::events::{Action, Actor, Events, NewEvent};
use accordion_core::live::{Hint, Live};
use accordion_core::storage::{Error, Storage};
use accordion_module::Stopping;
use accordion_types::{ItemId, UserId};
use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan};
use serde::Serialize;
use serde_json::json;

/// The live update telling someone's open tabs that their reminders
/// changed: one rang, or they set, snoozed, or finished one elsewhere.
pub(crate) const REMINDERS: &str = "notes.reminders";

pub(crate) const MIGRATION: Migration = Migration {
    version: 5,
    name: "Keep each person's reminders",
    sql: "
    CREATE TABLE notes_reminders (
        user_id BLOB NOT NULL CHECK (length(user_id) = 16),
        item_id BLOB NOT NULL CHECK (length(item_id) = 16),
        -- The clock time it's set for, such as 2026-11-20T09:00, in zone.
        -- A repeating one repeats from here.
        local TEXT NOT NULL,
        -- A time zone's name, such as Europe/Amsterdam.
        zone TEXT NOT NULL,
        repeat TEXT CHECK (repeat IN ('daily', 'weekly', 'monthly', 'yearly')),
        -- When it next rings, in UTC, as 2026-11-20T08:00:00Z. A snooze
        -- moves it.
        due_at TEXT NOT NULL,
        -- When it last rang, until it's done. One that doesn't repeat rings
        -- only once, and waits here to be snoozed or done.
        rang_at TEXT,
        PRIMARY KEY (user_id, item_id)
    ) STRICT, WITHOUT ROWID;
    CREATE INDEX notes_reminders_due ON notes_reminders (due_at);
    ",
};

/// How often a reminder comes back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Repeat {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

impl Repeat {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
            Self::Yearly => "yearly",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        [Self::Daily, Self::Weekly, Self::Monthly, Self::Yearly]
            .into_iter()
            .find(|repeat| repeat.as_str() == text)
    }

    /// The `n`th time after `first`, counting from it each time, so a
    /// monthly reminder on the 31st comes back on the 31st after a shorter
    /// month.
    fn nth(self, first: DateTime, n: i64) -> Option<DateTime> {
        let span = match self {
            Self::Daily => n.days(),
            Self::Weekly => (n * 7).days(),
            Self::Monthly => n.months(),
            Self::Yearly => n.years(),
        };
        first.checked_add(span).ok()
    }

    /// The most seconds between two of its times, counting an hour for the
    /// clocks changing, to skip ahead quickly.
    fn longest(self) -> i64 {
        const DAY: i64 = 86_400;
        match self {
            Self::Daily => DAY + 3_600,
            Self::Weekly => 7 * DAY + 3_600,
            Self::Monthly => 31 * DAY + 3_600,
            Self::Yearly => 366 * DAY + 3_600,
        }
    }
}

/// A clock time and a time zone, as someone set them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct When {
    pub local: DateTime,
    pub zone: String,
    pub repeat: Option<Repeat>,
}

/// Why a reminder can't be set as asked.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Invalid {
    Time,
    Zone,
    Repeat,
}

impl When {
    /// Reads a reminder as the web app sends it: `local` such as
    /// `2026-11-20T09:00`, and `zone` such as `Europe/Amsterdam`.
    pub(crate) fn parse(local: &str, zone: &str, repeat: Option<&str>) -> Result<Self, Invalid> {
        let local: DateTime = local.parse().map_err(|_| Invalid::Time)?;
        // Stored times keep four-digit years, so they sort as text.
        if !(2000..=9000).contains(&local.year()) {
            return Err(Invalid::Time);
        }
        TimeZone::get(zone).map_err(|_| Invalid::Zone)?;
        let repeat = match repeat {
            Some(text) => Some(Repeat::parse(text).ok_or(Invalid::Repeat)?),
            None => None,
        };
        Ok(Self {
            // To the minute, as people set them.
            local: local
                .with()
                .second(0)
                .subsec_nanosecond(0)
                .build()
                .map_err(|_| Invalid::Time)?,
            zone: zone.to_owned(),
            repeat,
        })
    }

    fn local_text(&self) -> String {
        self.local.strftime("%Y-%m-%dT%H:%M").to_string()
    }

    /// The moment the clock in its time zone reads `local`. A time the
    /// clocks skip comes as soon as they've moved on, and a time they pass
    /// twice comes the first time.
    fn moment(&self, local: DateTime) -> Option<Timestamp> {
        let zone = TimeZone::get(&self.zone).ok()?;
        let zoned = zone.to_ambiguous_zoned(local).compatible().ok()?;
        Some(zoned.timestamp())
    }

    /// When it next rings after `after`: its time if it doesn't repeat,
    /// whether that's passed or not, or the first time it comes after.
    pub(crate) fn next_after(&self, after: Timestamp) -> Option<Timestamp> {
        let first = self.moment(self.local)?;
        let Some(repeat) = self.repeat else {
            return Some(first);
        };
        // Each jump skips as many times as surely fit before `after`, since
        // no two are further apart than the longest gap, so it never passes
        // the one it's looking for, and gets there in a few jumps.
        let mut n = 0;
        for _ in 0..1_000 {
            let moment = self.moment(repeat.nth(self.local, n)?)?;
            if moment > after {
                return Some(moment);
            }
            let behind = after.as_second() - moment.as_second();
            n += (behind / repeat.longest()).max(1);
        }
        None
    }
}

/// Someone's reminder on a note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Reminder {
    pub user: UserId,
    pub item: ItemId,
    pub when: When,
    pub due_at: Timestamp,
    pub rang_at: Option<Timestamp>,
}

/// A reminder, as the web app shows it.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ReminderView {
    /// The clock time it's set for, such as `2026-11-20T09:00`.
    local: String,
    zone: String,
    /// `daily`, `weekly`, `monthly`, `yearly`, or null.
    repeat: Option<&'static str>,
    /// When it next rings, after any snooze. For one that doesn't repeat
    /// and has rung, when it rang.
    due_at: String,
    /// Whether it has rung and isn't done. One that doesn't repeat waits
    /// to be snoozed or done; one that repeats is already waiting for its
    /// next time.
    rang: bool,
}

impl From<&Reminder> for ReminderView {
    fn from(reminder: &Reminder) -> Self {
        Self {
            local: reminder.when.local_text(),
            zone: reminder.when.zone.clone(),
            repeat: reminder.when.repeat.map(Repeat::as_str),
            due_at: stamp(reminder.due_at),
            rang: reminder.rang_at.is_some(),
        }
    }
}

/// A moment as the table keeps it, to the second, so text sorts in time
/// order.
fn stamp(moment: Timestamp) -> String {
    Timestamp::from_second(moment.as_second())
        .unwrap_or(moment)
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

const COLUMNS: &str = "user_id, item_id, local, zone, repeat, due_at, rang_at";

fn reminder_from(row: &Row<'_>) -> accordion_core::db::rusqlite::Result<Option<Reminder>> {
    let local: String = row.get(2)?;
    let zone: String = row.get(3)?;
    let repeat: Option<String> = row.get(4)?;
    let due_at: String = row.get(5)?;
    let rang_at: Option<String> = row.get(6)?;
    // A row that can't be read, such as a time zone that's gone from the
    // system's list, is left out rather than breaking the whole list.
    let Ok(when) = When::parse(&local, &zone, repeat.as_deref()) else {
        return Ok(None);
    };
    let Ok(due_at) = due_at.parse() else {
        return Ok(None);
    };
    Ok(Some(Reminder {
        user: UserId::from_uuid(row.get(0)?),
        item: ItemId::from_uuid(row.get(1)?),
        when,
        due_at,
        rang_at: rang_at.and_then(|text| text.parse().ok()),
    }))
}

#[derive(Clone)]
pub(crate) struct Reminders {
    db: Database,
}

impl Reminders {
    pub(crate) fn new(db: Database) -> Self {
        Self { db }
    }

    /// `user`'s reminder on the note `item`, if they set one.
    pub(crate) async fn get(&self, user: UserId, item: ItemId) -> Result<Option<Reminder>, Error> {
        self.db
            .read(move |conn| {
                Ok(conn
                    .query_row(
                        &format!(
                            "SELECT {COLUMNS} FROM notes_reminders
                             WHERE user_id = ?1 AND item_id = ?2"
                        ),
                        (user.as_uuid(), item.as_uuid()),
                        reminder_from,
                    )
                    .optional()?
                    .flatten())
            })
            .await
    }

    /// Each of `user`'s reminders, by note.
    pub(crate) async fn of(&self, user: UserId) -> Result<HashMap<ItemId, Reminder>, Error> {
        self.db
            .read(move |conn| {
                let mut statement = conn.prepare(&format!(
                    "SELECT {COLUMNS} FROM notes_reminders WHERE user_id = ?1"
                ))?;
                let mut found = HashMap::new();
                for reminder in statement.query_map([user.as_uuid()], reminder_from)? {
                    if let Some(reminder) = reminder? {
                        found.insert(reminder.item, reminder);
                    }
                }
                Ok(found)
            })
            .await
    }

    /// Sets `user`'s reminder on `item`, in place of any they had. One that
    /// repeats starts at its first time after `now`.
    pub(crate) async fn set(
        &self,
        user: UserId,
        item: ItemId,
        when: When,
        now: Timestamp,
    ) -> Result<Option<Reminder>, Error> {
        let Some(due_at) = when.next_after(now) else {
            return Ok(None);
        };
        let reminder = Reminder {
            user,
            item,
            when,
            due_at,
            rang_at: None,
        };
        let row = reminder.clone();
        self.db
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO notes_reminders (user_id, item_id, local, zone, repeat, due_at, rang_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)
                     ON CONFLICT (user_id, item_id) DO UPDATE SET
                         local = excluded.local, zone = excluded.zone,
                         repeat = excluded.repeat, due_at = excluded.due_at,
                         rang_at = NULL",
                    (
                        row.user.as_uuid(),
                        row.item.as_uuid(),
                        row.when.local_text(),
                        row.when.zone.clone(),
                        row.when.repeat.map(Repeat::as_str),
                        stamp(row.due_at),
                    ),
                )?;
                Ok::<_, Error>(())
            })
            .await?;
        Ok(Some(reminder))
    }

    /// Takes `user`'s reminder off `item`. Returns whether there was one.
    pub(crate) async fn remove(&self, user: UserId, item: ItemId) -> Result<bool, Error> {
        self.db
            .write(move |tx| {
                let removed = tx.execute(
                    "DELETE FROM notes_reminders WHERE user_id = ?1 AND item_id = ?2",
                    (user.as_uuid(), item.as_uuid()),
                )?;
                Ok(removed > 0)
            })
            .await
    }

    /// Takes everyone's reminders off a note deleted for good.
    pub(crate) async fn forget(&self, item: ItemId) -> Result<(), Error> {
        self.db
            .write(move |tx| {
                tx.execute(
                    "DELETE FROM notes_reminders WHERE item_id = ?1",
                    [item.as_uuid()],
                )?;
                Ok(())
            })
            .await
    }

    /// Rings `user`'s reminder on `item` again at `until`. A repeating one
    /// carries on from its own times after that.
    pub(crate) async fn snooze(
        &self,
        user: UserId,
        item: ItemId,
        until: Timestamp,
    ) -> Result<Option<Reminder>, Error> {
        let due = stamp(until);
        let changed = self
            .db
            .write(move |tx| {
                Ok::<_, Error>(tx.execute(
                    "UPDATE notes_reminders SET due_at = ?3, rang_at = NULL
                     WHERE user_id = ?1 AND item_id = ?2",
                    (user.as_uuid(), item.as_uuid(), due),
                )?)
            })
            .await?;
        if changed == 0 {
            return Ok(None);
        }
        self.get(user, item).await
    }

    /// Finishes `user`'s reminder on `item`: one that doesn't repeat goes,
    /// and one that does waits for its next time without showing it rang.
    /// Returns what's left.
    pub(crate) async fn done(&self, user: UserId, item: ItemId) -> Result<Option<Reminder>, Error> {
        let Some(reminder) = self.get(user, item).await? else {
            return Ok(None);
        };
        if reminder.when.repeat.is_none() {
            self.remove(user, item).await?;
            return Ok(None);
        }
        self.db
            .write(move |tx| {
                tx.execute(
                    "UPDATE notes_reminders SET rang_at = NULL
                     WHERE user_id = ?1 AND item_id = ?2",
                    (user.as_uuid(), item.as_uuid()),
                )?;
                Ok::<_, Error>(())
            })
            .await?;
        Ok(Some(Reminder {
            rang_at: None,
            ..reminder
        }))
    }

    /// Reminders due at `now` that haven't rung for it.
    async fn due(&self, now: Timestamp) -> Result<Vec<Reminder>, Error> {
        let now = stamp(now);
        self.db
            .read(move |conn| {
                let mut statement = conn.prepare(&format!(
                    "SELECT {COLUMNS} FROM notes_reminders
                     WHERE due_at <= ?1 AND (repeat IS NOT NULL OR rang_at IS NULL)
                     ORDER BY due_at"
                ))?;
                let mut found = Vec::new();
                for reminder in statement.query_map([now], reminder_from)? {
                    found.extend(reminder?);
                }
                Ok(found)
            })
            .await
    }

    /// Notes that `reminder` rang at `now`. One that repeats moves to its
    /// next time. Nothing changes if someone changed it since it was read,
    /// such as by snoozing it.
    async fn rang(&self, reminder: &Reminder, now: Timestamp) -> Result<(), Error> {
        let next = match reminder.when.repeat {
            Some(_) => reminder
                .when
                .next_after(now)
                .map_or_else(|| stamp(reminder.due_at), stamp),
            None => stamp(reminder.due_at),
        };
        let (user, item, was) = (reminder.user, reminder.item, stamp(reminder.due_at));
        let rang = stamp(now);
        self.db
            .write(move |tx| {
                tx.execute(
                    "UPDATE notes_reminders SET due_at = ?4, rang_at = ?5
                     WHERE user_id = ?1 AND item_id = ?2 AND due_at = ?3",
                    (user.as_uuid(), item.as_uuid(), was, next, rang),
                )?;
                Ok(())
            })
            .await
    }
}

/// What ringing reminders needs.
#[derive(Clone)]
pub(crate) struct Ringer {
    pub reminders: Reminders,
    pub storage: Storage,
    pub events: Events,
    pub live: Live,
}

impl Ringer {
    /// Rings every reminder due at `now`, each once, through the bell.
    /// A reminder on a note its person can't open any more goes, and one on
    /// a note in the trash moves on without ringing.
    pub(crate) async fn ring_due(&self, now: Timestamp) -> Result<usize, Error> {
        let mut rang = 0;
        for reminder in self.reminders.due(now).await? {
            let item = self.storage.item(reminder.item).await?;
            let reachable = match &item {
                Some(_) => self
                    .storage
                    .access(reminder.user, reminder.item)
                    .await?
                    .is_some(),
                None => false,
            };
            let Some(item) = item.filter(|_| reachable) else {
                self.reminders.remove(reminder.user, reminder.item).await?;
                continue;
            };
            if self.storage.outside_trash(item.id).await? {
                let event = NewEvent::about(Action::ReminderDue, item.id, item.owner)
                    .other(reminder.user)
                    .details(json!({
                        "local": reminder.when.local_text(),
                        "repeat": reminder.when.repeat.map(Repeat::as_str),
                    }));
                // Rung before it's marked, so a crash in between rings it
                // twice rather than not at all.
                if let Err(err) = self.events.record(Actor::SERVER, event).await {
                    tracing::warn!(note = %item.id, %err, "couldn't ring a reminder");
                    continue;
                }
                rang += 1;
            }
            self.reminders.rang(&reminder, now).await?;
            self.live.send(Hint::person(REMINDERS, reminder.user));
        }
        Ok(rang)
    }

    /// Rings reminders at the start of each minute until Notes stops.
    pub(crate) async fn run(self, stopping: Stopping) {
        let stop = stopping.wait();
        tokio::pin!(stop);
        loop {
            if let Err(err) = self.ring_due(Timestamp::now()).await {
                tracing::warn!(%err, "couldn't check for reminders that are due");
            }
            tokio::select! {
                () = &mut stop => return,
                () = tokio::time::sleep(until_next_minute()) => {}
            }
        }
    }
}

/// How long until the clock next reaches a whole minute, so a reminder for
/// 09:00 rings at 09:00, not up to a minute late.
fn until_next_minute() -> Duration {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let into = since.as_millis() % 60_000;
    Duration::from_millis((60_000 - into) as u64 + 50)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn when(local: &str, zone: &str, repeat: Option<&str>) -> When {
        When::parse(local, zone, repeat).unwrap()
    }

    #[test]
    fn a_weekly_reminder_keeps_its_clock_time_when_the_clocks_change() {
        // Mondays at 09:00 in Amsterdam: 08:00 UTC in winter, 07:00 in
        // summer. The clocks went forward on March 29, 2026.
        let mondays = when("2026-03-23T09:00", "Europe/Amsterdam", Some("weekly"));
        assert_eq!(
            mondays.next_after(at("2026-03-23T08:00:00Z")),
            Some(at("2026-03-30T07:00:00Z"))
        );
        // And back on October 25, 2026.
        assert_eq!(
            mondays.next_after(at("2026-10-20T00:00:00Z")),
            Some(at("2026-10-26T08:00:00Z"))
        );
    }

    #[test]
    fn a_time_the_clocks_skip_or_pass_twice_rings_once() {
        // 02:30 doesn't happen in Amsterdam on March 29, 2026, so it rings
        // at 03:30, an hour on.
        let skipped = when("2026-03-29T02:30", "Europe/Amsterdam", None);
        assert_eq!(
            skipped.next_after(at("2026-03-01T00:00:00Z")),
            Some(at("2026-03-29T01:30:00Z"))
        );
        // 02:30 comes twice on October 25, 2026, and it rings the first.
        let twice = when("2026-10-25T02:30", "Europe/Amsterdam", None);
        assert_eq!(
            twice.next_after(at("2026-10-01T00:00:00Z")),
            Some(at("2026-10-25T00:30:00Z"))
        );
    }

    #[test]
    fn a_monthly_reminder_on_the_31st_comes_back_on_the_31st() {
        let end = when("2026-01-31T18:00", "UTC", Some("monthly"));
        assert_eq!(
            end.next_after(at("2026-01-31T18:00:00Z")),
            Some(at("2026-02-28T18:00:00Z"))
        );
        assert_eq!(
            end.next_after(at("2026-02-28T18:00:00Z")),
            Some(at("2026-03-31T18:00:00Z"))
        );
    }

    #[test]
    fn a_daily_reminder_from_long_ago_finds_its_next_time_quickly() {
        let daily = when("2001-05-05T07:15", "America/New_York", Some("daily"));
        assert_eq!(
            daily.next_after(at("2026-11-16T20:00:00Z")),
            Some(at("2026-11-17T12:15:00Z"))
        );
    }

    #[test]
    fn a_reminder_that_doesnt_repeat_keeps_its_time_once_passed() {
        let once = when("2026-11-20T09:00", "Asia/Tokyo", None);
        assert_eq!(
            once.next_after(at("2026-12-01T00:00:00Z")),
            Some(at("2026-11-20T00:00:00Z"))
        );
    }

    #[test]
    fn only_real_times_zones_and_repeats_are_kept() {
        assert_eq!(
            When::parse("2026-02-30T09:00", "UTC", None),
            Err(Invalid::Time)
        );
        assert_eq!(
            When::parse("2026-02-03T09:00", "Mars/Olympus", None),
            Err(Invalid::Zone)
        );
        assert_eq!(
            When::parse("2026-02-03T09:00", "UTC", Some("hourly")),
            Err(Invalid::Repeat)
        );
        assert_eq!(
            When::parse("2026-02-03T09:00:41", "UTC", None)
                .unwrap()
                .local_text(),
            "2026-02-03T09:00"
        );
    }

    mod ringing {
        use accordion_core::events::Action;
        use accordion_core::identity::{Identity, NewUser};
        use accordion_core::storage::ShareLevel;
        use accordion_types::Role;

        use super::*;

        struct Home {
            _dir: tempfile::TempDir,
            ringer: Ringer,
            storage: Storage,
            alex: UserId,
            sam: UserId,
            note: ItemId,
        }

        async fn home() -> Home {
            let dir = tempfile::tempdir().unwrap();
            let db = Database::open(&dir.path().join("accordion.db")).unwrap();
            let identity = Identity::new(db.clone()).await.unwrap();
            let storage = Storage::open(db.clone(), identity.clone(), &dir.path().join("storage"))
                .await
                .unwrap();
            db.migrate(crate::OWNER, crate::MIGRATIONS).await.unwrap();
            let user = |name: &str| NewUser {
                username: name.into(),
                display_name: name.into(),
                role: Role::User,
                password: "correct horse".into(),
            };
            let alex = identity.create_user(user("alex")).await.unwrap().id;
            let sam = identity.create_user(user("sam")).await.unwrap().id;
            let top = storage.top_folder(alex).await.unwrap();
            let note = storage
                .write_file(top, "Groceries.md", &b"Milk"[..])
                .await
                .unwrap()
                .id;
            let ringer = Ringer {
                reminders: Reminders::new(db.clone()),
                storage: storage.clone(),
                events: Events::open(db.clone()).await.unwrap(),
                live: Live::new(db, &identity),
            };
            Home {
                _dir: dir,
                ringer,
                storage,
                alex,
                sam,
                note,
            }
        }

        impl Home {
            async fn rung(&self, user: UserId) -> usize {
                self.ringer
                    .events
                    .notifications(user, 50)
                    .await
                    .unwrap()
                    .iter()
                    .filter(|notice| notice.event.action == Action::ReminderDue)
                    .count()
            }

            /// The same tables, as after a restart.
            fn restarted(&self) -> Ringer {
                self.ringer.clone()
            }
        }

        #[tokio::test]
        async fn a_reminder_rings_once_at_its_time_and_not_again_after_a_restart() {
            let h = home().await;
            let reminders = &h.ringer.reminders;
            let set = at("2026-11-19T12:00:00Z");
            reminders
                .set(h.alex, h.note, when("2026-11-20T09:00", "UTC", None), set)
                .await
                .unwrap();

            assert_eq!(
                h.ringer.ring_due(at("2026-11-20T08:59:00Z")).await.unwrap(),
                0
            );
            assert_eq!(
                h.ringer.ring_due(at("2026-11-20T09:00:00Z")).await.unwrap(),
                1
            );
            assert_eq!(
                h.ringer.ring_due(at("2026-11-20T09:01:00Z")).await.unwrap(),
                0
            );
            let later = h.restarted();
            assert_eq!(later.ring_due(at("2026-11-20T10:00:00Z")).await.unwrap(), 0);
            assert_eq!(h.rung(h.alex).await, 1);
            let rang = reminders.get(h.alex, h.note).await.unwrap().unwrap();
            assert_eq!(rang.rang_at, Some(at("2026-11-20T09:00:00Z")));

            // Snoozed, it rings once more, and done, it's gone.
            reminders
                .snooze(h.alex, h.note, at("2026-11-20T11:00:00Z"))
                .await
                .unwrap();
            assert_eq!(later.ring_due(at("2026-11-20T10:59:00Z")).await.unwrap(), 0);
            assert_eq!(later.ring_due(at("2026-11-20T11:00:00Z")).await.unwrap(), 1);
            assert_eq!(later.ring_due(at("2026-11-20T11:05:00Z")).await.unwrap(), 0);
            assert_eq!(reminders.done(h.alex, h.note).await.unwrap(), None);
            assert_eq!(reminders.get(h.alex, h.note).await.unwrap(), None);
            assert_eq!(h.rung(h.alex).await, 2);
        }

        #[tokio::test]
        async fn a_repeating_reminder_missed_while_off_rings_once_then_waits() {
            let h = home().await;
            let reminders = &h.ringer.reminders;
            reminders
                .set(
                    h.alex,
                    h.note,
                    when("2026-11-20T09:00", "UTC", Some("daily")),
                    at("2026-11-19T12:00:00Z"),
                )
                .await
                .unwrap();
            assert_eq!(
                h.ringer.ring_due(at("2026-11-20T09:00:00Z")).await.unwrap(),
                1
            );
            let next = reminders.get(h.alex, h.note).await.unwrap().unwrap();
            assert_eq!(next.due_at, at("2026-11-21T09:00:00Z"));

            // Off for three days: one ring for all of them.
            let back = h.restarted();
            assert_eq!(back.ring_due(at("2026-11-24T10:30:00Z")).await.unwrap(), 1);
            assert_eq!(back.ring_due(at("2026-11-24T10:31:00Z")).await.unwrap(), 0);
            let next = reminders.get(h.alex, h.note).await.unwrap().unwrap();
            assert_eq!(next.due_at, at("2026-11-25T09:00:00Z"));

            // Done clears that it rang, and it keeps its schedule.
            let left = reminders.done(h.alex, h.note).await.unwrap().unwrap();
            assert_eq!(left.rang_at, None);
            assert_eq!(left.due_at, at("2026-11-25T09:00:00Z"));
        }

        #[tokio::test]
        async fn reminders_are_each_persons_and_go_with_access() {
            let h = home().await;
            let reminders = &h.ringer.reminders;
            h.storage
                .share(h.note, h.sam, ShareLevel::Viewer, h.alex)
                .await
                .unwrap();
            let set = at("2026-11-19T12:00:00Z");
            reminders
                .set(h.sam, h.note, when("2026-11-20T09:00", "UTC", None), set)
                .await
                .unwrap();
            reminders
                .set(h.alex, h.note, when("2026-11-21T09:00", "UTC", None), set)
                .await
                .unwrap();

            assert_eq!(
                h.ringer.ring_due(at("2026-11-20T09:00:00Z")).await.unwrap(),
                1
            );
            assert_eq!(h.rung(h.sam).await, 1);
            assert_eq!(h.rung(h.alex).await, 0);

            // Once Sam can't open the note, Sam's reminder goes, and Alex's
            // stays.
            reminders
                .snooze(h.sam, h.note, at("2026-11-20T10:00:00Z"))
                .await
                .unwrap();
            h.storage.unshare(h.note, h.sam, h.alex).await.unwrap();
            assert_eq!(
                h.ringer.ring_due(at("2026-11-20T10:00:00Z")).await.unwrap(),
                0
            );
            assert_eq!(reminders.get(h.sam, h.note).await.unwrap(), None);
            assert!(reminders.get(h.alex, h.note).await.unwrap().is_some());
        }

        #[tokio::test]
        async fn a_note_in_the_trash_doesnt_ring() {
            let h = home().await;
            let reminders = &h.ringer.reminders;
            reminders
                .set(
                    h.alex,
                    h.note,
                    when("2026-11-20T09:00", "UTC", None),
                    at("2026-11-19T12:00:00Z"),
                )
                .await
                .unwrap();
            h.storage.delete(h.note).await.unwrap();
            assert_eq!(
                h.ringer.ring_due(at("2026-11-20T09:00:00Z")).await.unwrap(),
                0
            );
            assert_eq!(
                h.ringer.ring_due(at("2026-11-20T09:05:00Z")).await.unwrap(),
                0
            );
            assert_eq!(h.rung(h.alex).await, 0);
        }
    }

    #[test]
    fn stored_moments_sort_as_text() {
        assert_eq!(stamp(at("2026-11-20T08:00:00.5Z")), "2026-11-20T08:00:00Z");
    }
}
