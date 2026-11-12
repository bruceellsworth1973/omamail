//! Undo history for Gmail. A place is `{"labels": [...]}`: the message's label
//! ids, sorted and unique, without the labels that describe the message rather
//! than where it is (read, starred, importance, drafts, sent, chat, inbox
//! categories). Ids are stable, so a placed message keeps its id.
//!
//! Putting a message back reads its labels live and writes only the difference:
//! TRASH through the trash and untrash endpoints, never through modify, and
//! every other label through `batchModify`, one call for each equal change.
//! Writes go through the account's mutation queue like every other action.
use super::*;
use crate::mail::Account;
use crate::mail::place::{Located, PlaceItem, PlaceOutcome, Placed};
use futures_util::{StreamExt, stream};
use std::collections::{BTreeMap, BTreeSet};
use std::pin::Pin;

const MAX_LABELS: usize = 100;
const MAX_LABEL_ID: usize = 256;
const MAX_BATCH: usize = 1_000;
/// Reads in flight at once; the account's quota pacing still sets the rate.
const READS: usize = 8;
const TRASH: &str = "TRASH";

fn volatile(label: &str) -> bool {
    matches!(
        label,
        "UNREAD" | "STARRED" | "IMPORTANT" | "DRAFT" | "SENT" | "CHAT"
    ) || label.starts_with("CATEGORY_")
}

fn label_id(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= MAX_LABEL_ID
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// The labels of a place, refused unless exactly what `locate` produces.
fn place_labels(place: &Value) -> Result<Vec<String>, &'static str> {
    let object = place.as_object().ok_or("invalid_params")?;
    if object.len() != 1 {
        return Err("invalid_params");
    }
    let values = object
        .get("labels")
        .and_then(Value::as_array)
        .filter(|values| values.len() <= MAX_LABELS)
        .ok_or("invalid_params")?;
    let mut labels: Vec<String> = Vec::with_capacity(values.len());
    for value in values {
        let label = value
            .as_str()
            .filter(|label| label_id(label) && !volatile(label))
            .ok_or("invalid_params")?;
        // Strictly ascending: sorted and without repeats.
        if labels.last().is_some_and(|last| last.as_str() >= label) {
            return Err("invalid_params");
        }
        labels.push(label.to_owned());
    }
    Ok(labels)
}

pub(crate) fn validate_place(place: &Value) -> Result<(), &'static str> {
    place_labels(place).map(|_| ())
}

/// Where a message is, from the `labelIds` Gmail answered for it. A message
/// with no labels at all is archived: its place is the empty set.
fn location(label_ids: &Value) -> Result<Vec<String>, &'static str> {
    let values = match label_ids {
        Value::Null => return Ok(Vec::new()),
        Value::Array(values) => values,
        _ => return Err("gmail_invalid_response"),
    };
    let mut labels = BTreeSet::new();
    for value in values {
        let label = value
            .as_str()
            .filter(|label| label_id(label))
            .ok_or("gmail_invalid_response")?;
        if !volatile(label) {
            labels.insert(label.to_owned());
        }
    }
    if labels.len() > MAX_LABELS {
        return Err("gmail_invalid_response");
    }
    Ok(labels.into_iter().collect())
}

fn place_of(labels: &[String]) -> Value {
    json!({"labels":labels})
}

/// The modify half of a move: what to add and remove, TRASH excluded.
fn labels_change(current: &[String], wanted: &[String]) -> (Vec<String>, Vec<String>) {
    let add = wanted
        .iter()
        .filter(|label| *label != TRASH && !current.contains(label))
        .cloned()
        .collect();
    let remove = current
        .iter()
        .filter(|label| *label != TRASH && !wanted.contains(label))
        .cloned()
        .collect();
    (add, remove)
}

type Reply<'a> = Pin<Box<dyn Future<Output = Result<Value, &'static str>> + Send + 'a>>;

/// The account-bound Gmail calls a place needs; tests answer them in memory.
trait Remote: Send + Sync {
    /// The message's `labelIds` as Gmail has them now.
    fn labels<'a>(&'a self, id: &'a str) -> Reply<'a>;
    /// A settled `gmail.batchModify`, `gmail.trash` or `gmail.untrash`.
    fn write<'a>(&'a self, method: &'a str, params: Value) -> Reply<'a>;
    fn account(&self) -> &str;
}

type Read<'a, T> = Pin<Box<dyn Future<Output = Result<T, &'static str>> + Send + 'a>>;

/// Each message's location labels, read a few at a time, in order.
async fn locations<'a>(
    remote: &'a impl Remote,
    ids: impl Iterator<Item = &'a str>,
) -> Vec<Result<Vec<String>, &'static str>> {
    // Boxed before streaming: a closure returning borrowed futures is not
    // general enough for the backend's Send futures.
    let reads: Vec<Read<'a, Vec<String>>> = ids
        .map(|id| -> Read<'a, Vec<String>> {
            Box::pin(async move { location(&remote.labels(id).await?) })
        })
        .collect();
    stream::iter(reads).buffered(READS).collect().await
}

async fn locate_with(remote: &impl Remote, ids: &[String]) -> Vec<Located> {
    locations(remote, ids.iter().map(String::as_str))
        .await
        .into_iter()
        .map(|labels| labels.map(|labels| place_of(&labels)))
        .collect()
}

/// One message on its way: the labels it has now, and what has happened.
struct Move {
    current: Vec<String>,
    wanted: Vec<String>,
    moved: bool,
    failed: Option<&'static str>,
}

/// The labels after trash or untrash: Gmail answers the message, and a reply
/// without its labels is read again rather than guessed.
async fn labels_after(
    remote: &impl Remote,
    id: &str,
    reply: &Value,
) -> Result<Vec<String>, &'static str> {
    match reply.get("labelIds") {
        Some(labels @ Value::Array(_)) => location(labels),
        _ => location(&remote.labels(id).await?),
    }
}

/// Untrash or trash every move that needs it, one message per call (the
/// endpoints take one id).
async fn trash_phase(remote: &impl Remote, items: &[PlaceItem], moves: &mut [Move], trash: bool) {
    for (item, entry) in items.iter().zip(moves.iter_mut()) {
        if entry.failed.is_some()
            || entry.current.iter().any(|label| label == TRASH) == trash
            || entry.wanted.iter().any(|label| label == TRASH) != trash
        {
            continue;
        }
        let method = if trash {
            "gmail.trash"
        } else {
            "gmail.untrash"
        };
        let params = json!({"accountId":remote.account(),"id":item.id});
        match remote.write(method, params).await {
            Ok(reply) => {
                entry.moved = true;
                match labels_after(remote, &item.id, &reply).await {
                    Ok(labels) => entry.current = labels,
                    Err(code) => entry.failed = Some(code),
                }
            }
            Err(code) => entry.failed = Some(code),
        }
    }
}

fn trashing(entry: &Move) -> bool {
    !entry.current.iter().any(|label| label == TRASH)
        && entry.wanted.iter().any(|label| label == TRASH)
}

/// Every remaining label difference, one `batchModify` per equal change. A
/// message still to be trashed waits: trashing may change its labels.
async fn modify_phase(remote: &impl Remote, items: &[PlaceItem], moves: &mut [Move]) {
    let mut groups: BTreeMap<(Vec<String>, Vec<String>), Vec<usize>> = BTreeMap::new();
    for (index, entry) in moves.iter().enumerate() {
        if entry.failed.is_some() || trashing(entry) {
            continue;
        }
        let (add, remove) = labels_change(&entry.current, &entry.wanted);
        if !add.is_empty() || !remove.is_empty() {
            groups.entry((add, remove)).or_default().push(index);
        }
    }
    for ((add, remove), members) in groups {
        for chunk in members.chunks(MAX_BATCH) {
            let ids: Vec<&str> = chunk
                .iter()
                .map(|index| items[*index].id.as_str())
                .collect();
            let params = json!({"accountId":remote.account(),"ids":ids,"addLabelIds":add,"removeLabelIds":remove});
            let answer = remote.write("gmail.batchModify", params).await;
            for index in chunk {
                let entry = &mut moves[*index];
                match &answer {
                    Ok(_) => {
                        entry.moved = true;
                        entry
                            .current
                            .retain(|label| label == TRASH || !remove.contains(label));
                        entry.current.extend(add.iter().cloned());
                        entry.current.sort();
                    }
                    Err(code) => entry.failed = Some(*code),
                }
            }
        }
    }
}

async fn place_with(remote: &impl Remote, items: &[PlaceItem]) -> Vec<PlaceOutcome> {
    crate::probe("place: reading current labels");
    let currents = locations(remote, items.iter().map(|item| item.id.as_str())).await;
    crate::probe("place: current labels read");
    let mut moves: Vec<Move> = items
        .iter()
        .zip(currents)
        .map(|(item, current)| {
            let wanted = place_labels(&item.place);
            let failed = current.as_ref().err().or(wanted.as_ref().err()).copied();
            Move {
                current: current.unwrap_or_default(),
                wanted: wanted.unwrap_or_default(),
                moved: false,
                failed,
            }
        })
        .collect();
    // Out of the trash first, so labels land on a message Gmail lists again;
    // into the trash last, then whatever trashing changed is set right.
    trash_phase(remote, items, &mut moves, false).await;
    crate::probe("place: untrash phase done");
    modify_phase(remote, items, &mut moves).await;
    crate::probe("place: modify phase done");
    trash_phase(remote, items, &mut moves, true).await;
    modify_phase(remote, items, &mut moves).await;
    crate::probe("place: all phases done");
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

struct Live<'a> {
    session: &'a Session,
    account: String,
}

impl Remote for Live<'_> {
    fn labels<'a>(&'a self, id: &'a str) -> Reply<'a> {
        Box::pin(async move {
            let account = self.account.as_str();
            self.session
                .account(account)?
                .pace(quota_cost("gmail.read"))
                .await?;
            let answer = self
                .session
                .get_with(
                    account,
                    || refresh_grant(account.to_owned()),
                    |token| async move {
                        gmail_http::get(
                            &["messages", id],
                            &[("format".into(), "minimal".into())],
                            &token,
                        )
                        .await
                    },
                )
                .await?;
            Ok(answer["labelIds"].clone())
        })
    }

    fn write<'a>(&'a self, method: &'a str, params: Value) -> Reply<'a> {
        Box::pin(async move { self.session.call_settled(method, &params).await })
    }

    fn account(&self) -> &str {
        &self.account
    }
}

impl Session {
    /// The account must be a registered Gmail account before any credential is read.
    async fn live(&self, account: &Account) -> Result<Live<'_>, &'static str> {
        let id = field(&json!({"accountId":account.id}), "accountId", true)?.to_lowercase();
        let accounts = tokio::task::spawn_blocking(crate::account::list_readonly)
            .await
            .map_err(|_| "session_failed")??;
        if !accounts["accounts"].as_array().is_some_and(|entries| {
            entries
                .iter()
                .any(|a| a["id"] == id && a["provider"] == "gmail")
        }) {
            return Err("gmail_account_unknown");
        }
        Ok(Live {
            session: self,
            account: id,
        })
    }

    pub(crate) async fn locate(
        &self,
        account: &Account,
        ids: &[String],
    ) -> Result<Vec<Located>, &'static str> {
        let live = self.live(account).await?;
        Ok(locate_with(&live, ids).await)
    }

    pub(crate) async fn place(
        &self,
        account: &Account,
        items: &[PlaceItem],
    ) -> Result<Vec<PlaceOutcome>, &'static str> {
        crate::probe("place: checking account");
        let live = self.live(account).await?;
        crate::probe("place: account checked");
        Ok(place_with(&live, items).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Gmail in memory: labels per message, every write recorded. Trashing
    /// takes the message out of the Inbox, which Gmail's trash may also do.
    struct Fake {
        labels: Mutex<HashMap<String, Vec<String>>>,
        writes: Mutex<Vec<(String, Value)>>,
        refuse: Option<&'static str>,
        silent_trash: bool,
    }

    fn fake(messages: &[(&str, &[&str])]) -> Fake {
        Fake {
            labels: Mutex::new(
                messages
                    .iter()
                    .map(|(id, labels)| {
                        (
                            (*id).to_owned(),
                            labels.iter().map(|label| (*label).to_owned()).collect(),
                        )
                    })
                    .collect(),
            ),
            writes: Mutex::default(),
            refuse: None,
            silent_trash: false,
        }
    }

    impl Fake {
        fn now(&self, id: &str) -> Vec<String> {
            let mut labels = self.labels.lock().unwrap()[id].clone();
            labels.sort();
            labels
        }
        fn writes(&self) -> Vec<(String, Value)> {
            self.writes.lock().unwrap().clone()
        }
    }

    impl Remote for Fake {
        fn labels<'a>(&'a self, id: &'a str) -> Reply<'a> {
            Box::pin(async move {
                self.labels
                    .lock()
                    .unwrap()
                    .get(id)
                    .map(|labels| json!(labels))
                    .ok_or("gmail_http_failed")
            })
        }

        fn write<'a>(&'a self, method: &'a str, params: Value) -> Reply<'a> {
            Box::pin(async move {
                self.writes
                    .lock()
                    .unwrap()
                    .push((method.to_owned(), params.clone()));
                if let Some(code) = self.refuse {
                    return Err(code);
                }
                let mut store = self.labels.lock().unwrap();
                let strings = |key: &str| -> Vec<String> {
                    params[key]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|value| value.as_str().unwrap().to_owned())
                        .collect()
                };
                match method {
                    "gmail.batchModify" => {
                        let (add, remove) = (strings("addLabelIds"), strings("removeLabelIds"));
                        for id in strings("ids") {
                            let labels = store.get_mut(&id).unwrap();
                            labels.retain(|label| !remove.contains(label));
                            for label in &add {
                                if !labels.contains(label) {
                                    labels.push(label.clone());
                                }
                            }
                        }
                        Ok(json!({}))
                    }
                    "gmail.trash" | "gmail.untrash" => {
                        let id = params["id"].as_str().unwrap();
                        let labels = store.get_mut(id).unwrap();
                        if method == "gmail.trash" {
                            labels.retain(|label| label != "INBOX");
                            labels.push(TRASH.into());
                        } else {
                            labels.retain(|label| label != TRASH);
                        }
                        if self.silent_trash {
                            return Ok(json!({"id":id}));
                        }
                        Ok(json!({"id":id,"labelIds":labels.clone()}))
                    }
                    _ => panic!("unexpected write {method}"),
                }
            })
        }

        fn account(&self) -> &str {
            "me@example.org"
        }
    }

    fn item(id: &str, labels: &[&str]) -> PlaceItem {
        PlaceItem {
            id: id.into(),
            place: json!({"labels":labels}),
        }
    }

    fn moved(id: &str) -> PlaceOutcome {
        Ok(Placed {
            new_id: id.into(),
            moved: true,
        })
    }

    fn names_trash_or_volatile(writes: &[(String, Value)]) -> bool {
        writes.iter().any(|(method, params)| {
            method == "gmail.batchModify"
                && ["addLabelIds", "removeLabelIds"].iter().any(|key| {
                    params[key]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|label| label == TRASH || volatile(label.as_str().unwrap()))
                })
        })
    }

    #[tokio::test]
    async fn locate_answers_sorted_location_labels_without_volatile_ones() {
        let remote = fake(&[
            (
                "m1",
                &[
                    "UNREAD",
                    "Label_9",
                    "INBOX",
                    "CATEGORY_SOCIAL",
                    "STARRED",
                    "IMPORTANT",
                ],
            ),
            ("m2", &["SENT", "CHAT", "DRAFT"]),
            ("m3", &["Label 1"]),
        ]);
        let ids = ["m1", "m2", "missing", "m3"].map(String::from);
        assert_eq!(
            locate_with(&remote, &ids).await,
            vec![
                Ok(json!({"labels":["INBOX","Label_9"]})),
                Ok(json!({"labels":[]})),
                Err("gmail_http_failed"),
                Err("gmail_invalid_response"),
            ]
        );
        assert!(remote.writes().is_empty());
        for place in [json!({"labels":["INBOX","Label_9"]}), json!({"labels":[]})] {
            assert_eq!(validate_place(&place), Ok(()));
        }
    }

    #[test]
    fn invalid_places_are_refused() {
        for place in [
            json!([]),
            json!({}),
            json!({"labels":["INBOX"],"extra":true}),
            json!({"labels":"INBOX"}),
            json!({"labels":[1]}),
            json!({"labels":["Label_2","INBOX"]}),
            json!({"labels":["INBOX","INBOX"]}),
            json!({"labels":["INBOX","UNREAD"]}),
            json!({"labels":["STARRED"]}),
            json!({"labels":["CATEGORY_UPDATES"]}),
            json!({"labels":[""]}),
            json!({"labels":["Label 1"]}),
            json!({"labels":["../x"]}),
            json!({"labels":["x".repeat(MAX_LABEL_ID + 1)]}),
            json!({"labels":(0..=MAX_LABELS).map(|n| format!("Label_{n:04}")).collect::<Vec<_>>()}),
        ] {
            assert_eq!(validate_place(&place), Err("invalid_params"), "{place}");
        }
    }

    #[tokio::test]
    async fn place_writes_only_the_label_difference() {
        let remote = fake(&[("m1", &["INBOX", "Label_1", "UNREAD", "STARRED"])]);
        let outcome = place_with(&remote, &[item("m1", &["Label_2"])]).await;
        assert_eq!(outcome, vec![moved("m1")]);
        assert_eq!(
            remote.writes(),
            vec![(
                "gmail.batchModify".into(),
                json!({"accountId":"me@example.org","ids":["m1"],"addLabelIds":["Label_2"],"removeLabelIds":["INBOX","Label_1"]})
            )]
        );
        assert_eq!(remote.now("m1"), ["Label_2", "STARRED", "UNREAD"]);
    }

    #[tokio::test]
    async fn trash_moves_through_the_trash_endpoints_only() {
        let remote = fake(&[
            ("in", &["INBOX", "Label_1", "UNREAD"]),
            ("out", &["TRASH", "Label_1", "STARRED"]),
        ]);
        let outcome = place_with(
            &remote,
            &[
                item("in", &["Label_1", "TRASH"]),
                item("out", &["INBOX", "Label_1"]),
            ],
        )
        .await;
        assert_eq!(outcome, vec![moved("in"), moved("out")]);
        let writes = remote.writes();
        assert_eq!(
            writes
                .iter()
                .map(|(method, _)| method.as_str())
                .collect::<Vec<_>>(),
            ["gmail.untrash", "gmail.batchModify", "gmail.trash"]
        );
        assert_eq!(
            writes[0].1,
            json!({"accountId":"me@example.org","id":"out"})
        );
        assert_eq!(writes[1].1["ids"], json!(["out"]));
        assert_eq!(writes[1].1["addLabelIds"], json!(["INBOX"]));
        assert_eq!(writes[2].1, json!({"accountId":"me@example.org","id":"in"}));
        assert!(!names_trash_or_volatile(&writes));
        assert_eq!(remote.now("in"), ["Label_1", "TRASH", "UNREAD"]);
        assert_eq!(remote.now("out"), ["INBOX", "Label_1", "STARRED"]);
    }

    #[tokio::test]
    async fn labels_trashing_removed_are_put_back_afterwards() {
        let mut remote = fake(&[("m1", &["INBOX"])]);
        remote.silent_trash = true;
        let outcome = place_with(&remote, &[item("m1", &["INBOX", "TRASH"])]).await;
        assert_eq!(outcome, vec![moved("m1")]);
        let writes = remote.writes();
        assert_eq!(writes[0].0, "gmail.trash");
        assert_eq!(writes[1].0, "gmail.batchModify");
        assert_eq!(writes[1].1["addLabelIds"], json!(["INBOX"]));
        assert_eq!(writes.len(), 2);
        assert_eq!(remote.now("m1"), ["INBOX", "TRASH"]);
    }

    #[tokio::test]
    async fn equal_changes_share_one_batch_call() {
        let remote = fake(&[
            ("a", &["INBOX"]),
            ("b", &["INBOX", "UNREAD"]),
            ("c", &["INBOX", "IMPORTANT"]),
            ("d", &["Label_1"]),
        ]);
        let outcome = place_with(
            &remote,
            &[
                item("a", &[]),
                item("b", &[]),
                item("c", &[]),
                item("d", &["SPAM"]),
            ],
        )
        .await;
        assert_eq!(outcome, ["a", "b", "c", "d"].map(moved));
        let writes = remote.writes();
        assert_eq!(writes.len(), 2);
        let archive = writes
            .iter()
            .find(|(_, params)| params["removeLabelIds"] == json!(["INBOX"]))
            .unwrap();
        assert_eq!(archive.1["ids"], json!(["a", "b", "c"]));
        assert_eq!(archive.1["addLabelIds"], json!([]));
        assert!(!names_trash_or_volatile(&writes));
    }

    #[tokio::test]
    async fn a_message_already_in_place_is_not_written() {
        let remote = fake(&[("m1", &["INBOX", "UNREAD", "CATEGORY_PROMOTIONS"])]);
        let outcome = place_with(&remote, &[item("m1", &["INBOX"])]).await;
        assert_eq!(
            outcome,
            vec![Ok(Placed {
                new_id: "m1".into(),
                moved: false
            })]
        );
        assert!(remote.writes().is_empty());
    }

    #[tokio::test]
    async fn failures_are_per_message_and_stop_that_message_only() {
        let remote = fake(&[("m1", &["INBOX"])]);
        let outcome = place_with(
            &remote,
            &[item("missing", &["INBOX"]), item("m1", &["Label_1"])],
        )
        .await;
        assert_eq!(outcome, vec![Err("gmail_http_failed"), moved("m1")]);
        let mut refused = fake(&[("m1", &["TRASH"])]);
        refused.refuse = Some("gmail_forbidden");
        let outcome = place_with(&refused, &[item("m1", &["INBOX"])]).await;
        assert_eq!(outcome, vec![Err("gmail_forbidden")]);
        // The untrash was refused, so its labels were never modified.
        assert_eq!(refused.writes().len(), 1);
    }

    #[tokio::test]
    async fn locate_and_place_cycles_are_exact_and_leave_volatile_labels_alone() {
        let remote = fake(&[(
            "m1",
            &["INBOX", "Label_1", "UNREAD", "STARRED", "CATEGORY_SOCIAL"],
        )]);
        let ids = ["m1".to_owned()];
        let before = locate_with(&remote, &ids).await[0].clone().unwrap();
        for destination in [
            json!({"labels":["TRASH"]}),
            json!({"labels":["SPAM"]}),
            json!({"labels":[]}),
        ] {
            let item = PlaceItem {
                id: "m1".into(),
                place: destination.clone(),
            };
            assert_eq!(place_with(&remote, &[item]).await, vec![moved("m1")]);
            assert_eq!(locate_with(&remote, &ids).await[0], Ok(destination));
            let back = PlaceItem {
                id: "m1".into(),
                place: before.clone(),
            };
            assert_eq!(place_with(&remote, &[back]).await, vec![moved("m1")]);
            assert_eq!(locate_with(&remote, &ids).await[0], Ok(before.clone()));
            assert_eq!(
                remote.now("m1"),
                ["CATEGORY_SOCIAL", "INBOX", "Label_1", "STARRED", "UNREAD"]
            );
        }
        assert!(!names_trash_or_volatile(&remote.writes()));
    }
}
