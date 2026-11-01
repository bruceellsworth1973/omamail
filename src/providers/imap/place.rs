//! Undo history for IMAP, Outlook and Proton Bridge. A place is
//! `{"folder", "messageId"}`: the folder as LIST names it, and the message's
//! Message-ID header (empty when it has none usable). See `crate::mail::place`
//! and docs/BACKEND.md "Undo history" for the contract every provider keeps.
//!
//! Ids are `<uid>:<folder>`, so a moved message gets a new id: the server's
//! COPYUID answer names its new UID, and without one the destination is
//! searched for its Message-ID. The Message-ID also proves that a UID still
//! names the message the place was read from. Moves use UID MOVE, or COPY,
//! `\Deleted` and UID EXPUNGE without it, like every other IMAP action; a
//! message leaving the trash loses a `\Deleted` flag first, as `imap.untrash`
//! does. Reads use `BODY.PEEK` and EXAMINE, so nothing is marked seen.
use super::read;
use super::*;
use crate::mail::Account;
use crate::mail::place::{Located, PlaceItem, PlaceOutcome, Placed};
use mailparse::MailHeaderMap;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;

/// Leaves room for `<uid>:` inside the 8 KiB id bound.
const MAX_FOLDER: usize = 4096;
/// One header line.
const MAX_MESSAGE_ID: usize = 998;
const REFUSED: &str = "imap_command_failed";
const LOST: &str = "undo_message_lost";
const GONE: &str = "undo_folder_gone";

/// The folder and Message-ID of a place, refused unless exactly what `locate`
/// produces.
fn place_fields(place: &Value) -> Result<(&str, &str)> {
    let object = place.as_object().ok_or("invalid_params")?;
    if object.len() != 2 {
        return Err("invalid_params");
    }
    let folder = object
        .get("folder")
        .and_then(Value::as_str)
        .filter(|folder| usable_folder(folder))
        .ok_or("invalid_params")?;
    let message_id = object
        .get("messageId")
        .and_then(Value::as_str)
        .filter(|value| usable_message_id(value))
        .ok_or("invalid_params")?;
    Ok((folder, message_id))
}

pub(crate) fn validate_place(place: &Value) -> Result<()> {
    place_fields(place).map(|_| ())
}

fn usable_folder(folder: &str) -> bool {
    !folder.is_empty() && folder.len() <= MAX_FOLDER && quote(folder).is_ok()
}

/// Printable ASCII without surrounding spaces: it is searched for as a quoted
/// string, which IMAP4rev1 allows only in ASCII.
fn usable_message_id(value: &str) -> bool {
    value.len() <= MAX_MESSAGE_ID
        && value.bytes().all(|byte| (0x20..0x7f).contains(&byte))
        && value.trim() == value
}

/// The Message-ID of a fetched header block; empty when it has none usable.
fn header_message_id(raw: &[u8]) -> String {
    mailparse::parse_headers(raw)
        .ok()
        .and_then(|(headers, _)| headers.get_first_value("Message-ID"))
        .map(|value| value.trim().to_owned())
        .filter(|value| usable_message_id(value))
        .unwrap_or_default()
}

/// The same folder: INBOX is case-insensitive, every other name exact.
fn same(a: &str, b: &str) -> bool {
    a == b || (a.eq_ignore_ascii_case("INBOX") && b.eq_ignore_ascii_case("INBOX"))
}

fn uid_list(uids: &[u32]) -> String {
    uids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// A UID set from a response, expanded in order; `None` when malformed or
/// longer than `limit`.
fn uid_set(text: &str, limit: usize) -> Option<Vec<u32>> {
    let parse = |value: &str| value.parse::<u32>().ok().filter(|uid| *uid > 0);
    let mut out = Vec::new();
    for part in text.split(',') {
        let (a, b) = part.split_once(':').unwrap_or((part, part));
        let (a, b) = (parse(a)?, parse(b)?);
        let (low, high) = (a.min(b), a.max(b));
        if usize::try_from(high - low).ok()? >= limit.saturating_sub(out.len()) {
            return None;
        }
        out.extend(low..=high);
    }
    Some(out)
}

/// Old UID to new UID, from every `[COPYUID validity from to]` response code
/// (untagged after MOVE, tagged after COPY), for the UIDs that were asked.
fn copied(data: &[u8], asked: &[u32]) -> BTreeMap<u32, u32> {
    const CODE: &str = "[COPYUID ";
    let asked: BTreeSet<u32> = asked.iter().copied().collect();
    let mut pairs = BTreeMap::new();
    for line in String::from_utf8_lossy(data).lines() {
        let Some(start) = line.to_ascii_uppercase().find(CODE) else {
            continue;
        };
        let Some((code, _)) = line[start + CODE.len()..].split_once(']') else {
            continue;
        };
        let words: Vec<&str> = code.split_ascii_whitespace().collect();
        let [_, from, to] = words[..] else {
            continue;
        };
        let (Some(from), Some(to)) = (uid_set(from, asked.len()), uid_set(to, asked.len())) else {
            continue;
        };
        if from.len() == to.len() {
            pairs.extend(
                from.into_iter()
                    .zip(to)
                    .filter(|(old, _)| asked.contains(old)),
            );
        }
    }
    pairs
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Fetched {
    message_id: String,
    deleted: bool,
}

/// Each fetched UID's Message-ID and `\Deleted` flag. Only rows carrying the
/// header answer for a UID: an unsolicited flag update proves nothing.
fn fetched(data: &[u8]) -> Result<BTreeMap<u32, Fetched>> {
    let mut found = BTreeMap::new();
    for row in read::nodes(data)? {
        if row.len() < 4 || !row[0].is("*") || !row[2].is("FETCH") {
            continue;
        }
        let (mut uid, mut deleted, mut header) = (None, false, None);
        for pair in row[3].list().chunks_exact(2) {
            if pair[0].is("UID") {
                uid = pair[1].number();
            } else if pair[0].is("FLAGS") {
                deleted = pair[1].list().iter().any(|flag| flag.is("\\Deleted"));
            } else if pair[0].text().to_ascii_uppercase().starts_with(b"BODY[") {
                header = Some(pair[1].text());
            }
        }
        if let (Some(uid), Some(header)) = (uid, header) {
            let message_id = header_message_id(header);
            found.insert(
                uid,
                Fetched {
                    message_id,
                    deleted,
                },
            );
        }
    }
    Ok(found)
}

fn uid_next(data: &[u8]) -> Result<Option<u32>> {
    for row in read::nodes(data)? {
        if row.len() >= 4 && row[0].is("*") && row[1].is("STATUS") {
            for pair in row[3].list().chunks_exact(2) {
                if pair[0].is("UIDNEXT") {
                    return Ok(pair[1].number());
                }
            }
        }
    }
    Ok(None)
}

/// What a place needs to know of the account's folders.
#[derive(Clone, Debug, Default)]
struct Folders {
    /// Selectable folders, as LIST spells them.
    names: Vec<String>,
    trash: Option<String>,
    /// COPYUID can be expected. Without it the destination's UIDNEXT is read
    /// before a move, so the Message-ID search sees only what arrived.
    uidplus: bool,
}

impl Folders {
    fn find(&self, name: &str) -> Option<&str> {
        self.names
            .iter()
            .find(|listed| same(listed, name))
            .map(String::as_str)
    }
}

type Step<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// The IMAP conversation a place needs, one selected folder at a time; tests
/// answer it in memory. `imap_command_failed` refuses that command alone; any
/// other error leaves the conversation unusable.
trait Remote: Send {
    fn folders(&mut self) -> Step<'_, Folders>;
    /// SELECT, or EXAMINE when nothing will be written.
    fn select<'a>(&'a mut self, folder: &'a str, examine: bool) -> Step<'a, ()>;
    /// The UIDs still in the selected folder, never marking them seen.
    fn fetch<'a>(&'a mut self, uids: &'a [u32]) -> Step<'a, BTreeMap<u32, Fetched>>;
    fn undelete<'a>(&'a mut self, uids: &'a [u32]) -> Step<'a, ()>;
    /// UIDNEXT of a folder other than the selected one.
    fn uid_next<'a>(&'a mut self, folder: &'a str) -> Step<'a, Option<u32>>;
    /// Moves UIDs out of the selected folder: the COPYUID pairs reported.
    fn transfer<'a>(&'a mut self, uids: &'a [u32], target: &'a str)
    -> Step<'a, BTreeMap<u32, u32>>;
    /// UIDs of the selected folder, from `from` on when given, whose
    /// Message-ID header is `message_id`.
    fn search<'a>(&'a mut self, message_id: &'a str, from: Option<u32>) -> Step<'a, Vec<u32>>;
}

/// A refused command fails only its own messages (the inner error); anything
/// else ends the call (the outer error).
fn refused<T>(result: Result<T>) -> Result<Result<T>> {
    match result {
        Err(code) if code != REFUSED => Err(code),
        other => Ok(other),
    }
}

/// Every answer still open takes the code that ended the call; a call that
/// ended before any answer is one whole-call error.
fn settle<T>(answers: Vec<Option<Result<T>>>, ended: Result<()>) -> Result<Vec<Result<T>>> {
    if let Err(code) = ended
        && answers.iter().all(Option::is_none)
    {
        return Err(code);
    }
    let code = ended.err().unwrap_or("mail_place_invalid_response");
    Ok(answers
        .into_iter()
        .map(|answer| answer.unwrap_or(Err(code)))
        .collect())
}

type Members = Vec<(usize, u32)>;

/// Ids grouped by folder, one SELECT each; answers keep request order.
fn by_folder<'a, T>(
    ids: impl Iterator<Item = &'a str>,
    answers: &mut [Option<Result<T>>],
) -> BTreeMap<String, Members> {
    let mut groups: BTreeMap<String, Members> = BTreeMap::new();
    for (index, id) in ids.enumerate() {
        match read::message_id(id) {
            Ok((uid, folder)) => groups.entry(folder).or_default().push((index, uid)),
            Err(code) => answers[index] = Some(Err(code)),
        }
    }
    groups
}

fn fail<T>(
    answers: &mut [Option<Result<T>>],
    indices: impl Iterator<Item = usize>,
    code: &'static str,
) {
    for index in indices {
        answers[index] = Some(Err(code));
    }
}

async fn locate_with(
    remote: &mut impl Remote,
    ids: &[String],
    answers: &mut [Option<Located>],
) -> Result<()> {
    for (folder, members) in by_folder(ids.iter().map(String::as_str), answers) {
        let indices = members.iter().map(|(index, _)| *index);
        if !usable_folder(&folder) {
            fail(answers, indices, "undo_unsupported");
            continue;
        }
        if refused(remote.select(&folder, true).await)?.is_err() {
            fail(answers, indices, LOST);
            continue;
        }
        let uids: Vec<u32> = members.iter().map(|(_, uid)| *uid).collect();
        let facts = match refused(remote.fetch(&uids).await)? {
            Ok(facts) => facts,
            Err(code) => {
                fail(answers, indices, code);
                continue;
            }
        };
        for (index, uid) in members {
            answers[index] = Some(
                facts
                    .get(&uid)
                    .map(|fact| json!({"folder":folder,"messageId":fact.message_id}))
                    .ok_or(LOST),
            );
        }
    }
    Ok(())
}

/// A message moved without a COPYUID answer, to be found by its Message-ID.
struct Waiting {
    index: usize,
    message_id: String,
    from: Option<u32>,
}

/// One source folder: check each message is still the one its place was read
/// from, then move what is not already there, one transfer per destination.
/// A folder whose messages all stay is only examined.
async fn place_folder(
    remote: &mut impl Remote,
    folders: &Folders,
    items: &[PlaceItem],
    source: &str,
    members: Members,
    answers: &mut [Option<PlaceOutcome>],
    waiting: &mut BTreeMap<String, Vec<Waiting>>,
) -> Result<()> {
    let target_of = |index: usize| {
        place_fields(&items[index].place)
            .ok()
            .and_then(|(folder, _)| folders.find(folder))
    };
    let staying = members
        .iter()
        .all(|(index, _)| target_of(*index).is_some_and(|target| same(target, source)));
    if let Err(code) = refused(remote.select(source, staying).await)? {
        fail(answers, members.iter().map(|(index, _)| *index), code);
        return Ok(());
    }
    let uids: Vec<u32> = members.iter().map(|(_, uid)| *uid).collect();
    let facts = match refused(remote.fetch(&uids).await)? {
        Ok(facts) => facts,
        Err(code) => {
            fail(answers, members.iter().map(|(index, _)| *index), code);
            return Ok(());
        }
    };
    let leaving_trash = folders
        .trash
        .as_deref()
        .is_some_and(|trash| same(trash, source));
    let mut moving: BTreeMap<&str, Vec<(usize, u32, String)>> = BTreeMap::new();
    let mut undelete = Vec::new();
    for (index, uid) in members {
        let item = &items[index];
        let recorded = place_fields(&item.place).map_or("", |(_, message_id)| message_id);
        let (Some(target), Some(fact)) = (target_of(index), facts.get(&uid)) else {
            answers[index] = Some(Err(LOST));
            continue;
        };
        // A UID can come to name another message (a new UIDVALIDITY).
        if !recorded.is_empty() && recorded != fact.message_id {
            answers[index] = Some(Err(LOST));
            continue;
        }
        if same(target, source) {
            answers[index] = Some(Ok(Placed {
                new_id: item.id.clone(),
                moved: false,
            }));
            continue;
        }
        if leaving_trash && fact.deleted {
            undelete.push(uid);
        }
        let message_id = fact.message_id.clone();
        moving
            .entry(target)
            .or_default()
            .push((index, uid, message_id));
    }
    if !undelete.is_empty()
        && let Err(code) = refused(remote.undelete(&undelete).await)?
    {
        let indices = moving.into_values().flatten().map(|(index, _, _)| index);
        fail(answers, indices, code);
        return Ok(());
    }
    for (target, moves) in moving {
        let from = if folders.uidplus {
            None
        } else {
            refused(remote.uid_next(target).await)?.unwrap_or(None)
        };
        let uids: Vec<u32> = moves.iter().map(|(_, uid, _)| *uid).collect();
        let pairs = match refused(remote.transfer(&uids, target).await)? {
            Ok(pairs) => pairs,
            Err(code) => {
                fail(answers, moves.iter().map(|(index, _, _)| *index), code);
                continue;
            }
        };
        for (index, uid, message_id) in moves {
            match pairs.get(&uid) {
                Some(new) => {
                    answers[index] = Some(Ok(Placed {
                        new_id: format!("{new}:{target}"),
                        moved: true,
                    }));
                }
                None => waiting.entry(target.to_owned()).or_default().push(Waiting {
                    index,
                    message_id,
                    from,
                }),
            }
        }
    }
    Ok(())
}

async fn place_with(
    remote: &mut impl Remote,
    items: &[PlaceItem],
    answers: &mut [Option<PlaceOutcome>],
) -> Result<()> {
    let folders = remote.folders().await?;
    let mut sources = by_folder(items.iter().map(|item| item.id.as_str()), answers);
    // A destination that is gone is the clearer answer: given before any read.
    for members in sources.values_mut() {
        members.retain(|(index, _)| {
            let answer = match place_fields(&items[*index].place) {
                Ok((target, _)) if folders.find(target).is_some() => return true,
                Ok(_) => GONE,
                Err(code) => code,
            };
            answers[*index] = Some(Err(answer));
            false
        });
    }
    let mut waiting = BTreeMap::new();
    for (source, members) in sources {
        if members.is_empty() {
            continue;
        }
        if folders.find(&source).is_none() {
            fail(answers, members.iter().map(|(index, _)| *index), LOST);
            continue;
        }
        place_folder(
            remote,
            &folders,
            items,
            &source,
            members,
            answers,
            &mut waiting,
        )
        .await?;
    }
    for (target, entries) in waiting {
        let examined = refused(remote.select(&target, true).await)?.is_ok();
        for entry in entries {
            let found = if examined && !entry.message_id.is_empty() {
                refused(remote.search(&entry.message_id, entry.from).await)?
                    .ok()
                    .and_then(|uids| {
                        uids.into_iter()
                            .filter(|uid| entry.from.is_none_or(|from| *uid >= from))
                            .max()
                    })
            } else {
                None
            };
            answers[entry.index] = Some(
                found
                    .map(|uid| Placed {
                        new_id: format!("{uid}:{target}"),
                        moved: true,
                    })
                    .ok_or(LOST),
            );
        }
    }
    Ok(())
}

/// One authenticated connection from the pool. A connection that failed
/// other than by a tagged refusal is dropped, never pooled or written again.
struct Live {
    wire: Option<Wire>,
    key: String,
    ended: &'static str,
    moves: bool,
}

impl Live {
    async fn open(p: &Value) -> Result<Self> {
        credentials(p)?;
        let (wire, key) = acquire(p).await?;
        Ok(Self {
            wire: Some(wire),
            key,
            ended: "mail_connection_closed",
            moves: false,
        })
    }

    fn check<T>(&mut self, result: Result<T>) -> Result<T> {
        if let Err(code) = result
            && code != REFUSED
        {
            self.wire = None;
            self.ended = code;
        }
        result
    }

    async fn run(&mut self, text: &str) -> Result<Vec<u8>> {
        let wire = self.wire.as_mut().ok_or(self.ended)?;
        let result = command(wire, text).await;
        self.check(result)
    }

    async fn finish(self) {
        if let Some(wire) = self.wire {
            release(wire, self.key).await;
        }
    }
}

impl Remote for Live {
    fn folders(&mut self) -> Step<'_, Folders> {
        Box::pin(async move {
            let wire = self.wire.as_mut().ok_or(self.ended)?;
            let result = read::discover_mailboxes(wire).await;
            let boxes = self.check(result)?;
            let has = |name: &str| {
                boxes
                    .capabilities
                    .iter()
                    .any(|capability| capability.eq_ignore_ascii_case(name))
            };
            self.moves = has("MOVE");
            Ok(Folders {
                names: boxes.selectable().map(str::to_owned).collect(),
                trash: boxes.special.get("\\trash").cloned(),
                uidplus: has("UIDPLUS"),
            })
        })
    }

    fn select<'a>(&'a mut self, folder: &'a str, examine: bool) -> Step<'a, ()> {
        Box::pin(async move {
            let verb = if examine { "EXAMINE" } else { "SELECT" };
            self.run(&format!("{verb} {}", quote(folder)?)).await?;
            Ok(())
        })
    }

    fn fetch<'a>(&'a mut self, uids: &'a [u32]) -> Step<'a, BTreeMap<u32, Fetched>> {
        Box::pin(async move {
            let set = uid_list(uids);
            let data = self
                .run(&format!(
                    "UID FETCH {set} (UID FLAGS BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])"
                ))
                .await?;
            fetched(&data)
        })
    }

    fn undelete<'a>(&'a mut self, uids: &'a [u32]) -> Step<'a, ()> {
        Box::pin(async move {
            let set = uid_list(uids);
            self.run(&format!("UID STORE {set} -FLAGS.SILENT (\\Deleted)"))
                .await?;
            Ok(())
        })
    }

    fn uid_next<'a>(&'a mut self, folder: &'a str) -> Step<'a, Option<u32>> {
        Box::pin(async move {
            let data = self
                .run(&format!("STATUS {} (UIDNEXT)", quote(folder)?))
                .await?;
            uid_next(&data)
        })
    }

    fn transfer<'a>(
        &'a mut self,
        uids: &'a [u32],
        target: &'a str,
    ) -> Step<'a, BTreeMap<u32, u32>> {
        Box::pin(async move {
            let set = uid_list(uids);
            let target = quote(target)?;
            let answer = if self.moves {
                self.run(&format!("UID MOVE {set} {target}")).await?
            } else {
                let answer = self.run(&format!("UID COPY {set} {target}")).await?;
                self.run(&format!("UID STORE {set} +FLAGS.SILENT (\\Deleted)"))
                    .await?;
                self.run(&format!("UID EXPUNGE {set}")).await?;
                answer
            };
            Ok(copied(&answer, uids))
        })
    }

    fn search<'a>(&'a mut self, message_id: &'a str, from: Option<u32>) -> Step<'a, Vec<u32>> {
        Box::pin(async move {
            let range = from
                .map(|from| format!("UID {from}:* "))
                .unwrap_or_default();
            let data = self
                .run(&format!(
                    "UID SEARCH {range}HEADER MESSAGE-ID {}",
                    quote(message_id)?
                ))
                .await?;
            read::search_uids(&data)
        })
    }
}

async fn locate_params(p: &Value, ids: &[String], answers: &mut [Option<Located>]) -> Result<()> {
    let mut live = Live::open(p).await?;
    locate_with(&mut live, ids, answers).await?;
    live.finish().await;
    Ok(())
}

async fn place_params(
    p: &Value,
    items: &[PlaceItem],
    answers: &mut [Option<PlaceOutcome>],
) -> Result<()> {
    let mut live = Live::open(p).await?;
    place_with(&mut live, items, answers).await?;
    live.finish().await;
    Ok(())
}

/// Read-only, live from the server: one entry per id, in order. `ids` are
/// unique and already pass `crate::mail::action::validate_message_id`.
pub(crate) async fn locate(account: &Account, ids: &[String]) -> Result<Vec<Located>> {
    let mut answers = vec![None; ids.len()];
    let ended = tokio::time::timeout(CALL_BUDGET, async {
        let request = json!({"accountId":account.id,"readOnly":true});
        let p = resolve_account(&request, "imap.locate").await?;
        locate_params(&p, ids, &mut answers).await
    })
    .await
    .unwrap_or(Err("request_timed_out"));
    settle(answers, ended)
}

/// Moves each message to its place: one entry per item, in order. Places have
/// passed `validate_place`; a message already there is `Placed { new_id: id,
/// moved: false }` with no write. Answers are kept outside the deadline, so a
/// late timeout cannot hide the moves already made.
pub(crate) async fn place(account: &Account, items: &[PlaceItem]) -> Result<Vec<PlaceOutcome>> {
    let mut answers = vec![None; items.len()];
    let ended = tokio::time::timeout(CALL_BUDGET, async {
        let p = resolve_account(&json!({"accountId":account.id}), "imap.place").await?;
        place_params(&p, items, &mut answers).await
    })
    .await
    .unwrap_or(Err("request_timed_out"));
    settle(answers, ended)
}

#[cfg(test)]
mod tests;
