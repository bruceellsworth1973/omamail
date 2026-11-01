//! Undo history for HEY. A place is `{"box": <kind>}`: the box a posting is
//! in, by the kind `hey box` answers (`imbox`, `feedbox`, `trailbox`,
//! `asidebox`, `laterbox`, `bubblebox`), or `trash`. Posting ids are stable, so
//! a placed message keeps its id. See `crate::mail::place` and docs/BACKEND.md
//! "Undo history" for the contract every provider keeps.
//!
//! The HEY CLI answers no box for a single posting, so a posting is found by
//! reading the boxes page by page, then the Trash through `hey search --in
//! trash`, stopping as soon as every posting asked about has been seen. One
//! that is in none of them (Spam, a bundle, the Screener) has no place this
//! can name or put back: `undo_unsupported`.
//!
//! Putting a posting back writes only when its box differs: `hey trash` into
//! the Trash, `hey move --to <kind>` into any box but Bubble Up, which is a
//! scheduled time `move` cannot set (`undo_unsupported`). HEY marks a thread
//! seen when it moves into any box but the Imbox, so an unseen one is marked
//! unseen again; and it silently leaves out of Reply Later a thread that
//! cannot be replied to, so a move there is read back before it counts.
use crate::mail::place::{Located, PlaceItem, PlaceOutcome, Placed};
use crate::mail::{Account, Provider};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::{future::Future, pin::Pin, time::Duration};

const IMBOX: &str = "imbox";
const LATER: &str = "laterbox";
const BUBBLE: &str = "bubblebox";
const TRASH: &str = "trash";
/// Every place, in the order a posting is looked for.
const PLACES: [&str; 7] = [
    IMBOX, "feedbox", "trailbox", "asidebox", LATER, BUBBLE, TRASH,
];
/// The CLI's own bound for reading a whole box (`--all`).
const MAX_PAGES: usize = 100;
const MAX_CURSOR: usize = 4096;
const LIMIT: usize = 16 * 1024 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(60);
const WRITE_TIMEOUT: Duration = Duration::from_secs(20);
const INVALID: &str = "mail_place_invalid_response";

/// The box of a place, refused unless exactly what `locate` produces.
fn place_box(place: &Value) -> Result<&'static str, &'static str> {
    let object = place.as_object().ok_or("invalid_params")?;
    if object.len() != 1 {
        return Err("invalid_params");
    }
    let name = object
        .get("box")
        .and_then(Value::as_str)
        .ok_or("invalid_params")?;
    PLACES
        .into_iter()
        .find(|kind| *kind == name)
        .ok_or("invalid_params")
}

/// The exact shape of a HEY place; unknown keys and out-of-range values are
/// refused (`invalid_params` or an `undo_*` code), never guessed at.
pub(crate) fn validate_place(place: &Value) -> Result<(), &'static str> {
    place_box(place).map(|_| ())
}

/// Where a posting is: its box, and whether it has been seen where the
/// listing says (a Trash search answers no seen state).
#[derive(Clone, Copy, Debug)]
struct Found {
    kind: &'static str,
    seen: Option<bool>,
}

type Reply<'a> = Pin<Box<dyn Future<Output = Result<Value, &'static str>> + Send + 'a>>;

/// The account-bound HEY CLI commands a place needs; tests answer them in memory.
trait Remote: Send + Sync {
    /// A read-only listing, `hey box` or `hey search`: its envelope's `data`.
    fn read(&self, args: Vec<String>) -> Reply<'_>;
    /// A mutation, settled once HEY acknowledged it.
    fn write(&self, args: Vec<String>) -> Reply<'_>;
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn numeric(value: &str) -> bool {
    !value.is_empty() && value.len() <= 32 && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// A posting id as HEY answers it: a number, or digits in a string.
fn posting_of(value: &Value) -> Option<String> {
    let text = match value {
        Value::Number(number) => number.as_u64()?.to_string(),
        Value::String(text) => text.trim().to_owned(),
        _ => return None,
    };
    numeric(&text).then_some(text)
}

fn page_args(kind: &str, cursor: &str) -> Vec<String> {
    if kind == TRASH {
        return strings(&["search", "--in", TRASH, "--json", "--page", cursor]);
    }
    let mut args = strings(&["box", kind, "--json"]);
    if !cursor.is_empty() {
        args.extend(strings(&["--page", cursor]));
    }
    args
}

type Page = (Vec<(String, Found)>, String);

/// One page of a box or of the Trash: its postings and the cursor after it,
/// empty at the end. A box that answers as another kind is not believed.
fn page(kind: &'static str, cursor: &str, data: &Value) -> Result<Page, &'static str> {
    if kind == TRASH {
        let rows = data.as_array().ok_or(INVALID)?;
        // A search row without an id has no box item to act on.
        let found = rows
            .iter()
            .filter_map(|row| posting_of(&row["id"]))
            .map(|posting| (posting, Found { kind, seen: None }))
            .collect();
        let next = if rows.is_empty() {
            String::new()
        } else {
            cursor
                .parse::<u64>()
                .ok()
                .and_then(|page| page.checked_add(1))
                .ok_or(INVALID)?
                .to_string()
        };
        return Ok((found, next));
    }
    if data["kind"] != kind {
        return Err(INVALID);
    }
    let rows = data["postings"].as_array().ok_or(INVALID)?;
    let found = rows
        .iter()
        .filter_map(|row| {
            let posting = posting_of(&row["id"])?;
            // A missing `seen` means unseen.
            let seen = Some(row["seen"] == true);
            Some((posting, Found { kind, seen }))
        })
        .collect();
    let next = match &data["next_page"] {
        Value::Null => String::new(),
        Value::String(next)
            if next.len() <= MAX_CURSOR
                && !next.starts_with('-')
                && !next.chars().any(char::is_control) =>
        {
            next.clone()
        }
        _ => return Err(INVALID),
    };
    Ok((found, next))
}

/// Where each posting is, looking through `kinds` in order and stopping once
/// every posting has been found. A posting no listing showed is
/// `undo_unsupported`, or the error of a listing that could not be read.
async fn scan(
    remote: &impl Remote,
    postings: &[&str],
    kinds: &[&'static str],
) -> HashMap<String, Result<Found, &'static str>> {
    let mut found: HashMap<String, Found> = HashMap::new();
    let mut failed: HashMap<&str, &'static str> = HashMap::new();
    let done = |found: &HashMap<String, Found>| postings.iter().all(|p| found.contains_key(*p));
    for kind in kinds {
        if done(&found) {
            break;
        }
        let mut cursor = if *kind == TRASH {
            "1".to_owned()
        } else {
            String::new()
        };
        for _ in 0..MAX_PAGES {
            let answer = remote.read(page_args(kind, &cursor)).await;
            match answer.and_then(|data| page(kind, &cursor, &data)) {
                Ok((rows, next)) => {
                    for (posting, place) in rows {
                        if postings.contains(&posting.as_str()) {
                            found.entry(posting).or_insert(place);
                        }
                    }
                    if done(&found) || next.is_empty() {
                        break;
                    }
                    cursor = next;
                }
                Err(code) => {
                    for posting in postings {
                        if !found.contains_key(*posting) {
                            failed.entry(posting).or_insert(code);
                        }
                    }
                    break;
                }
            }
        }
    }
    postings
        .iter()
        .map(|posting| {
            let answer = match found.get(*posting) {
                Some(place) => Ok(*place),
                None => Err(failed.get(posting).copied().unwrap_or("undo_unsupported")),
            };
            ((*posting).to_owned(), answer)
        })
        .collect()
}

fn posting(id: &str) -> Result<&str, &'static str> {
    super::hey_actions::message_id(id)
        .map(|(posting, _)| posting)
        .map_err(|_| "invalid_params")
}

async fn locate_with(remote: &impl Remote, ids: &[String]) -> Vec<Located> {
    let postings: Vec<&str> = ids.iter().filter_map(|id| posting(id).ok()).collect();
    let places = scan(remote, &postings, &PLACES).await;
    ids.iter()
        .map(|id| {
            let place = places.get(posting(id)?).ok_or("invalid_params")?;
            place.map(|found| json!({"box":found.kind}))
        })
        .collect()
}

/// One posting on its way: where it is, where it goes, and what happened.
struct Move<'a> {
    posting: &'a str,
    current: Option<Found>,
    wanted: &'static str,
    moved: bool,
    failed: Option<&'static str>,
}

/// One command for the listed moves, each posting named once between
/// `command` and `tail`.
async fn write_all(
    remote: &impl Remote,
    moves: &[Move<'_>],
    members: &[usize],
    command: &str,
    tail: &[&str],
) -> Result<(), &'static str> {
    let mut args = strings(&[command]);
    for index in members {
        let posting = moves[*index].posting;
        if !args[1..].iter().any(|named| named == posting) {
            args.push(posting.to_owned());
        }
    }
    args.extend(strings(tail));
    args.push("--json".into());
    remote.write(args).await.map(|_| ())
}

async fn place_with(remote: &impl Remote, items: &[PlaceItem]) -> Vec<PlaceOutcome> {
    let mut moves: Vec<Move> = items
        .iter()
        .map(|item| {
            let (posting, wanted) = match (posting(&item.id), place_box(&item.place)) {
                (Ok(posting), Ok(wanted)) => (posting, Ok(wanted)),
                (Err(code), _) | (_, Err(code)) => ("", Err(code)),
            };
            Move {
                posting,
                current: None,
                wanted: wanted.unwrap_or(IMBOX),
                moved: false,
                failed: wanted.err(),
            }
        })
        .collect();
    let postings: Vec<&str> = moves
        .iter()
        .filter(|entry| entry.failed.is_none())
        .map(|entry| entry.posting)
        .collect();
    let currents = scan(remote, &postings, &PLACES).await;
    let mut groups: BTreeMap<&'static str, Vec<usize>> = BTreeMap::new();
    for (index, entry) in moves.iter_mut().enumerate() {
        if entry.failed.is_some() {
            continue;
        }
        match currents.get(entry.posting) {
            Some(Ok(found)) => entry.current = Some(*found),
            Some(Err(code)) => entry.failed = Some(code),
            None => entry.failed = Some("undo_unsupported"),
        }
        let Some(current) = entry.current else {
            continue;
        };
        if current.kind == entry.wanted {
            continue;
        }
        // Bubble Up is a time, which `move` cannot set.
        if entry.wanted == BUBBLE {
            entry.failed = Some("undo_unsupported");
            continue;
        }
        groups.entry(entry.wanted).or_default().push(index);
    }
    for (wanted, members) in &groups {
        let answer = if *wanted == TRASH {
            write_all(remote, &moves, members, "trash", &[]).await
        } else {
            write_all(remote, &moves, members, "move", &["--to", wanted]).await
        };
        for index in members {
            match answer {
                Ok(()) => moves[*index].moved = true,
                Err(code) => moves[*index].failed = Some(code),
            }
        }
    }
    // Reply Later leaves out a thread that cannot be replied to and still
    // answers success: only a thread read back from it is there.
    if let Some(members) = groups.get(LATER) {
        let landed: Vec<&str> = members
            .iter()
            .filter(|index| moves[**index].moved)
            .map(|index| moves[*index].posting)
            .collect();
        if !landed.is_empty() {
            let after = scan(remote, &landed, &[LATER]).await;
            for index in members {
                let entry = &mut moves[*index];
                if entry.moved
                    && let Some(Err(code)) = after.get(entry.posting)
                {
                    entry.failed = Some(code);
                }
            }
        }
    }
    // HEY marks a thread seen when it lands in any box but the Imbox; the
    // Trash keeps it as it was. Read state is never part of a place.
    let unseen: Vec<usize> = moves
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            entry.failed.is_none()
                && entry.moved
                && entry.wanted != IMBOX
                && entry.wanted != TRASH
                && entry.current.is_some_and(|found| found.seen == Some(false))
        })
        .map(|(index, _)| index)
        .collect();
    if !unseen.is_empty()
        && write_all(remote, &moves, &unseen, "unseen", &[])
            .await
            .is_err()
    {
        for index in &unseen {
            moves[*index].failed = Some("hey_seen_not_restored");
        }
    }
    items
        .iter()
        .zip(moves)
        .map(|(item, entry)| match entry.failed {
            Some(code) => Err(code),
            None => Ok(Placed {
                new_id: item.id.clone(),
                moved: entry.moved,
            }),
        })
        .collect()
}

/// The official CLI, with the envelope checked the way every HEY read is:
/// a refusal can still exit 0, and diagnostics are never forwarded.
async fn run(program: &str, args: &[String], timeout: Duration) -> Result<Value, &'static str> {
    let output = crate::process::async_run::run(program, args, b"", timeout, LIMIT).await?;
    if !output.success {
        return Err("HEY refused the request");
    }
    let answer: Value =
        serde_json::from_slice(&output.stdout).map_err(|_| "HEY returned invalid JSON")?;
    if !answer.is_object() || answer["ok"] != true {
        return Err("HEY refused the request");
    }
    Ok(answer)
}

struct Live {
    program: String,
}

impl Remote for Live {
    fn read(&self, args: Vec<String>) -> Reply<'_> {
        Box::pin(async move { Ok(run(&self.program, &args, READ_TIMEOUT).await?["data"].take()) })
    }

    fn write(&self, args: Vec<String>) -> Reply<'_> {
        Box::pin(async move { run(&self.program, &args, WRITE_TIMEOUT).await })
    }
}

/// The executable must be the one on the path and signed in as this account
/// before any posting is read or moved.
async fn live(account: &Account) -> Result<Live, &'static str> {
    if account.provider != Provider::Hey {
        return Err("invalid_params");
    }
    let program = super::hey_access::program()?;
    super::hey_access::checked_params(&json!({"accountId":account.id,"program":program})).await?;
    Ok(Live { program })
}

/// Read-only, live from the server: one entry per id, in order. `ids` are
/// unique and already pass `crate::mail::action::validate_message_id`.
pub(crate) async fn locate(
    account: &Account,
    ids: &[String],
) -> Result<Vec<Located>, &'static str> {
    let live = live(account).await?;
    Ok(locate_with(&live, ids).await)
}

/// Moves each message to its place: one entry per item, in order. Places have
/// passed `validate_place`; a message already there is `Placed { new_id: id,
/// moved: false }` with no write.
pub(crate) async fn place(
    account: &Account,
    items: &[PlaceItem],
) -> Result<Vec<PlaceOutcome>, &'static str> {
    let live = live(account).await?;
    Ok(place_with(&live, items).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const PAGE: usize = 2;

    /// The HEY CLI in memory: each posting's box and seen state, two
    /// postings to a page, every command recorded. A move into any box but
    /// the Imbox marks a thread seen, as HEY does; `spam` is a box no command
    /// lists.
    struct Fake {
        postings: Mutex<BTreeMap<u64, (&'static str, bool)>>,
        reads: Mutex<Vec<Vec<String>>>,
        writes: Mutex<Vec<Vec<String>>>,
        unreplyable: Vec<u64>,
        refuse_write: Option<&'static str>,
        refuse_read: Option<&'static str>,
    }

    fn fake(postings: &[(u64, &'static str, bool)]) -> Fake {
        Fake {
            postings: Mutex::new(
                postings
                    .iter()
                    .map(|(id, kind, seen)| (*id, (*kind, *seen)))
                    .collect(),
            ),
            reads: Mutex::default(),
            writes: Mutex::default(),
            unreplyable: Vec::new(),
            refuse_write: None,
            refuse_read: None,
        }
    }

    impl Fake {
        fn now(&self, id: u64) -> (&'static str, bool) {
            self.postings.lock().unwrap()[&id]
        }
        fn reads(&self) -> Vec<Vec<String>> {
            self.reads.lock().unwrap().clone()
        }
        fn writes(&self) -> Vec<Vec<String>> {
            self.writes.lock().unwrap().clone()
        }
        fn listed(&self, kind: &str) -> Vec<(u64, bool)> {
            self.postings
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, (box_kind, _))| *box_kind == kind)
                .map(|(id, (_, seen))| (*id, *seen))
                .collect()
        }
    }

    impl Remote for Fake {
        fn read(&self, args: Vec<String>) -> Reply<'_> {
            Box::pin(async move {
                self.reads.lock().unwrap().push(args.clone());
                if let Some(code) = self.refuse_read {
                    return Err(code);
                }
                assert_eq!(args.iter().filter(|arg| *arg == "--json").count(), 1);
                if args[0] == "search" {
                    assert_eq!(&args[..5], ["search", "--in", "trash", "--json", "--page"]);
                    let page: usize = args[5].parse().unwrap();
                    let rows: Vec<Value> = self
                        .listed(TRASH)
                        .iter()
                        .skip((page - 1) * PAGE)
                        .take(PAGE)
                        .map(|(id, _)| json!({"id":id,"topic_id":id + 1000}))
                        .collect();
                    return Ok(json!(rows));
                }
                assert_eq!(args[0], "box");
                let kind = args[1].as_str();
                let page: usize = match args.get(3).map(String::as_str) {
                    None => 0,
                    Some("--page") => args[4].strip_prefix('p').unwrap().parse().unwrap(),
                    Some(other) => panic!("unexpected flag {other}"),
                };
                let listed = self.listed(kind);
                let rows: Vec<Value> = listed
                    .iter()
                    .skip(page * PAGE)
                    .take(PAGE)
                    .map(|(id, seen)| {
                        let url = format!("https://app.hey.com/topics/{}", id + 1000);
                        let mut row = json!({"id":id,"app_url":url});
                        if *seen {
                            row["seen"] = json!(true);
                        }
                        row
                    })
                    .collect();
                let next = if listed.len() > (page + 1) * PAGE {
                    json!(format!("p{}", page + 1))
                } else {
                    Value::Null
                };
                Ok(json!({"kind":kind,"postings":rows,"next_page":next}))
            })
        }

        fn write(&self, args: Vec<String>) -> Reply<'_> {
            Box::pin(async move {
                self.writes.lock().unwrap().push(args.clone());
                if let Some(code) = self.refuse_write {
                    return Err(code);
                }
                assert_eq!(args.last().unwrap(), "--json");
                let ids: Vec<u64> = args[1..]
                    .iter()
                    .take_while(|arg| arg.starts_with(|c: char| c.is_ascii_digit()))
                    .map(|arg| arg.parse().unwrap())
                    .collect();
                assert!(!ids.is_empty());
                let mut store = self.postings.lock().unwrap();
                for id in ids {
                    let entry = store.get_mut(&id).unwrap();
                    match args[0].as_str() {
                        "trash" => entry.0 = TRASH,
                        "unseen" => entry.1 = false,
                        "move" => {
                            let at = args.iter().position(|arg| arg == "--to").unwrap();
                            let to = PLACES
                                .into_iter()
                                .find(|kind| *kind == args[at + 1])
                                .unwrap();
                            assert_ne!(to, TRASH);
                            assert_ne!(to, BUBBLE);
                            if to == LATER && self.unreplyable.contains(&id) {
                                continue;
                            }
                            entry.0 = to;
                            if to != IMBOX {
                                entry.1 = true;
                            }
                        }
                        other => panic!("unexpected write {other}"),
                    }
                }
                Ok(json!({"ok":true}))
            })
        }
    }

    fn id(posting: u64) -> String {
        format!("{posting}:{}", posting + 1000)
    }

    fn item(posting: u64, kind: &str) -> PlaceItem {
        PlaceItem {
            id: id(posting),
            place: json!({"box":kind}),
        }
    }

    fn moved(posting: u64) -> PlaceOutcome {
        Ok(Placed {
            new_id: id(posting),
            moved: true,
        })
    }

    fn stayed(posting: u64) -> PlaceOutcome {
        Ok(Placed {
            new_id: id(posting),
            moved: false,
        })
    }

    #[tokio::test]
    async fn locate_answers_each_box_and_the_trash_without_writing() {
        let remote = fake(&[
            (1, IMBOX, true),
            (2, "feedbox", false),
            (3, "feedbox", true),
            (4, "feedbox", true),
            (5, "trailbox", true),
            (6, "asidebox", true),
            (7, LATER, false),
            (8, BUBBLE, true),
            (9, TRASH, true),
            (10, "spam", false),
        ]);
        let ids: Vec<String> = (1..=10).map(id).collect();
        let places = locate_with(&remote, &ids).await;
        let expected: Vec<Located> = [
            IMBOX, "feedbox", "feedbox", "feedbox", "trailbox", "asidebox", LATER, BUBBLE, TRASH,
        ]
        .iter()
        .map(|kind| Ok(json!({"box":kind})))
        .chain([Err("undo_unsupported")])
        .collect();
        assert_eq!(places, expected);
        for place in places.iter().flatten() {
            assert_eq!(validate_place(place), Ok(()));
        }
        assert!(remote.writes().is_empty());
        // The Feed's second page was followed with its cursor.
        assert!(
            remote
                .reads()
                .contains(&strings(&["box", "feedbox", "--json", "--page", "p1"]))
        );
    }

    #[tokio::test]
    async fn locate_stops_reading_once_every_posting_is_found() {
        let remote = fake(&[(1, IMBOX, true), (2, "feedbox", true), (3, TRASH, true)]);
        assert_eq!(
            locate_with(&remote, &[id(1)]).await,
            vec![Ok(json!({"box":IMBOX}))]
        );
        assert_eq!(remote.reads(), vec![strings(&["box", IMBOX, "--json"])]);
    }

    #[tokio::test]
    async fn an_unreadable_listing_fails_what_it_could_have_held() {
        let mut remote = fake(&[(1, IMBOX, true)]);
        remote.refuse_read = Some("HEY refused the request");
        assert_eq!(
            locate_with(&remote, &[id(1)]).await,
            vec![Err("HEY refused the request")]
        );
        assert_eq!(
            place_with(&remote, &[item(1, TRASH)]).await,
            vec![Err("HEY refused the request")]
        );
        assert!(remote.writes().is_empty());
    }

    #[test]
    fn listings_that_are_not_what_was_asked_are_refused() {
        let invalid = Err(INVALID);
        for data in [
            json!({"kind":"imbox","postings":[]}),
            json!({"kind":"feedbox"}),
            json!({"kind":"feedbox","postings":[],"next_page":"--help"}),
            json!({"kind":"feedbox","postings":[],"next_page":7}),
        ] {
            assert_eq!(page("feedbox", "", &data).map(|_| ()), invalid, "{data}");
        }
        assert_eq!(page(TRASH, "1", &json!({})).map(|_| ()), invalid);
        let (rows, next) = page(
            IMBOX,
            "",
            &json!({"kind":"imbox","postings":[{"id":"12"},{"id":"x"},{"id":13,"seen":true}],"next_page":"abc"}),
        )
        .unwrap();
        assert_eq!(
            rows.iter()
                .map(|(posting, found)| (posting.as_str(), found.seen))
                .collect::<Vec<_>>(),
            [("12", Some(false)), ("13", Some(true))]
        );
        assert_eq!(next, "abc");
        let (rows, next) = page(TRASH, "4", &json!([{"topic_id":1},{"id":9}])).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "9");
        assert_eq!(rows[0].1.seen, None);
        assert_eq!(next, "5");
        assert_eq!(page(TRASH, "4", &json!([])).unwrap().1, "");
    }

    #[test]
    fn invalid_places_are_refused() {
        for place in [
            json!([]),
            json!({}),
            json!({"box":"imbox","extra":true}),
            json!({"box":"Imbox"}),
            json!({"box":"spam"}),
            json!({"box":"screener"}),
            json!({"box":"feed"}),
            json!({"box":"123"}),
            json!({"box":""}),
            json!({"box":1}),
            json!({"box":["imbox"]}),
            json!({"labels":["imbox"]}),
        ] {
            assert_eq!(validate_place(&place), Err("invalid_params"), "{place}");
        }
    }

    #[tokio::test]
    async fn place_moves_into_every_box_move_can_target() {
        for kind in [IMBOX, "feedbox", "trailbox", "asidebox", LATER] {
            let remote = fake(&[(1, TRASH, true)]);
            assert_eq!(place_with(&remote, &[item(1, kind)]).await, vec![moved(1)]);
            assert_eq!(remote.now(1), (kind, true));
            assert_eq!(
                remote.writes(),
                vec![strings(&["move", "1", "--to", kind, "--json"])]
            );
        }
        let remote = fake(&[(1, "feedbox", true)]);
        assert_eq!(place_with(&remote, &[item(1, TRASH)]).await, vec![moved(1)]);
        assert_eq!(remote.writes(), vec![strings(&["trash", "1", "--json"])]);
        assert_eq!(remote.now(1), (TRASH, true));
    }

    #[tokio::test]
    async fn postings_going_to_one_box_share_one_command() {
        let remote = fake(&[
            (1, IMBOX, true),
            (2, "feedbox", true),
            (3, TRASH, true),
            (4, IMBOX, true),
        ]);
        let outcome = place_with(
            &remote,
            &[
                item(1, "trailbox"),
                item(2, "trailbox"),
                item(3, "trailbox"),
                item(4, TRASH),
            ],
        )
        .await;
        assert_eq!(outcome, [1, 2, 3, 4].map(moved));
        assert_eq!(
            remote.writes(),
            vec![
                strings(&["move", "1", "2", "3", "--to", "trailbox", "--json"]),
                strings(&["trash", "4", "--json"]),
            ]
        );
    }

    #[tokio::test]
    async fn a_posting_already_in_place_is_not_written() {
        let remote = fake(&[(1, "asidebox", false), (2, TRASH, true)]);
        let outcome = place_with(&remote, &[item(1, "asidebox"), item(2, TRASH)]).await;
        assert_eq!(outcome, vec![stayed(1), stayed(2)]);
        assert!(remote.writes().is_empty());
        assert_eq!(remote.now(1), ("asidebox", false));
    }

    #[tokio::test]
    async fn moves_the_cli_cannot_make_exactly_are_refused_per_posting() {
        let mut remote = fake(&[
            (1, IMBOX, true),
            (2, "spam", true),
            (3, IMBOX, true),
            (4, IMBOX, true),
            (5, BUBBLE, true),
        ]);
        remote.unreplyable = vec![3];
        let outcome = place_with(
            &remote,
            &[
                item(1, BUBBLE),
                item(2, IMBOX),
                item(3, LATER),
                item(4, "feedbox"),
                item(5, BUBBLE),
            ],
        )
        .await;
        assert_eq!(
            outcome,
            vec![
                Err("undo_unsupported"),
                Err("undo_unsupported"),
                Err("undo_unsupported"),
                moved(4),
                stayed(5),
            ]
        );
        assert_eq!(remote.now(1), (IMBOX, true));
        assert_eq!(remote.now(2), ("spam", true));
        assert_eq!(remote.now(3), (IMBOX, true));
        assert_eq!(remote.now(4), ("feedbox", true));
        let writes = remote.writes();
        assert!(
            writes
                .iter()
                .all(|args| !args.iter().any(|arg| arg == BUBBLE || arg == "2"))
        );
    }

    #[tokio::test]
    async fn refused_writes_fail_their_postings_only() {
        let mut remote = fake(&[(1, IMBOX, true), (2, "feedbox", true)]);
        remote.refuse_write = Some("HEY refused the request");
        let outcome = place_with(&remote, &[item(1, TRASH), item(2, "feedbox")]).await;
        assert_eq!(outcome, vec![Err("HEY refused the request"), stayed(2)]);
        assert_eq!(remote.writes().len(), 1);
    }

    #[tokio::test]
    async fn an_unseen_thread_stays_unseen_wherever_it_moves() {
        let remote = fake(&[(1, IMBOX, false), (2, IMBOX, true)]);
        let outcome = place_with(&remote, &[item(1, "feedbox"), item(2, "feedbox")]).await;
        assert_eq!(outcome, vec![moved(1), moved(2)]);
        assert_eq!(
            remote.writes(),
            vec![
                strings(&["move", "1", "2", "--to", "feedbox", "--json"]),
                strings(&["unseen", "1", "--json"]),
            ]
        );
        assert_eq!(remote.now(1), ("feedbox", false));
        assert_eq!(remote.now(2), ("feedbox", true));
        // Into the Imbox HEY keeps the seen state itself.
        assert_eq!(place_with(&remote, &[item(1, IMBOX)]).await, vec![moved(1)]);
        assert_eq!(remote.writes().len(), 3);
        assert_eq!(remote.now(1), (IMBOX, false));
    }

    #[tokio::test]
    async fn locate_and_place_cycles_are_exact_and_leave_read_state_alone() {
        let remote = fake(&[(1, LATER, false), (2, "trailbox", true), (3, IMBOX, true)]);
        let ids = vec![id(1), id(2)];
        let before = locate_with(&remote, &ids).await;
        for destination in [IMBOX, "asidebox", "feedbox", TRASH] {
            let away = [item(1, destination), item(2, destination)];
            assert_eq!(place_with(&remote, &away).await, vec![moved(1), moved(2)]);
            assert_eq!(
                locate_with(&remote, &ids).await,
                vec![Ok(json!({"box":destination})); 2]
            );
            let back: Vec<PlaceItem> = ids
                .iter()
                .zip(&before)
                .map(|(id, place)| PlaceItem {
                    id: id.clone(),
                    place: place.clone().unwrap(),
                })
                .collect();
            let outcome = place_with(&remote, &back).await;
            assert_eq!(outcome, vec![moved(1), moved(2)]);
            assert_eq!(locate_with(&remote, &ids).await, before);
            assert_eq!(remote.now(2), ("trailbox", true));
            if destination != TRASH {
                // Out of a box that answered its seen state, it is kept.
                assert_eq!(remote.now(1), (LATER, false));
            }
        }
        assert_eq!(remote.now(3), (IMBOX, true));
    }
}
