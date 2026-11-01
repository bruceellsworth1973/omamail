//! Undo history for Microsoft 365: a place is `{"folderId"}`, the Graph id of the folder a message is
//! in, as Graph spells it in `parentFolderId`. See `crate::mail::place` and docs/BACKEND.md "Undo
//! history" for the contract every provider keeps.
//!
//! Locate reads `parentFolderId` live, one request per message. Place reads it again and, unless the
//! message is already there, moves it with `POST /me/messages/{id}/move`; a move keeps the immutable
//! id, and an answer naming another one is that message's new id. A move refused as "not found" is
//! told apart by asking for the folder: a folder Graph no longer has is `undo_folder_gone`. Requests
//! follow `m365.modify`'s rules: four at a time, every one ending by the call's deadline, none begun
//! in its last fifth (`m365_timeout`), and one already begun waited for.
use std::future::Future;
use std::pin::Pin;

use futures_util::StreamExt as _;
use reqwest::Method;
use serde_json::{Value, json};
use tokio::time::Instant;

use super::graph::{Graph, MAX_JSON, Mode, NOT_FOUND, url};
use super::{cancel, ids, read};
use crate::mail::Account;
use crate::mail::place::{Located, PlaceItem, PlaceOutcome, Placed};

const INVALID: &str = "invalid_params";
const BAD: &str = "m365_invalid_response";
const LATE: &str = "m365_timeout";
const GONE: &str = "undo_folder_gone";
/// Graph folder ids are URL-safe base64 of about 120 characters; anything longer is not one.
const MAX_FOLDER_ID: usize = 512;
/// Requests in flight at once: the mailbox's own limit.
const IN_FLIGHT: usize = 4;

/// A folder id as Graph writes one: URL-safe base64 only, so it can name nothing else in a request.
fn folder_id(id: &str) -> bool {
    (1..=MAX_FOLDER_ID).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'='))
}

/// The folder of a place, refused unless exactly what `locate` produces.
fn place_folder(place: &Value) -> Result<&str, &'static str> {
    let object = place.as_object().ok_or(INVALID)?;
    if object.len() != 1 {
        return Err(INVALID);
    }
    object
        .get("folderId")
        .and_then(Value::as_str)
        .filter(|id| folder_id(id))
        .ok_or(INVALID)
}

/// The exact shape of a Microsoft 365 place; unknown keys and out-of-range values are
/// refused (`invalid_params` or an `undo_*` code), never guessed at.
pub(crate) fn validate_place(place: &Value) -> Result<(), &'static str> {
    place_folder(place).map(|_| ())
}

type Ask<'a, T> = Pin<Box<dyn Future<Output = Result<T, &'static str>> + Send + 'a>>;

/// The mailbox calls a place needs, by Graph id; tests answer them in memory.
trait Remote: Send + Sync {
    /// The `parentFolderId` Graph answers for the message now.
    fn parent<'a>(&'a self, graph_id: &'a str) -> Ask<'a, Value>;
    /// Moves the message; answers the `id` Graph gave the moved message, if its answer said.
    fn move_to<'a>(&'a self, graph_id: &'a str, folder: &'a str) -> Ask<'a, Option<String>>;
    /// Whether Graph still has the folder.
    fn folder_exists<'a>(&'a self, folder: &'a str) -> Ask<'a, bool>;
}

/// The Graph id of a message and the folder it is in now.
async fn current(remote: &impl Remote, id: &str) -> Result<(String, String), &'static str> {
    let graph_id = ids::decode(id)?;
    let parent = remote.parent(&graph_id).await?;
    let folder = parent
        .as_str()
        .filter(|folder| folder_id(folder))
        .ok_or(BAD)?;
    Ok((graph_id, folder.to_owned()))
}

async fn locate_with(remote: &impl Remote, ids: &[String]) -> Vec<Located> {
    // Boxed before streaming: a closure returning borrowed futures is not general enough for the
    // backend's Send futures.
    let reads: Vec<Ask<'_, Value>> = ids
        .iter()
        .map(|id| -> Ask<'_, Value> {
            Box::pin(async move {
                let (_, folder) = current(remote, id).await?;
                Ok(json!({"folderId": folder}))
            })
        })
        .collect();
    futures_util::stream::iter(reads)
        .buffered(IN_FLIGHT)
        .collect()
        .await
}

async fn place_one(remote: &impl Remote, item: &PlaceItem) -> PlaceOutcome {
    let wanted = place_folder(&item.place)?;
    let (graph_id, folder) = current(remote, &item.id).await?;
    if folder == wanted {
        return Ok(Placed {
            new_id: item.id.clone(),
            moved: false,
        });
    }
    match remote.move_to(&graph_id, wanted).await {
        Ok(Some(new)) if new != graph_id => Ok(Placed {
            new_id: ids::encode(&new)?,
            moved: true,
        }),
        Ok(_) => Ok(Placed {
            new_id: item.id.clone(),
            moved: true,
        }),
        // The message was there a moment ago: "not found" is the folder unless Graph still has it.
        Err(NOT_FOUND) => match remote.folder_exists(wanted).await {
            Ok(false) => Err(GONE),
            _ => Err(NOT_FOUND),
        },
        Err(code) => Err(code),
    }
}

async fn place_with(remote: &impl Remote, items: &[PlaceItem]) -> Vec<PlaceOutcome> {
    let moves: Vec<Ask<'_, Placed>> = items
        .iter()
        .map(|item| -> Ask<'_, Placed> { Box::pin(place_one(remote, item)) })
        .collect();
    futures_util::stream::iter(moves)
        .buffered(IN_FLIGHT)
        .collect()
        .await
}

/// One call's access to the mailbox: every request ends by the call's deadline and none is begun in
/// its last fifth, as in `m365.modify`.
struct Live {
    graph: Graph,
    last_start: Instant,
}

impl Live {
    fn new(account: &Account) -> Result<Live, &'static str> {
        let account = ids::account_id(&account.id)?;
        let limit = cancel::limit(cancel::DEADLINE);
        let end = Instant::now() + limit;
        Ok(Live {
            graph: Graph::new(account).until(end),
            last_start: end - limit / 5,
        })
    }

    fn begin(&self) -> Result<(), &'static str> {
        if Instant::now() < self.last_start {
            Ok(())
        } else {
            Err(LATE)
        }
    }
}

impl Remote for Live {
    fn parent<'a>(&'a self, graph_id: &'a str) -> Ask<'a, Value> {
        Box::pin(async move {
            self.begin()?;
            let url = url(
                &["me", "messages", graph_id],
                &[("$select", "parentFolderId")],
            )?;
            let message = self.graph.get(&url).await.map_err(|fault| fault.code)?;
            Ok(message["parentFolderId"].clone())
        })
    }

    fn move_to<'a>(&'a self, graph_id: &'a str, folder: &'a str) -> Ask<'a, Option<String>> {
        Box::pin(async move {
            self.begin()?;
            let url = url(&["me", "messages", graph_id, "move"], &[])?;
            let body = json!({"destinationId": folder}).to_string();
            // The answer is the moved message; one too long to read keeps the id.
            let reply = self
                .graph
                .request(
                    Mode::Write,
                    Method::POST,
                    &url,
                    Some(("application/json", body.as_bytes())),
                    MAX_JSON,
                )
                .await
                .map_err(|fault| fault.code)?;
            Ok(serde_json::from_slice::<Value>(&reply.body)
                .ok()
                .and_then(|message| message["id"].as_str().map(str::to_owned)))
        })
    }

    fn folder_exists<'a>(&'a self, folder: &'a str) -> Ask<'a, bool> {
        Box::pin(async move {
            self.begin()?;
            let url = url(&["me", "mailFolders", folder], &[("$select", "id")])?;
            match self.graph.get(&url).await {
                Ok(_) => Ok(true),
                Err(fault) if fault.code == NOT_FOUND => Ok(false),
                Err(fault) => Err(fault.code),
            }
        })
    }
}

/// Read-only, live from the server: one entry per id, in order. `ids` are
/// unique and already pass `crate::mail::action::validate_message_id`.
pub(crate) async fn locate(
    account: &Account,
    ids: &[String],
) -> Result<Vec<Located>, &'static str> {
    let live = Live::new(account)?;
    Ok(locate_with(&live, ids).await)
}

/// Moves each message to its place: one entry per item, in order. Places have
/// passed `validate_place`; a message already there is `Placed { new_id: id,
/// moved: false }` with no write.
pub(crate) async fn place(
    account: &Account,
    items: &[PlaceItem],
) -> Result<Vec<PlaceOutcome>, &'static str> {
    let live = Live::new(account)?;
    let ids: Vec<String> = items.iter().map(|item| item.id.clone()).collect();
    read::forget(&live.graph.account, Some(&ids));
    Ok(place_with(&live, items).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// The mailbox in memory: each message's folder by Graph id, the folders that exist, every move
    /// recorded. A message named in `renamed` comes back from a move under that other id.
    #[derive(Default)]
    struct Fake {
        folders: Mutex<HashMap<String, String>>,
        existing: Vec<String>,
        moves: Mutex<Vec<(String, String)>>,
        refuse: Option<&'static str>,
        renamed: HashMap<String, String>,
    }

    impl Fake {
        fn now(&self, graph_id: &str) -> String {
            self.folders.lock().unwrap()[graph_id].clone()
        }
        fn moves(&self) -> Vec<(String, String)> {
            self.moves.lock().unwrap().clone()
        }
    }

    impl Remote for Fake {
        fn parent<'a>(&'a self, graph_id: &'a str) -> Ask<'a, Value> {
            Box::pin(async move {
                self.folders
                    .lock()
                    .unwrap()
                    .get(graph_id)
                    .map(|folder| json!(folder))
                    .ok_or(NOT_FOUND)
            })
        }

        fn move_to<'a>(&'a self, graph_id: &'a str, folder: &'a str) -> Ask<'a, Option<String>> {
            Box::pin(async move {
                self.moves
                    .lock()
                    .unwrap()
                    .push((graph_id.to_owned(), folder.to_owned()));
                if let Some(code) = self.refuse {
                    return Err(code);
                }
                if !self.existing.iter().any(|known| known == folder) {
                    return Err(NOT_FOUND);
                }
                let mut store = self.folders.lock().unwrap();
                store.remove(graph_id).ok_or(NOT_FOUND)?;
                let id = self.renamed.get(graph_id).map_or(graph_id, String::as_str);
                store.insert(id.to_owned(), folder.to_owned());
                Ok(Some(id.to_owned()))
            })
        }

        fn folder_exists<'a>(&'a self, folder: &'a str) -> Ask<'a, bool> {
            Box::pin(async move { Ok(self.existing.iter().any(|known| known == folder)) })
        }
    }

    const INBOX: &str = "AAMkAGI2inbox0AAA=";
    const ARCHIVE: &str = "AAMkAGI2archiveAAA=";
    const PROJECTS: &str = "AAMkAGI2-projects_AAA=";
    const DELETED: &str = "AAMkAGI2deletedAAA=";

    fn gid(n: u32) -> String {
        format!("AAMkAGI2{n:04}AAA=")
    }

    fn oid(n: u32) -> String {
        ids::encode(&gid(n)).unwrap()
    }

    /// Messages by number in their folders; the four folders above exist.
    fn mailbox(messages: &[(u32, &str)]) -> Fake {
        Fake {
            folders: Mutex::new(
                messages
                    .iter()
                    .map(|(n, folder)| (gid(*n), (*folder).to_owned()))
                    .collect(),
            ),
            existing: [INBOX, ARCHIVE, PROJECTS, DELETED]
                .map(String::from)
                .to_vec(),
            ..Fake::default()
        }
    }

    fn item(n: u32, folder: &str) -> PlaceItem {
        PlaceItem {
            id: oid(n),
            place: json!({"folderId": folder}),
        }
    }

    fn moved(n: u32) -> PlaceOutcome {
        Ok(Placed {
            new_id: oid(n),
            moved: true,
        })
    }

    fn stayed(n: u32) -> PlaceOutcome {
        Ok(Placed {
            new_id: oid(n),
            moved: false,
        })
    }

    #[tokio::test]
    async fn locate_answers_each_message_s_folder_in_order() {
        let remote = mailbox(&[(1, INBOX), (2, PROJECTS), (4, "folder/../x")]);
        let ids = [oid(1), oid(3), oid(2), oid(4), "not-an-id".into()];
        let located = locate_with(&remote, &ids).await;
        assert_eq!(
            located,
            vec![
                Ok(json!({"folderId": INBOX})),
                Err(NOT_FOUND),
                Ok(json!({"folderId": PROJECTS})),
                Err(BAD),
                Err(INVALID),
            ]
        );
        for place in located.into_iter().flatten() {
            assert_eq!(validate_place(&place), Ok(()));
        }
        assert!(remote.moves().is_empty());
    }

    #[test]
    fn invalid_places_are_refused() {
        for place in [
            json!([]),
            json!({}),
            json!(null),
            json!({"folderId": INBOX, "extra": true}),
            json!({"folder": INBOX}),
            json!({"folderId": 7}),
            json!({"folderId": [INBOX]}),
            json!({"folderId": null}),
            json!({"folderId": ""}),
            json!({"folderId": "../me"}),
            json!({"folderId": "AAMk/AAA="}),
            json!({"folderId": "AAMk+AAA="}),
            json!({"folderId": "AAMk AAA="}),
            json!({"folderId": "AAMk%2FAAA"}),
            json!({"folderId": "AAMk\u{e9}AAA"}),
            json!({"folderId": "x".repeat(MAX_FOLDER_ID + 1)}),
        ] {
            assert_eq!(validate_place(&place), Err(INVALID), "{place}");
        }
        for place in [
            json!({"folderId": INBOX}),
            json!({"folderId": PROJECTS}),
            json!({"folderId": "x".repeat(MAX_FOLDER_ID)}),
        ] {
            assert_eq!(validate_place(&place), Ok(()), "{place}");
        }
    }

    #[tokio::test]
    async fn place_moves_to_a_custom_folder() {
        let remote = mailbox(&[(1, INBOX), (2, DELETED)]);
        let outcome = place_with(&remote, &[item(1, PROJECTS), item(2, ARCHIVE)]).await;
        assert_eq!(outcome, vec![moved(1), moved(2)]);
        assert_eq!(
            remote.moves(),
            vec![(gid(1), PROJECTS.into()), (gid(2), ARCHIVE.into())]
        );
        assert_eq!(remote.now(&gid(1)), PROJECTS);
        assert_eq!(remote.now(&gid(2)), ARCHIVE);
    }

    #[tokio::test]
    async fn a_message_already_in_place_is_not_moved() {
        let remote = mailbox(&[(1, PROJECTS)]);
        assert_eq!(
            place_with(&remote, &[item(1, PROJECTS)]).await,
            vec![stayed(1)]
        );
        assert!(remote.moves().is_empty());
    }

    #[tokio::test]
    async fn a_folder_that_is_gone_is_said_so_for_that_message_only() {
        let remote = mailbox(&[(1, INBOX), (2, INBOX)]);
        let gone = "AAMkAGI2removedAAA=";
        let outcome = place_with(&remote, &[item(1, gone), item(2, ARCHIVE)]).await;
        assert_eq!(outcome, vec![Err(GONE), moved(2)]);
        assert_eq!(remote.now(&gid(1)), INBOX);
    }

    #[tokio::test]
    async fn failures_are_per_message() {
        // A message that is not there, beside one moved and one already in place.
        let remote = mailbox(&[(1, INBOX), (3, INBOX)]);
        let outcome = place_with(
            &remote,
            &[item(2, ARCHIVE), item(1, ARCHIVE), item(3, INBOX)],
        )
        .await;
        assert_eq!(outcome, vec![Err(NOT_FOUND), moved(1), stayed(3)]);
        let mut refused = mailbox(&[(1, INBOX), (2, ARCHIVE)]);
        refused.refuse = Some("m365_access_denied");
        let outcome = place_with(&refused, &[item(1, ARCHIVE), item(2, ARCHIVE)]).await;
        assert_eq!(outcome, vec![Err("m365_access_denied"), stayed(2)]);
        assert_eq!(refused.moves(), vec![(gid(1), ARCHIVE.into())]);
        assert_eq!(refused.now(&gid(1)), INBOX);
    }

    #[tokio::test]
    async fn a_move_that_answers_another_id_returns_it() {
        let mut remote = mailbox(&[(1, INBOX)]);
        remote.renamed.insert(gid(1), gid(7));
        assert_eq!(
            place_with(&remote, &[item(1, ARCHIVE)]).await,
            vec![moved(7)]
        );
        // Moved, but under an id that cannot be carried: not confirmed.
        let mut unusable = mailbox(&[(1, INBOX)]);
        unusable.renamed.insert(gid(1), "short".into());
        assert_eq!(
            place_with(&unusable, &[item(1, ARCHIVE)]).await,
            vec![Err("m365_bad_response")]
        );
    }

    #[tokio::test]
    async fn locate_and_place_cycles_are_exact() {
        let remote = mailbox(&[(1, PROJECTS)]);
        let ids = [oid(1)];
        let before = locate_with(&remote, &ids).await[0].clone().unwrap();
        for destination in [DELETED, ARCHIVE, INBOX] {
            for _ in 0..3 {
                // The action, then undo (locate kept for redo, place back), then redo.
                assert_eq!(
                    place_with(&remote, &[item(1, destination)]).await,
                    vec![moved(1)]
                );
                let redo = locate_with(&remote, &ids).await[0].clone().unwrap();
                assert_eq!(redo, json!({"folderId": destination}));
                let back = PlaceItem {
                    id: oid(1),
                    place: before.clone(),
                };
                assert_eq!(place_with(&remote, &[back]).await, vec![moved(1)]);
                assert_eq!(locate_with(&remote, &ids).await[0], Ok(before.clone()));
                let again = PlaceItem {
                    id: oid(1),
                    place: redo,
                };
                assert_eq!(place_with(&remote, &[again]).await, vec![moved(1)]);
                let back = PlaceItem {
                    id: oid(1),
                    place: before.clone(),
                };
                assert_eq!(place_with(&remote, &[back]).await, vec![moved(1)]);
            }
        }
        assert_eq!(remote.moves().len(), 36);
        assert_eq!(remote.now(&gid(1)), PROJECTS);
    }

    /// The same over the wire: what Graph is asked, with the immutable-id preference, and the
    /// folder probe after a move to a folder the mailbox no longer has.
    #[tokio::test]
    async fn live_requests_read_the_parent_and_post_the_move() {
        use super::super::model::Role;
        use super::super::tests::{ACCOUNT, State, World, gid, isolated, oid};
        isolated!();
        let world = World::new().await;
        {
            let mut state = world.fake.state();
            state.add(1, Role::Inbox, "One");
            state.add(2, Role::Inbox, "Two");
        }
        let account = Account {
            id: ACCOUNT.into(),
            provider: crate::mail::Provider::M365,
        };
        let inbox = json!({"folderId": State::folder_id(Role::Inbox)});
        let archive = json!({"folderId": State::folder_id(Role::Archive)});
        assert_eq!(
            locate(&account, &[oid(1)]).await.unwrap(),
            vec![Ok(inbox.clone())]
        );
        let items = [
            PlaceItem {
                id: oid(1),
                place: archive.clone(),
            },
            PlaceItem {
                id: oid(2),
                place: inbox.clone(),
            },
        ];
        assert_eq!(
            place(&account, &items).await.unwrap(),
            vec![
                Ok(Placed {
                    new_id: oid(1),
                    moved: true
                }),
                Ok(Placed {
                    new_id: oid(2),
                    moved: false
                }),
            ]
        );
        assert_eq!(world.fake.state().msg(1).folder, Role::Archive);
        let sent = world.fake.graph();
        assert!(
            sent.iter()
                .all(|r| r.header("prefer") == Some("IdType=\"ImmutableId\""))
        );
        let reads: Vec<_> = sent.iter().filter(|r| r.method == "GET").collect();
        assert_eq!(reads.len(), 3);
        assert!(
            reads
                .iter()
                .all(|r| r.param("$select") == Some("parentFolderId"))
        );
        let moves: Vec<_> = sent.iter().filter(|r| r.method == "POST").collect();
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].path, format!("/v1.0/me/messages/{}/move", gid(1)));
        assert_eq!(
            moves[0].json(),
            json!({"destinationId": State::folder_id(Role::Archive)})
        );
        // The Archive is deleted: the way back there is gone, the message stays where it is.
        world
            .fake
            .state()
            .roles
            .retain(|role| *role != Role::Archive);
        let back = [PlaceItem {
            id: oid(2),
            place: archive,
        }];
        assert_eq!(place(&account, &back).await.unwrap(), vec![Err(GONE)]);
        assert_eq!(world.fake.state().msg(2).folder, Role::Inbox);
        let last = world.fake.graph().pop().unwrap();
        assert_eq!(
            last.path,
            format!("/v1.0/me/mailFolders/{}", State::folder_id(Role::Archive))
        );
    }
}
