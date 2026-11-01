//! Undo history for JMAP. A place is
//! `{"mailboxIds": {<id>: true, ...}, "keywords": {"$junk": bool, "$notjunk": bool}}`:
//! every mailbox the email is in, and the two keywords that say whether it was
//! marked as junk. Read, flagged and every other keyword describe the message,
//! not where it is, and are never read into a place or written. Email ids are
//! stable, so a placed email keeps its id.
//!
//! Putting an email back reads its mailboxes and keywords live and writes only
//! what differs, in one `Email/set` update per email: `mailboxIds` replaced
//! wholesale, `keywords/$junk` and `keywords/$notjunk` patched. A recorded
//! mailbox the server no longer has is `undo_folder_gone`, checked with a live
//! `Mailbox/get` before anything is written. See `crate::mail::place` and
//! docs/BACKEND.md "Undo history" for the contract every provider keeps.
use super::mailbox::{Context, Snapshot, argument};
use super::query::string;
use super::*;
use crate::mail::Account;
use crate::mail::place::{Located, PlaceItem, PlaceOutcome, Placed};
use serde_json::Map;
use std::collections::{BTreeSet, HashSet};
use std::{future::Future, pin::Pin, sync::Arc};

const MAX_MAILBOXES: usize = 256;
/// RFC 8620 section 1.2: an Id is 1 to 255 octets of URL-safe base64 letters.
const MAX_ID: usize = 255;
const JUNK: &str = "$junk";
const NOT_JUNK: &str = "$notjunk";

fn jmap_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// Where an email is: its mailboxes and its two junk keywords.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Where {
    mailboxes: BTreeSet<String>,
    junk: bool,
    not_junk: bool,
}

fn mailbox_set(
    boxes: Option<&Map<String, Value>>,
    error: &'static str,
) -> Result<BTreeSet<String>, &'static str> {
    let boxes = boxes
        .filter(|boxes| !boxes.is_empty() && boxes.len() <= MAX_MAILBOXES)
        .ok_or(error)?;
    let mut mailboxes = BTreeSet::new();
    for (id, value) in boxes {
        if !jmap_id(id) || *value != Value::Bool(true) {
            return Err(error);
        }
        mailboxes.insert(id.clone());
    }
    Ok(mailboxes)
}

/// The place's mailboxes and keywords, refused unless exactly what `locate`
/// produces.
fn place_where(place: &Value) -> Result<Where, &'static str> {
    let object = place.as_object().ok_or("invalid_params")?;
    if object.len() != 2 {
        return Err("invalid_params");
    }
    let mailboxes = mailbox_set(
        object.get("mailboxIds").and_then(Value::as_object),
        "invalid_params",
    )?;
    let keywords = object
        .get("keywords")
        .and_then(Value::as_object)
        .filter(|keywords| keywords.len() == 2)
        .ok_or("invalid_params")?;
    let flag = |name: &str| {
        keywords
            .get(name)
            .and_then(Value::as_bool)
            .ok_or("invalid_params")
    };
    Ok(Where {
        mailboxes,
        junk: flag(JUNK)?,
        not_junk: flag(NOT_JUNK)?,
    })
}

/// The exact shape of a JMAP place; unknown keys and out-of-range values are
/// refused (`invalid_params` or an `undo_*` code), never guessed at.
pub(crate) fn validate_place(place: &Value) -> Result<(), &'static str> {
    place_where(place).map(|_| ())
}

fn mailbox_map(mailboxes: &BTreeSet<String>) -> Value {
    mailboxes
        .iter()
        .map(|id| (id.clone(), Value::Bool(true)))
        .collect::<Map<_, _>>()
        .into()
}

fn place_of(at: &Where) -> Value {
    json!({"mailboxIds":mailbox_map(&at.mailboxes),"keywords":{JUNK:at.junk,NOT_JUNK:at.not_junk}})
}

/// Where the server says an email is. Keywords are case-insensitive in JMAP.
fn location(email: &Value) -> Result<Where, &'static str> {
    let mailboxes = mailbox_set(email["mailboxIds"].as_object(), "jmap_invalid_response")?;
    let keywords = match &email["keywords"] {
        Value::Null => None,
        Value::Object(keywords) => Some(keywords),
        _ => return Err("jmap_invalid_response"),
    };
    let flag = |name: &str| {
        keywords.into_iter().flatten().any(|(keyword, value)| {
            keyword.eq_ignore_ascii_case(name) && *value == Value::Bool(true)
        })
    };
    Ok(Where {
        mailboxes,
        junk: flag(JUNK),
        not_junk: flag(NOT_JUNK),
    })
}

/// Each requested email's place, in request order, from an `Email/get` list.
/// A row the request did not ask for, or one answered twice, makes the whole
/// answer untrustworthy.
fn locations(ids: &[String], rows: &[Value]) -> Vec<Result<Where, &'static str>> {
    let mut found: HashMap<&str, &Value> = HashMap::with_capacity(rows.len());
    for row in rows {
        let id = string(&row["id"]);
        if !ids.iter().any(|wanted| wanted == id) || found.insert(id, row).is_some() {
            return vec![Err("jmap_invalid_response"); ids.len()];
        }
    }
    ids.iter()
        .map(|id| {
            found
                .get(id.as_str())
                .map_or(Err("jmap_message_not_found"), |row| location(row))
        })
        .collect()
}

/// The update that takes an email from `now` to `wanted`: only what differs.
fn change(now: &Where, wanted: &Where) -> Map<String, Value> {
    let mut patch = Map::new();
    if now.mailboxes != wanted.mailboxes {
        patch.insert("mailboxIds".into(), mailbox_map(&wanted.mailboxes));
    }
    for (name, was, wants) in [
        (JUNK, now.junk, wanted.junk),
        (NOT_JUNK, now.not_junk, wanted.not_junk),
    ] {
        if was != wants {
            patch.insert(
                format!("keywords/{name}"),
                if wants {
                    Value::Bool(true)
                } else {
                    Value::Null
                },
            );
        }
    }
    patch
}

fn refusal(error: &Value) -> &'static str {
    match string(&error["type"]) {
        "notFound" => "jmap_message_not_found",
        "forbidden" => "jmap_forbidden",
        _ => "jmap_update_failed",
    }
}

type Settled = Vec<Result<(), &'static str>>;

/// Each id's outcome from one `Email/set` answer. Every id must be answered
/// exactly once, as in the planned action path; anything else leaves the whole
/// chunk unknown.
fn settled(chunk: &[&str], result: &Value) -> Result<Settled, &'static str> {
    let updated = match &result["updated"] {
        Value::Null => None,
        Value::Object(map) => Some(map),
        _ => return Err("jmap_invalid_response"),
    };
    let failed = match &result["notUpdated"] {
        Value::Null => None,
        Value::Object(map) => Some(map),
        _ => return Err("jmap_invalid_response"),
    };
    let mut seen = HashSet::new();
    for id in updated.into_iter().chain(failed).flat_map(Map::keys) {
        if !chunk.contains(&id.as_str()) || !seen.insert(id.as_str()) {
            return Err("jmap_invalid_response");
        }
    }
    if seen.len() != chunk.len() {
        return Err("jmap_invalid_response");
    }
    Ok(chunk
        .iter()
        .map(|id| match failed.and_then(|failed| failed.get(*id)) {
            Some(error) => Err(refusal(error)),
            None => Ok(()),
        })
        .collect())
}

type Reply<'a, T> = Pin<Box<dyn Future<Output = Result<T, &'static str>> + Send + 'a>>;

/// The account-bound JMAP calls a place needs; tests answer them in memory.
trait Remote: Send + Sync {
    /// `Email/get` rows (`id`, `mailboxIds`, `keywords`) for these ids.
    fn emails<'a>(&'a self, ids: &'a [String]) -> Reply<'a, Vec<Value>>;
    /// Which of these mailbox ids the server has now.
    fn mailboxes<'a>(&'a self, ids: &'a [String]) -> Reply<'a, HashSet<String>>;
    /// One `Email/set` update; answers that method's arguments.
    fn update<'a>(&'a self, update: Map<String, Value>) -> Reply<'a, Value>;
    /// The server's `maxObjectsInSet`.
    fn batch(&self) -> usize;
}

async fn read(remote: &impl Remote, ids: &[String]) -> Vec<Result<Where, &'static str>> {
    match remote.emails(ids).await {
        Ok(rows) => locations(ids, &rows),
        Err(code) => vec![Err(code); ids.len()],
    }
}

async fn locate_with(remote: &impl Remote, ids: &[String]) -> Vec<Located> {
    read(remote, ids)
        .await
        .into_iter()
        .map(|at| at.map(|at| place_of(&at)))
        .collect()
}

/// One email on its way: its index among the items, where it is, where it goes.
struct Move {
    index: usize,
    now: Where,
    wanted: Where,
}

async fn place_with(remote: &impl Remote, items: &[PlaceItem]) -> Vec<PlaceOutcome> {
    let ids: Vec<String> = items.iter().map(|item| item.id.clone()).collect();
    let currents = read(remote, &ids).await;
    let mut outcomes: Vec<Option<PlaceOutcome>> = vec![None; items.len()];
    let mut moves: Vec<Move> = Vec::new();
    for (index, (item, current)) in items.iter().zip(currents).enumerate() {
        match (current, place_where(&item.place)) {
            (Err(code), _) | (_, Err(code)) => outcomes[index] = Some(Err(code)),
            (Ok(now), Ok(wanted)) if now == wanted => {
                outcomes[index] = Some(Ok(Placed {
                    new_id: item.id.clone(),
                    moved: false,
                }));
            }
            (Ok(now), Ok(wanted)) => moves.push(Move { index, now, wanted }),
        }
    }
    // A mailbox the email is in now exists; every other one it goes back to
    // is asked about before anything moves.
    let named: BTreeSet<&String> = moves
        .iter()
        .flat_map(|entry| entry.wanted.mailboxes.difference(&entry.now.mailboxes))
        .collect();
    if !named.is_empty() {
        let named: Vec<String> = named.into_iter().cloned().collect();
        let existing = remote.mailboxes(&named).await;
        moves.retain(|entry| {
            let failed = match &existing {
                Ok(existing) => entry
                    .wanted
                    .mailboxes
                    .difference(&entry.now.mailboxes)
                    .any(|id| !existing.contains(id))
                    .then_some("undo_folder_gone"),
                Err(code) => Some(*code),
            };
            if let Some(code) = failed {
                outcomes[entry.index] = Some(Err(code));
            }
            failed.is_none()
        });
    }
    let mut stopped = None;
    for chunk in moves.chunks(remote.batch().max(1)) {
        let chunk_ids: Vec<&str> = chunk
            .iter()
            .map(|entry| items[entry.index].id.as_str())
            .collect();
        let answers = match stopped {
            Some(code) => Err(code),
            None => {
                let update: Map<String, Value> = chunk
                    .iter()
                    .map(|entry| {
                        (
                            items[entry.index].id.clone(),
                            change(&entry.now, &entry.wanted).into(),
                        )
                    })
                    .collect();
                match remote.update(update).await {
                    Ok(result) => settled(&chunk_ids, &result),
                    Err(code) => Err(code),
                }
            }
        };
        match answers {
            Ok(answers) => {
                for (entry, answer) in chunk.iter().zip(answers) {
                    outcomes[entry.index] = Some(answer.map(|()| Placed {
                        new_id: items[entry.index].id.clone(),
                        moved: true,
                    }));
                }
            }
            // Unknown delivery: stop, and report this chunk and every unsent
            // one as failed, as the planned action path does.
            Err(code) => {
                stopped = Some(code);
                for entry in chunk {
                    outcomes[entry.index] = Some(Err(code));
                }
            }
        }
    }
    outcomes
        .into_iter()
        .map(|outcome| outcome.unwrap_or(Err("jmap_update_failed")))
        .collect()
}

struct Live<'a> {
    session: &'a Session,
    context: Arc<Context>,
    snapshot: Snapshot,
}

impl Live<'_> {
    /// A refused credential is remembered for the account, as `native` does.
    fn checked<T>(&self, result: Result<T, &'static str>) -> Result<T, &'static str> {
        if matches!(result, Err("jmap_unauthorized")) {
            self.context
                .rejected
                .store(true, std::sync::atomic::Ordering::Release);
        }
        result
    }
}

impl Remote for Live<'_> {
    fn emails<'a>(&'a self, ids: &'a [String]) -> Reply<'a, Vec<Value>> {
        Box::pin(async move {
            let rows = self
                .session
                .get_emails(&self.context, &self.snapshot, ids, false, true)
                .await;
            self.checked(rows)
        })
    }

    fn mailboxes<'a>(&'a self, ids: &'a [String]) -> Reply<'a, HashSet<String>> {
        Box::pin(async move {
            let mut existing = HashSet::new();
            for chunk in ids.chunks(self.snapshot.limit("maxObjectsInGet", 256)) {
                let reply = self.checked(
                    self.session
                        .api(
                            &self.context,
                            &self.snapshot,
                            json!([["Mailbox/get",{"accountId":self.snapshot.account,"ids":chunk,"properties":["id"]},"0"]]),
                            false,
                        )
                        .await,
                )?;
                let list = argument(&reply, "0", "Mailbox/get")?["list"]
                    .as_array()
                    .ok_or("jmap_invalid_response")?;
                existing.extend(list.iter().map(|row| string(&row["id"]).to_owned()));
            }
            Ok(existing)
        })
    }

    fn update<'a>(&'a self, update: Map<String, Value>) -> Reply<'a, Value> {
        Box::pin(async move {
            let reply = self.checked(
                self.session
                    .api(
                        &self.context,
                        &self.snapshot,
                        json!([["Email/set",{"accountId":self.snapshot.account,"update":update},"0"]]),
                        false,
                    )
                    .await,
            )?;
            let result = argument(&reply, "0", "Email/set")?.clone();
            // Whatever was written makes the retained rows stale, as after
            // a planned action.
            if result["updated"]
                .as_object()
                .is_some_and(|map| !map.is_empty())
            {
                if let Ok(mut summaries) = self.context.summaries.lock() {
                    summaries.clear();
                }
                if let Ok(mut blocks) = self.context.blocks.lock() {
                    blocks.clear();
                }
            }
            Ok(result)
        })
    }

    fn batch(&self) -> usize {
        self.snapshot.limit("maxObjectsInSet", 128)
    }
}

impl super::Session {
    async fn live(&self, account: &Account) -> Result<Live<'_>, &'static str> {
        let context = self.context(&account.id)?;
        if context.rejected.load(std::sync::atomic::Ordering::Acquire) {
            return Err("jmap_unauthorized");
        }
        let snapshot = self.snapshot(&account.id, &context).await;
        if matches!(snapshot, Err("jmap_unauthorized")) {
            context
                .rejected
                .store(true, std::sync::atomic::Ordering::Release);
        }
        Ok(Live {
            session: self,
            snapshot: snapshot?,
            context,
        })
    }

    /// Read-only, live from the server: one entry per id, in order. `ids` are
    /// unique and already pass `crate::mail::action::validate_message_id`.
    pub(crate) async fn locate(
        &self,
        account: &Account,
        ids: &[String],
    ) -> Result<Vec<Located>, &'static str> {
        let live = self.live(account).await?;
        Ok(locate_with(&live, ids).await)
    }

    /// Moves each message to its place: one entry per item, in order. Places
    /// have passed `validate_place`; a message already there is
    /// `Placed { new_id: id, moved: false }` with no write.
    pub(crate) async fn place(
        &self,
        account: &Account,
        items: &[PlaceItem],
    ) -> Result<Vec<PlaceOutcome>, &'static str> {
        let live = self.live(account).await?;
        Ok(place_with(&live, items).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Store = HashMap<String, (BTreeSet<String>, BTreeSet<String>)>;

    /// A JMAP account in memory: mailboxes and keywords per email, every
    /// `Email/set` update recorded. Any patch key other than `mailboxIds` and
    /// the two junk keywords fails the test.
    struct Fake {
        emails: Mutex<Store>,
        boxes: HashSet<String>,
        updates: Mutex<Vec<Value>>,
        mailbox_reads: Mutex<Vec<Vec<String>>>,
        refuse: HashMap<String, &'static str>,
        batch: usize,
    }

    fn strings(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn fake(boxes: &[&str], emails: &[(&str, &[&str], &[&str])]) -> Fake {
        Fake {
            emails: Mutex::new(
                emails
                    .iter()
                    .map(|(id, boxes, keywords)| {
                        ((*id).to_owned(), (strings(boxes), strings(keywords)))
                    })
                    .collect(),
            ),
            boxes: boxes.iter().map(|id| (*id).to_owned()).collect(),
            updates: Mutex::default(),
            mailbox_reads: Mutex::default(),
            refuse: HashMap::new(),
            batch: 128,
        }
    }

    impl Fake {
        fn now(&self, id: &str) -> (Vec<String>, Vec<String>) {
            let (boxes, keywords) = self.emails.lock().unwrap()[id].clone();
            (boxes.into_iter().collect(), keywords.into_iter().collect())
        }
        fn updates(&self) -> Vec<Value> {
            self.updates.lock().unwrap().clone()
        }
        fn mailbox_reads(&self) -> Vec<Vec<String>> {
            self.mailbox_reads.lock().unwrap().clone()
        }
    }

    fn flags(values: &BTreeSet<String>) -> Value {
        values
            .iter()
            .map(|value| (value.clone(), Value::Bool(true)))
            .collect::<Map<_, _>>()
            .into()
    }

    fn apply(boxes: &mut BTreeSet<String>, keywords: &mut BTreeSet<String>, patch: &Value) {
        for (key, value) in patch.as_object().unwrap() {
            match key.as_str() {
                "mailboxIds" => *boxes = value.as_object().unwrap().keys().cloned().collect(),
                "keywords/$junk" | "keywords/$notjunk" => {
                    let keyword = key.trim_start_matches("keywords/").to_owned();
                    match value {
                        Value::Bool(true) => keywords.insert(keyword),
                        Value::Null => keywords.remove(&keyword),
                        _ => panic!("bad keyword value {value}"),
                    };
                }
                _ => panic!("unexpected patch key {key}"),
            }
        }
    }

    impl Remote for Fake {
        fn emails<'a>(&'a self, ids: &'a [String]) -> Reply<'a, Vec<Value>> {
            Box::pin(async move {
                let store = self.emails.lock().unwrap();
                Ok(ids
                    .iter()
                    .filter_map(|id| {
                        store.get(id).map(|(boxes, keywords)| {
                            json!({"id":id,"threadId":"t","mailboxIds":flags(boxes),"keywords":flags(keywords)})
                        })
                    })
                    .collect())
            })
        }

        fn mailboxes<'a>(&'a self, ids: &'a [String]) -> Reply<'a, HashSet<String>> {
            Box::pin(async move {
                self.mailbox_reads.lock().unwrap().push(ids.to_vec());
                Ok(ids
                    .iter()
                    .filter(|id| self.boxes.contains(*id))
                    .cloned()
                    .collect())
            })
        }

        fn update<'a>(&'a self, update: Map<String, Value>) -> Reply<'a, Value> {
            Box::pin(async move {
                self.updates
                    .lock()
                    .unwrap()
                    .push(Value::Object(update.clone()));
                let mut store = self.emails.lock().unwrap();
                let mut updated = Map::new();
                let mut failed = Map::new();
                for (id, patch) in update {
                    if let Some(kind) = self.refuse.get(&id) {
                        failed.insert(id, json!({"type":kind}));
                    } else if let Some((boxes, keywords)) = store.get_mut(&id) {
                        apply(boxes, keywords, &patch);
                        updated.insert(id, Value::Null);
                    } else {
                        failed.insert(id, json!({"type":"notFound"}));
                    }
                }
                Ok(json!({"updated":updated,"notUpdated":failed}))
            })
        }

        fn batch(&self) -> usize {
            self.batch
        }
    }

    fn place(boxes: &[&str], junk: bool, not_junk: bool) -> Value {
        json!({"mailboxIds":flags(&strings(boxes)),"keywords":{"$junk":junk,"$notjunk":not_junk}})
    }

    fn item(id: &str, place: Value) -> PlaceItem {
        PlaceItem {
            id: id.into(),
            place,
        }
    }

    fn moved(id: &str) -> PlaceOutcome {
        Ok(Placed {
            new_id: id.into(),
            moved: true,
        })
    }

    fn stayed(id: &str) -> PlaceOutcome {
        Ok(Placed {
            new_id: id.into(),
            moved: false,
        })
    }

    #[tokio::test]
    async fn locate_answers_mailboxes_and_only_the_junk_keywords() {
        let remote = fake(
            &["I", "L", "J"],
            &[
                ("e1", &["I", "L"], &["$seen", "$flagged", "$notjunk"]),
                ("e2", &["J"], &["$junk", "$answered"]),
            ],
        );
        let ids = ["e1", "missing", "e2"].map(String::from);
        let located = locate_with(&remote, &ids).await;
        assert_eq!(
            located,
            vec![
                Ok(place(&["I", "L"], false, true)),
                Err("jmap_message_not_found"),
                Ok(place(&["J"], true, false)),
            ]
        );
        for place in located.into_iter().flatten() {
            assert_eq!(validate_place(&place), Ok(()));
        }
        assert!(remote.updates().is_empty());
        assert!(remote.mailbox_reads().is_empty());
    }

    #[test]
    fn locate_refuses_untrustworthy_rows() {
        let ids = ["e1".to_owned(), "e2".to_owned()];
        let row = |id: &str, boxes: Value| json!({"id":id,"mailboxIds":boxes,"keywords":{}});
        let good = row("e1", json!({"I":true}));
        assert_eq!(
            locations(&ids, &[good.clone(), row("e9", json!({"I":true}))]),
            vec![Err("jmap_invalid_response"); 2]
        );
        assert_eq!(
            locations(&ids, &[good.clone(), good.clone()]),
            vec![Err("jmap_invalid_response"); 2]
        );
        assert_eq!(
            locations(&ids, &[good, row("e2", json!({"I":"true"}))])[1],
            Err("jmap_invalid_response")
        );
        assert_eq!(
            locations(&ids, &[row("e2", json!({}))])[1],
            Err("jmap_invalid_response")
        );
        let upper = json!({"id":"e1","mailboxIds":{"J":true},"keywords":{"$Junk":true}});
        assert!(locations(&ids[..1], &[upper])[0].as_ref().unwrap().junk);
    }

    #[test]
    fn invalid_places_are_refused() {
        let off = json!({"$junk":false,"$notjunk":false});
        for bad in [
            json!([]),
            json!({}),
            json!({"mailboxIds":{"I":true}}),
            json!({"mailboxIds":{"I":true},"keywords":off,"extra":1}),
            json!({"mailboxIds":{},"keywords":off}),
            json!({"mailboxIds":["I"],"keywords":off}),
            json!({"mailboxIds":{"I":false},"keywords":off}),
            json!({"mailboxIds":{"I":"true"},"keywords":off}),
            json!({"mailboxIds":{"a/b":true},"keywords":off}),
            json!({"mailboxIds":{"":true},"keywords":off}),
            json!({"mailboxIds":{"x".repeat(MAX_ID + 1):true},"keywords":off}),
            json!({"mailboxIds":(0..=MAX_MAILBOXES).map(|n| (format!("m{n}"), json!(true))).collect::<Map<_,_>>(),"keywords":off}),
            json!({"mailboxIds":{"I":true},"keywords":{"$junk":false}}),
            json!({"mailboxIds":{"I":true},"keywords":{"$junk":false,"$notjunk":null}}),
            json!({"mailboxIds":{"I":true},"keywords":{"$junk":false,"$notjunk":false,"$seen":true}}),
            json!({"mailboxIds":{"I":true},"keywords":{"$junk":false,"$seen":false}}),
            json!({"mailboxIds":{"I":true},"keywords":[]}),
        ] {
            assert_eq!(validate_place(&bad), Err("invalid_params"), "{bad}");
        }
        assert_eq!(
            validate_place(&place(&["I", "Mb-1_x"], true, false)),
            Ok(())
        );
    }

    #[tokio::test]
    async fn place_restores_a_multi_mailbox_set_wholesale() {
        let remote = fake(&["I", "A", "L1", "L2"], &[("e1", &["A"], &["$seen"])]);
        let outcome = place_with(
            &remote,
            &[item("e1", place(&["I", "L1", "L2"], false, false))],
        )
        .await;
        assert_eq!(outcome, vec![moved("e1")]);
        assert_eq!(
            remote.updates(),
            vec![json!({"e1":{"mailboxIds":{"I":true,"L1":true,"L2":true}}})]
        );
        assert_eq!(
            remote.now("e1"),
            (
                vec!["I".to_owned(), "L1".into(), "L2".into()],
                vec!["$seen".to_owned()]
            )
        );
        assert_eq!(
            remote.mailbox_reads(),
            vec![vec!["I".to_owned(), "L1".into(), "L2".into()]]
        );
    }

    #[tokio::test]
    async fn junk_keywords_are_restored_both_ways() {
        let remote = fake(
            &["I", "J"],
            &[
                ("spammed", &["J"], &["$junk", "$flagged"]),
                ("unspammed", &["I"], &["$notjunk"]),
            ],
        );
        let outcome = place_with(
            &remote,
            &[
                item("spammed", place(&["I"], false, true)),
                item("unspammed", place(&["J"], true, false)),
            ],
        )
        .await;
        assert_eq!(outcome, vec![moved("spammed"), moved("unspammed")]);
        assert_eq!(
            remote.updates(),
            vec![json!({
                "spammed":{"mailboxIds":{"I":true},"keywords/$junk":null,"keywords/$notjunk":true},
                "unspammed":{"mailboxIds":{"J":true},"keywords/$junk":true,"keywords/$notjunk":null},
            })]
        );
        assert_eq!(remote.now("spammed").1, ["$flagged", "$notjunk"]);
        assert_eq!(remote.now("unspammed").1, ["$junk"]);
    }

    #[tokio::test]
    async fn an_email_already_in_place_is_not_written() {
        let remote = fake(&["I"], &[("e1", &["I"], &["$seen", "$junk"])]);
        let outcome = place_with(&remote, &[item("e1", place(&["I"], true, false))]).await;
        assert_eq!(outcome, vec![stayed("e1")]);
        assert!(remote.updates().is_empty());
        assert!(remote.mailbox_reads().is_empty());
    }

    #[tokio::test]
    async fn a_mailbox_that_is_gone_fails_that_email_only() {
        let remote = fake(&["I", "A"], &[("e1", &["A"], &[]), ("e2", &["A"], &[])]);
        let outcome = place_with(
            &remote,
            &[
                item("e1", place(&["I", "Deleted"], false, false)),
                item("e2", place(&["I"], false, false)),
            ],
        )
        .await;
        assert_eq!(outcome, vec![Err("undo_folder_gone"), moved("e2")]);
        assert_eq!(
            remote.updates(),
            vec![json!({"e2":{"mailboxIds":{"I":true}}})]
        );
        assert_eq!(remote.now("e1").0, ["A"]);
    }

    #[tokio::test]
    async fn not_updated_is_reported_per_email() {
        let mut remote = fake(
            &["I", "A"],
            &[
                ("e1", &["A"], &[]),
                ("e2", &["A"], &[]),
                ("e3", &["A"], &[]),
            ],
        );
        remote.refuse.insert("e2".into(), "forbidden");
        remote.refuse.insert("e3".into(), "invalidProperties");
        let outcome = place_with(
            &remote,
            &[
                item("e1", place(&["I"], false, false)),
                item("e2", place(&["I"], false, false)),
                item("e3", place(&["I"], false, false)),
                item("gone", place(&["I"], false, false)),
            ],
        )
        .await;
        assert_eq!(
            outcome,
            vec![
                moved("e1"),
                Err("jmap_forbidden"),
                Err("jmap_update_failed"),
                Err("jmap_message_not_found"),
            ]
        );
        assert_eq!(remote.updates()[0].as_object().unwrap().len(), 3);
    }

    #[test]
    fn email_set_answers_must_cover_each_id_once() {
        let chunk = ["e1", "e2"];
        assert_eq!(
            settled(
                &chunk,
                &json!({"updated":{"e1":null},"notUpdated":{"e2":{"type":"notFound"}}})
            ),
            Ok(vec![Ok(()), Err("jmap_message_not_found")])
        );
        for bad in [
            json!({"updated":{"e1":null}}),
            json!({"updated":["e1","e2"]}),
            json!({"updated":{"e1":null,"e2":null,"e3":null}}),
            json!({"updated":{"e1":null,"e2":null},"notUpdated":{"e1":{"type":"forbidden"}}}),
        ] {
            assert_eq!(settled(&chunk, &bad), Err("jmap_invalid_response"), "{bad}");
        }
    }

    #[tokio::test]
    async fn writes_follow_max_objects_in_set() {
        let mut remote = fake(
            &["I", "A"],
            &[
                ("e1", &["A"], &[]),
                ("e2", &["A"], &[]),
                ("e3", &["A"], &[]),
            ],
        );
        remote.batch = 2;
        let items: Vec<_> = ["e1", "e2", "e3"]
            .iter()
            .map(|id| item(id, place(&["I"], false, false)))
            .collect();
        assert_eq!(
            place_with(&remote, &items).await,
            ["e1", "e2", "e3"].map(moved)
        );
        let updates = remote.updates();
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].as_object().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn locate_and_place_cycles_are_exact_and_leave_other_keywords_alone() {
        let remote = fake(
            &["I", "L", "A", "T", "J"],
            &[("e1", &["I", "L"], &["$seen", "$flagged", "$answered"])],
        );
        let ids = ["e1".to_owned()];
        let before = locate_with(&remote, &ids).await[0].clone().unwrap();
        for _ in 0..3 {
            for destination in [
                place(&["A"], false, false),
                place(&["T"], false, false),
                place(&["J"], true, false),
            ] {
                let undo = locate_with(&remote, &ids).await[0].clone().unwrap();
                let to = |place: &Value| [item("e1", place.clone())];
                assert_eq!(
                    place_with(&remote, &to(&destination)).await,
                    vec![moved("e1")]
                );
                assert_eq!(locate_with(&remote, &ids).await[0], Ok(destination.clone()));
                // Undo, redo and undo again: each exact.
                assert_eq!(place_with(&remote, &to(&undo)).await, vec![moved("e1")]);
                assert_eq!(locate_with(&remote, &ids).await[0], Ok(undo.clone()));
                assert_eq!(
                    place_with(&remote, &to(&destination)).await,
                    vec![moved("e1")]
                );
                assert_eq!(place_with(&remote, &to(&undo)).await, vec![moved("e1")]);
                assert_eq!(place_with(&remote, &to(&undo)).await, vec![stayed("e1")]);
            }
        }
        assert_eq!(locate_with(&remote, &ids).await[0], Ok(before));
        assert_eq!(remote.now("e1").1, ["$answered", "$flagged", "$seen"]);
        // The fake panics on any other key; check every written patch too.
        for update in remote.updates() {
            for patch in update.as_object().unwrap().values() {
                for key in patch.as_object().unwrap().keys() {
                    assert!(
                        matches!(
                            key.as_str(),
                            "mailboxIds" | "keywords/$junk" | "keywords/$notjunk"
                        ),
                        "{key}"
                    );
                }
            }
        }
    }

    /// The live calls against the synthetic JMAP peer: `Email/get`,
    /// `Mailbox/get` and `Email/set` through the account's session.
    #[tokio::test]
    async fn live_session_locates_and_places_over_the_wire() {
        use std::io::{BufRead, BufReader};
        struct Peer(std::process::Child);
        impl Drop for Peer {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut peer = Peer(
            std::process::Command::new("python3")
                .arg(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/src/providers/jmap/mailbox_tls_test.py"
                ))
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut output = BufReader::new(peer.0.stdout.take().unwrap());
        let mut port = String::new();
        output.read_line(&mut port).unwrap();
        let port: u16 = port.trim().parse().unwrap();
        let mut certificate = String::new();
        output.read_line(&mut certificate).unwrap();
        let pem = std::fs::read(certificate.trim()).unwrap();
        let session = Session::with_test_certificate(&pem).unwrap();
        let boxes = ["I", "S", "T", "A", "D"]
            .iter()
            .zip(["inbox", "sent", "trash", "archive", "drafts"])
            .map(|(id, role)| json!({"id":id,"role":role}))
            .collect();
        session
            .install_snapshot_for_test(
                "jmap:user@example.test",
                json!({
                    "apiUrl":format!("https://localhost:{port}/api"),
                    "downloadUrl":format!("https://localhost:{port}/blob/{{blobId}}"),
                    "uploadUrl":format!("https://localhost:{port}/upload"),
                    "state":"s1",
                    "capabilities":{
                        "urn:ietf:params:jmap:core":{"maxObjectsInGet":2,"maxObjectsInSet":2},
                        "urn:ietf:params:jmap:mail":{},
                    },
                    "accounts":{"account":{"accountCapabilities":{"urn:ietf:params:jmap:mail":{"emailQuerySortOptions":["receivedAt"]}}}},
                    "primaryAccounts":{"urn:ietf:params:jmap:mail":"account"},
                }),
                boxes,
                json!({"scheme":"basic","username":"user","secret":"synthetic"}),
                "user@example.test",
            )
            .await
            .unwrap();
        let account = Account {
            id: "jmap:user@example.test".into(),
            provider: crate::mail::Provider::Jmap,
        };
        // The peer keeps e1 in the Inbox (read), e2 in Sent (flagged), e3 in
        // the Trash.
        assert_eq!(
            session
                .locate(&account, &["e1".into(), "e2".into(), "e3".into()])
                .await
                .unwrap(),
            vec![
                Ok(place(&["I"], false, false)),
                Ok(place(&["S"], false, false)),
                Ok(place(&["T"], false, false)),
            ]
        );
        let outcome = session
            .place(
                &account,
                &[
                    item("e1", place(&["I"], false, false)),
                    item("e2", place(&["A"], true, false)),
                    item("e3", place(&["X"], false, false)),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            outcome,
            vec![stayed("e1"), moved("e2"), Err("undo_folder_gone")]
        );
        let client = client_builder()
            .add_root_certificate(reqwest::Certificate::from_pem(&pem).unwrap())
            .build()
            .unwrap();
        let report: Value = serde_json::from_slice(
            &client
                .get(format!("https://localhost:{port}/report"))
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap(),
        )
        .unwrap();
        let calls: Vec<&Value> = report
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|request| request["calls"].as_array().into_iter().flatten())
            .collect();
        let named = |method: &str| -> Vec<&Value> {
            calls
                .iter()
                .copied()
                .filter(|call| call[0] == method)
                .collect()
        };
        assert!(named("Email/get").iter().all(
            |call| call[1]["properties"] == json!(["id", "threadId", "mailboxIds", "keywords"])
        ));
        let boxes = named("Mailbox/get");
        assert_eq!(boxes.len(), 1);
        assert_eq!(boxes[0][1]["ids"], json!(["A", "X"]));
        let sets = named("Email/set");
        assert_eq!(sets.len(), 1);
        assert_eq!(
            sets[0][1]["update"],
            json!({"e2":{"mailboxIds":{"A":true},"keywords/$junk":true}})
        );
        assert!(
            report
                .as_array()
                .unwrap()
                .iter()
                .all(|request| request["authorization"] == true)
        );
        assert!(peer.0.wait().unwrap().success());
    }
}
