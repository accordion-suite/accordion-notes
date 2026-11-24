//! Importing Keep's notes from Google Takeout (decision 34), from made-up
//! archives shaped like a real export, read in the sandbox as on a server.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use accordion_core::coedit::{CoEditing, Timing};
use accordion_core::db::Database;
use accordion_core::events::Events;
use accordion_core::identity::{AuthMethod, Client, Identity, NewUser};
use accordion_core::live::Live;
use accordion_core::storage::Storage;
use accordion_core::takeout::imports::{Import, State};
use accordion_core::takeout::{self, Imports, Take, Takeout};
use accordion_core::texts::{self, Texts};
use accordion_core::thumbnails::{Thumbnails, Tools};
use accordion_core::transcripts::Transcripts;
use accordion_module::{Module, Services, Stopping};
use accordion_notes::Notes;
use accordion_types::{ItemId, Role, UserId};
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::sync::{oneshot, watch};
use tower::ServiceExt;
use zip::write::SimpleFileOptions;

const LAUNCHER: &str = env!("CARGO_BIN_EXE_accordion-notes-sandbox");

struct Server {
    _dir: tempfile::TempDir,
    _stop: watch::Sender<bool>,
    router: Router,
    storage: Storage,
    imports: Imports,
    person: UserId,
    bearer: String,
    parts: Vec<ItemId>,
}

async fn start() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(&dir.path().join("accordion.db")).unwrap();
    let identity = Identity::new(db.clone()).await.unwrap();
    let storage = Storage::open(db.clone(), identity.clone(), &dir.path().join("storage"))
        .await
        .unwrap();
    // Voice recordings are converted by ffmpeg, in the sandbox.
    let thumbnails = Thumbnails::open(
        db.clone(),
        storage.clone(),
        dir.path(),
        Tools::installed(LAUNCHER),
    )
    .await
    .unwrap();
    let texts = Texts::open(
        db.clone(),
        storage.clone(),
        thumbnails.clone(),
        dir.path(),
        texts::Tools::installed("/nonexistent"),
    )
    .await
    .unwrap();
    let events = Events::open(db.clone()).await.unwrap();
    let scratch = dir.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let imports = Imports::open(
        db.clone(),
        storage.clone(),
        events.clone(),
        Takeout::new(takeout::Tools::installed(LAUNCHER), scratch),
    )
    .await
    .unwrap();
    let services = Services {
        db: db.clone(),
        coediting: CoEditing::open(
            db.clone(),
            storage.clone(),
            identity.clone(),
            Timing::default(),
        )
        .await
        .unwrap(),
        identity: identity.clone(),
        storage: storage.clone(),
        events,
        thumbnails,
        texts,
        transcripts: Transcripts::canned(db.clone(), storage.clone(), dir.path(), |_| None)
            .await
            .unwrap(),
        live: Live::new(db, &identity),
        link_origin: watch::channel(None).1,
        takeout: imports.clone(),
    };
    let (stop, stopping) = watch::channel(false);
    let started = Notes
        .start(services, Stopping::new(stopping))
        .await
        .unwrap();
    if let Some(background) = started.background {
        tokio::spawn(background);
    }
    let user = identity
        .create_user(NewUser {
            username: "sam".into(),
            display_name: "Sam".into(),
            role: Role::Owner,
            password: "correct horse".into(),
        })
        .await
        .unwrap();
    let (token, _) = identity
        .create_session(user.id, Client::App, "Laptop", AuthMethod::Password)
        .await
        .unwrap();

    let top = storage.top_folder(user.id).await.unwrap();
    let mut parts = Vec::new();
    for (name, files) in [
        ("takeout-1.zip", notes_part()),
        ("takeout-2.zip", files_part()),
    ] {
        let path = dir.path().join(name);
        zip_part(&path, &files);
        let file = tokio::fs::File::open(&path).await.unwrap();
        parts.push(storage.create_file(top, name, file).await.unwrap().id);
    }
    Server {
        router: Router::new().nest("/api/notes", started.routes.api.unwrap()),
        storage,
        imports,
        person: user.id,
        bearer: format!("Bearer {}", token.as_str()),
        parts,
        _dir: dir,
        _stop: stop,
    }
}

/// A line of text as Keep's HTML has it.
fn p(tag: &str, text: &str, weight: u32) -> String {
    format!(
        "<{tag} dir=\"ltr\" style=\"line-height:1.38;margin-top:0.0pt;\"><span style=\"\
         font-size:11.0pt;font-family:'Google Sans Text';color:#000000;font-weight:{weight};\
         font-style:normal;text-decoration:none;white-space:pre-wrap;\">{text}</span></{tag}>"
    )
}

/// A note as Keep's JSON has it, with `extra` fields added.
fn note(title: &str, created: u64, text: &str, extra: Value) -> Vec<u8> {
    let mut note = json!({
        "color": "DEFAULT",
        "isTrashed": false,
        "isPinned": false,
        "isArchived": false,
        "textContent": text,
        "title": title,
        "userEditedTimestampUsec": created + 60_000_000,
        "createdTimestampUsec": created,
        "textContentHtml": p("p", text, 400),
    });
    for (key, value) in extra.as_object().unwrap() {
        note[key] = value.clone();
    }
    note.to_string().into_bytes()
}

/// November 2025, in microseconds, plus `minutes`.
fn at(minutes: u64) -> u64 {
    1_762_000_000_000_000 + minutes * 60_000_000
}

/// Three seconds of AMR-NB, as Keep records: each frame a one-byte header
/// and 31 bytes of sound.
fn amr() -> Vec<u8> {
    let mut amr = b"#!AMR\n".to_vec();
    for _ in 0..150 {
        amr.push(0x3c);
        amr.extend([0; 31]);
    }
    amr
}

/// The part with the notes, as Takeout's first part has them.
fn notes_part() -> Vec<(String, Vec<u8>)> {
    let keep = |name: &str, contents: Vec<u8>| (format!("Takeout/Keep/{name}"), contents);
    let formatted = [
        p("p", "Before the trip", 400),
        p("h1", "Packing", 400),
        p("p", "Passports", 700),
    ]
    .concat();
    vec![
        keep(
            "Groceries.json",
            note(
                "Groceries",
                at(1),
                "",
                json!({
                    "color": "CERULEAN",
                    "isPinned": true,
                    "labels": [{ "name": "Home" }],
                    "attachments": [{ "filePath": "111.222.jpg", "mimetype": "image/jpeg" }],
                    "listContent": [
                        { "text": "Milk", "isChecked": false },
                        { "text": "Bread", "isChecked": true },
                    ],
                    "textContent": null,
                    "textContentHtml": null,
                }),
            ),
        ),
        keep("Groceries.html", b"<html>Groceries</html>".to_vec()),
        keep(
            "Trip.json",
            note(
                "Trip",
                at(2),
                "Before the trip\nPacking\nPassports",
                json!({ "textContentHtml": formatted }),
            ),
        ),
        keep(
            "2025-11-01T12_00_00.000+00_00.json",
            note("", at(3), "Call the plumber about the sink", json!({})),
        ),
        keep("Plans.json", note("Plans", at(4), "First plans", json!({}))),
        keep(
            "Plans(1).json",
            note("Plans", at(5), "Second plans", json!({})),
        ),
        keep(
            "Voice.json",
            note(
                "Voice",
                at(6),
                "what Keep heard",
                json!({
                    "attachments": [{ "filePath": "aaa.bbb.3gp", "mimetype": "audio/3gp" }],
                    "tasks": [{ "id": "abc" }],
                }),
            ),
        ),
        keep(
            "Old.json",
            note(
                "Old",
                at(7),
                "Thrown away",
                json!({
                    "isTrashed": true,
                    "attachments": [{ "filePath": "333.444.png", "mimetype": "image/png" }],
                }),
            ),
        ),
        keep(
            "Shared.json",
            note(
                "Shared",
                at(8),
                "With Alex",
                json!({
                    "color": "RED",
                    "isArchived": true,
                    "sharees": [{ "isOwner": false, "type": "USER", "email": "alex@example.com" }],
                }),
            ),
        ),
        keep(
            "Link.json",
            note(
                "Link",
                at(9),
                "Worth reading",
                json!({
                    "annotations": [{
                        "source": "WEBLINK",
                        "url": "https://news.example/story",
                        "title": "A story",
                        "description": "What it's about",
                    }],
                }),
            ),
        ),
        keep(
            &format!("{}.json", "a".repeat(50)),
            note(&"a".repeat(300), at(10), "A long title", json!({})),
        ),
        keep(
            "Missing.json",
            note(
                "Missing",
                at(11),
                "Its picture is gone",
                json!({ "attachments": [{ "filePath": "gone.png", "mimetype": "image/png" }] }),
            ),
        ),
        keep("Broken.json", b"{ not a note".to_vec()),
        keep("Labels.txt", b"Home\n".to_vec()),
    ]
}

/// The part with the attachments, as Takeout's later parts can have them.
fn files_part() -> Vec<(String, Vec<u8>)> {
    vec![
        (
            "Takeout/Keep/111.222.jpg".to_owned(),
            [&[0xff, 0xd8, 0xff, 0xe0][..], b"a photo"].concat(),
        ),
        (
            "Takeout/Keep/333.444.png".to_owned(),
            b"\x89PNG\r\n\x1a\na drawing".to_vec(),
        ),
        ("Takeout/Keep/aaa.bbb.3gp".to_owned(), amr()),
    ]
}

fn zip_part(path: &Path, files: &[(String, Vec<u8>)]) {
    let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    for (name, contents) in files {
        zip.start_file(name, SimpleFileOptions::default()).unwrap();
        zip.write_all(contents).unwrap();
    }
    zip.finish().unwrap();
}

impl Server {
    /// Imports Keep from the parts, and waits for it to end.
    async fn import(&self) -> Import {
        self.imports
            .start(self.person, &self.parts, &[Take::Product("keep".into())])
            .await
            .unwrap();
        let (ended, end) = oneshot::channel::<()>();
        let runner = tokio::spawn(self.imports.clone().run(async move {
            let _ = end.await;
        }));
        let latest = loop {
            let latest = self.imports.list(self.person, 1).await.unwrap().remove(0);
            if !matches!(latest.state, State::Running | State::Canceling) {
                break latest;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let _ = ended.send(());
        tokio::time::timeout(Duration::from_secs(60), runner)
            .await
            .unwrap()
            .unwrap();
        latest
    }

    async fn get(&self, path: &str) -> Value {
        let request = Request::builder()
            .uri(format!("/api/notes{path}"))
            .header("authorization", &self.bearer)
            .body(Body::empty())
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        assert!(
            response.status().is_success(),
            "{path}: {}",
            response.status()
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// The names in the person's Notes place.
    async fn names(&self) -> Vec<String> {
        let top = self
            .storage
            .place_folder(self.person, "notes")
            .await
            .unwrap();
        let mut names: Vec<String> = self
            .storage
            .children(top)
            .await
            .unwrap()
            .into_iter()
            .map(|item| item.name)
            .collect();
        names.sort();
        names
    }

    async fn text(&self, id: ItemId) -> String {
        let mut text = String::new();
        self.storage
            .open_file(id)
            .await
            .unwrap()
            .read_to_string(&mut text)
            .await
            .unwrap();
        text
    }

    async fn unconverted(&self) -> Vec<String> {
        let top = self.storage.top_folder(self.person).await.unwrap();
        let folder = self
            .storage
            .find(
                top,
                vec!["Google Takeout".into(), "Unconverted".into(), "Keep".into()],
            )
            .await
            .unwrap()
            .unwrap();
        let mut names: Vec<String> = self
            .storage
            .children(folder.id)
            .await
            .unwrap()
            .into_iter()
            .map(|item| item.name)
            .collect();
        names.sort();
        names
    }
}

#[tokio::test]
async fn imports_keeps_notes_into_notes() {
    let server = start().await;
    let import = server.import().await;
    assert_eq!(import.state, State::Done, "{:?}", import.error);
    assert_eq!((import.items, import.done), (Some(12), 12));

    let long = format!("{}.md", "a".repeat(100));
    let mut expected = vec![
        "Call the plumber about the sink.md",
        "Groceries 1.jpg",
        "Groceries.md",
        "Link.md",
        "Missing.md",
        "Plans (2).md",
        "Plans.md",
        "Shared.md",
        "Trip.md",
        "Voice 1.ogg",
        "Voice.md",
        &long,
    ];
    expected.sort();
    assert_eq!(server.names().await, expected);

    let list = server.get("/notes").await;
    let notes = list["notes"].as_array().unwrap();
    let find = |title: &str| {
        notes
            .iter()
            .find(|note| note["title"] == title)
            .unwrap_or_else(|| panic!("{title} isn't in {notes:#?}"))
    };
    let groceries = find("Groceries");
    assert_eq!(
        groceries["body"],
        "![](Groceries%201.jpg)\n\n- [ ] Milk\n- [x] Bread\n"
    );
    assert_eq!(groceries["color"], "dark-blue");
    assert_eq!(groceries["pinned"], true);
    assert_eq!(groceries["tags"], json!(["Home"]));
    assert!(
        groceries["created_at"]
            .as_str()
            .unwrap()
            .starts_with("2025-11-01")
    );
    assert_eq!(
        find("Trip")["body"],
        "Before the trip\n# Packing\n**Passports**"
    );
    assert_eq!(
        find("Call the plumber about the sink")["body"],
        "Call the plumber about the sink"
    );
    assert_eq!(find("Plans")["body"], "First plans");
    assert_eq!(find("Plans (2)")["body"], "Second plans");
    assert_eq!(
        find("Voice")["body"],
        "![](Voice%201.ogg)\n\nwhat Keep heard"
    );
    let shared = find("Shared");
    assert_eq!(
        (&shared["color"], &shared["archived"]),
        (&json!("red"), &json!(true))
    );
    assert_eq!(
        find("Link")["body"],
        "Worth reading\n\n[A story](<https://news.example/story>)\n"
    );
    assert_eq!(find("Missing")["body"], "Its picture is gone");

    // The recording plays in browsers now.
    let top = server
        .storage
        .place_folder(server.person, "notes")
        .await
        .unwrap();
    let voice = server
        .storage
        .find(top, vec!["Voice 1.ogg".into()])
        .await
        .unwrap()
        .unwrap();
    let mut opus = Vec::new();
    server
        .storage
        .open_file(voice.id)
        .await
        .unwrap()
        .read_to_end(&mut opus)
        .await
        .unwrap();
    assert!(opus.starts_with(b"OggS"));

    // The trashed note is in Notes' trash, and its picture came back with
    // it.
    let trash = server.get("/trash").await;
    assert_eq!(trash["notes"][0]["title"], "Old");
    let old = trash["notes"][0]["id"].as_str().unwrap().to_owned();
    let restore = Request::builder()
        .method("POST")
        .uri(format!("/api/notes/trash/{old}/restore"))
        .header("authorization", &server.bearer)
        .body(Body::empty())
        .unwrap();
    let restored = server.router.clone().oneshot(restore).await.unwrap();
    assert!(restored.status().is_success());
    assert!(server.names().await.contains(&"Old 1.png".to_owned()));

    // Keep's recording and the file that isn't a note are kept.
    assert_eq!(server.unconverted().await, ["Broken.json", "aaa.bbb.3gp"]);

    let report = server.text(import.report.unwrap()).await;
    for line in [
        "- 11 notes from Keep, in Notes\n",
        "## Not converted\n\n- Broken.json: it isn't a note Accordion can read\n",
        "## Attachments not in the archives\n\n- **Missing**: gone.png\n",
        "## Voice recordings\n\n- **Voice**: converted so browsers can play it.",
        "## Reminders to set again\n\n- **Voice**\n",
        "## Notes that were shared\n\n- **Shared**: shared with alex@example.com\n",
        "## Names that were shortened\n\n- `aaaa",
    ] {
        assert!(report.contains(line), "{line:?} isn't in:\n{report}");
    }

    // Importing again brings nothing twice.
    let again = server.import().await;
    assert_eq!(again.state, State::Done);
    let report = server.text(again.report.unwrap()).await;
    assert!(report.contains("Nothing new came over."), "{report}");
    assert!(report.contains("- **Groceries**\n"), "{report}");
    assert_eq!(server.names().await.len(), expected.len() + 2);
}
