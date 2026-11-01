use super::*;
use tokio::net::TcpListener;

#[derive(Clone, Debug, PartialEq)]
struct Message {
    message_id: String,
    seen: bool,
    deleted: bool,
}

fn message(message_id: &str) -> Message {
    Message {
        message_id: message_id.to_owned(),
        seen: false,
        deleted: false,
    }
}

/// An IMAP account in memory: folders of UIDs, every command logged. A logged
/// command starting with `refuse` gets a tagged NO.
struct Fake {
    boxes: BTreeMap<String, BTreeMap<u32, Message>>,
    next: BTreeMap<String, u32>,
    selected: Option<(String, bool)>,
    trash: Option<String>,
    uidplus: bool,
    copyuid: bool,
    log: Vec<String>,
    refuse: Option<String>,
}

fn fake(boxes: &[(&str, &[(u32, Message)])]) -> Fake {
    Fake {
        boxes: boxes
            .iter()
            .map(|(name, messages)| ((*name).to_owned(), messages.iter().cloned().collect()))
            .collect(),
        next: boxes
            .iter()
            .map(|(name, messages)| {
                let top = messages.iter().map(|(uid, _)| *uid).max().unwrap_or(0);
                ((*name).to_owned(), top + 1)
            })
            .collect(),
        selected: None,
        trash: Some("Trash".into()),
        uidplus: true,
        copyuid: true,
        log: Vec::new(),
        refuse: None,
    }
}

impl Fake {
    fn record(&mut self, entry: String) -> Result<()> {
        let refused = self
            .refuse
            .as_ref()
            .is_some_and(|prefix| entry.starts_with(prefix.as_str()));
        self.log.push(entry);
        if refused { Err(REFUSED) } else { Ok(()) }
    }
    fn folder(&self) -> &str {
        &self.selected.as_ref().expect("a folder is selected").0
    }
    fn writes(&self) -> Vec<&String> {
        self.log
            .iter()
            .filter(|entry| entry.starts_with("MOVE") || entry.starts_with("UNDELETE"))
            .collect()
    }
    fn find(&self, message_id: &str) -> Vec<String> {
        self.boxes
            .iter()
            .flat_map(|(name, messages)| {
                messages
                    .iter()
                    .filter(|(_, message)| message.message_id == message_id)
                    .map(move |(uid, _)| format!("{uid}:{name}"))
            })
            .collect()
    }
}

impl Remote for Fake {
    fn folders(&mut self) -> Step<'_, Folders> {
        Box::pin(async move {
            self.record("LIST".into())?;
            Ok(Folders {
                names: self.boxes.keys().cloned().collect(),
                trash: self.trash.clone(),
                uidplus: self.uidplus,
            })
        })
    }
    fn select<'a>(&'a mut self, folder: &'a str, examine: bool) -> Step<'a, ()> {
        Box::pin(async move {
            let verb = if examine { "EXAMINE" } else { "SELECT" };
            self.record(format!("{verb} {folder}"))?;
            // Like a server, INBOX in any case.
            let Some(name) = self.boxes.keys().find(|name| same(name, folder)) else {
                return Err(REFUSED);
            };
            self.selected = Some((name.clone(), examine));
            Ok(())
        })
    }
    fn fetch<'a>(&'a mut self, uids: &'a [u32]) -> Step<'a, BTreeMap<u32, Fetched>> {
        Box::pin(async move {
            self.record(format!("FETCH {}", uid_list(uids)))?;
            let messages = &self.boxes[self.folder()];
            Ok(uids
                .iter()
                .filter_map(|uid| {
                    messages.get(uid).map(|message| {
                        let fact = Fetched {
                            message_id: message.message_id.clone(),
                            deleted: message.deleted,
                        };
                        (*uid, fact)
                    })
                })
                .collect())
        })
    }
    fn undelete<'a>(&'a mut self, uids: &'a [u32]) -> Step<'a, ()> {
        Box::pin(async move {
            self.record(format!("UNDELETE {}", uid_list(uids)))?;
            let folder = self.folder().to_owned();
            for uid in uids {
                if let Some(message) = self.boxes.get_mut(&folder).unwrap().get_mut(uid) {
                    message.deleted = false;
                }
            }
            Ok(())
        })
    }
    fn uid_next<'a>(&'a mut self, folder: &'a str) -> Step<'a, Option<u32>> {
        Box::pin(async move {
            self.record(format!("STATUS {folder}"))?;
            Ok(self.next.get(folder).copied())
        })
    }
    fn transfer<'a>(
        &'a mut self,
        uids: &'a [u32],
        target: &'a str,
    ) -> Step<'a, BTreeMap<u32, u32>> {
        Box::pin(async move {
            self.record(format!("MOVE {} {target}", uid_list(uids)))?;
            let (source, examined) = self.selected.clone().unwrap();
            assert!(!examined, "nothing is written to an examined folder");
            let mut pairs = BTreeMap::new();
            for uid in uids {
                if let Some(message) = self.boxes.get_mut(&source).unwrap().remove(uid) {
                    let next = self.next.get_mut(target).unwrap();
                    self.boxes.get_mut(target).unwrap().insert(*next, message);
                    pairs.insert(*uid, *next);
                    *next += 1;
                }
            }
            Ok(if self.copyuid { pairs } else { BTreeMap::new() })
        })
    }
    fn search<'a>(&'a mut self, message_id: &'a str, from: Option<u32>) -> Step<'a, Vec<u32>> {
        Box::pin(async move {
            self.record(format!("SEARCH {message_id} {from:?}"))?;
            Ok(self.boxes[self.folder()]
                .iter()
                .filter(|(uid, message)| {
                    message.message_id == message_id && from.is_none_or(|from| **uid >= from)
                })
                .map(|(uid, _)| *uid)
                .collect())
        })
    }
}

fn place_of(folder: &str, message_id: &str) -> Value {
    json!({"folder":folder,"messageId":message_id})
}

fn item(id: &str, place: Value) -> PlaceItem {
    PlaceItem {
        id: id.to_owned(),
        place,
    }
}

fn ids(values: &[&str]) -> Vec<String> {
    values.iter().map(|id| (*id).to_owned()).collect()
}

async fn locate_fake(remote: &mut Fake, ids: &[String]) -> Vec<Located> {
    let mut answers = vec![None; ids.len()];
    let ended = locate_with(remote, ids, &mut answers).await;
    settle(answers, ended).unwrap()
}

async fn place_fake(remote: &mut Fake, items: &[PlaceItem]) -> Vec<PlaceOutcome> {
    let mut answers = vec![None; items.len()];
    let ended = place_with(remote, items, &mut answers).await;
    settle(answers, ended).unwrap()
}

fn moved(new_id: &str) -> PlaceOutcome {
    Ok(Placed {
        new_id: new_id.to_owned(),
        moved: true,
    })
}

fn stayed(id: &str) -> PlaceOutcome {
    Ok(Placed {
        new_id: id.to_owned(),
        moved: false,
    })
}

#[test]
fn only_the_exact_place_shape_is_accepted() {
    for place in [
        place_of("INBOX", "<a@example.org>"),
        place_of("Archive/2026", ""),
        place_of("&ZeVnLIqe-", "<a b@example.org>"),
    ] {
        assert_eq!(validate_place(&place), Ok(()), "{place}");
    }
    let long = "x".repeat(MAX_FOLDER + 1);
    let long_id = format!("<{}>", "x".repeat(MAX_MESSAGE_ID));
    for place in [
        json!({"folder":"INBOX"}),
        json!({"messageId":""}),
        json!({"folder":"INBOX","messageId":"","flags":[]}),
        json!({"folder":"","messageId":""}),
        json!({"folder":"IN\rBOX","messageId":""}),
        json!({"folder":"INBOX\0","messageId":""}),
        json!({"folder":long,"messageId":""}),
        json!({"folder":7,"messageId":""}),
        json!({"folder":"INBOX","messageId":null}),
        json!({"folder":"INBOX","messageId":" <a@example.org>"}),
        json!({"folder":"INBOX","messageId":"<a@example.org>\r\nX: y"}),
        json!({"folder":"INBOX","messageId":"<é@example.org>"}),
        json!({"folder":"INBOX","messageId":long_id}),
        json!(["INBOX", ""]),
    ] {
        assert_eq!(validate_place(&place), Err("invalid_params"), "{place}");
    }
}

#[tokio::test]
async fn locate_answers_folder_and_message_id_and_only_examines() {
    let mut remote = fake(&[
        (
            "INBOX",
            &[(1, message("<a@example.org>")), (2, message(""))],
        ),
        ("Archive", &[(5, message("<b@example.org>"))]),
    ]);
    let located = locate_fake(
        &mut remote,
        &ids(&["5:Archive", "1:INBOX", "9:INBOX", "2:INBOX", "3:Gone"]),
    )
    .await;
    assert_eq!(
        located,
        vec![
            Ok(place_of("Archive", "<b@example.org>")),
            Ok(place_of("INBOX", "<a@example.org>")),
            Err(LOST),
            Ok(place_of("INBOX", "")),
            Err(LOST),
        ]
    );
    for place in located.iter().flatten() {
        assert_eq!(validate_place(place), Ok(()));
    }
    assert_eq!(
        remote.log,
        [
            "EXAMINE Archive",
            "FETCH 5",
            "EXAMINE Gone",
            "EXAMINE INBOX",
            "FETCH 1,9,2"
        ]
    );
}

#[tokio::test]
async fn place_moves_and_takes_the_new_uid_from_copyuid() {
    let mut inbox = message("<a@example.org>");
    inbox.seen = true;
    let mut remote = fake(&[
        (
            "INBOX",
            &[(1, inbox.clone()), (2, message("<b@example.org>"))],
        ),
        ("Archive", &[(40, message("<c@example.org>"))]),
        ("Trash", &[]),
    ]);
    let outcomes = place_fake(
        &mut remote,
        &[
            item("2:INBOX", place_of("Trash", "<b@example.org>")),
            item("1:INBOX", place_of("Archive", "<a@example.org>")),
        ],
    )
    .await;
    assert_eq!(outcomes, vec![moved("1:Trash"), moved("41:Archive")]);
    assert_eq!(
        remote.log,
        [
            "LIST",
            "SELECT INBOX",
            "FETCH 2,1",
            "MOVE 1 Archive",
            "MOVE 2 Trash"
        ]
    );
    // Read state travels with the message and is never written.
    assert_eq!(remote.boxes["Archive"][&41], inbox);
}

#[tokio::test]
async fn without_copyuid_the_new_uid_is_searched_among_what_arrived() {
    let mut remote = fake(&[
        ("INBOX", &[(1, message("<a@example.org>"))]),
        // An older copy of the same message must not be taken.
        ("Archive", &[(40, message("<a@example.org>"))]),
    ]);
    remote.uidplus = false;
    remote.copyuid = false;
    let outcomes = place_fake(
        &mut remote,
        &[item("1:INBOX", place_of("Archive", "<a@example.org>"))],
    )
    .await;
    assert_eq!(outcomes, vec![moved("41:Archive")]);
    assert_eq!(
        remote.log,
        [
            "LIST",
            "SELECT INBOX",
            "FETCH 1",
            "STATUS Archive",
            "MOVE 1 Archive",
            "EXAMINE Archive",
            "SEARCH <a@example.org> Some(41)"
        ]
    );
}

#[tokio::test]
async fn uidplus_without_copyuid_searches_the_whole_folder() {
    let mut remote = fake(&[
        ("INBOX", &[(1, message("<a@example.org>"))]),
        ("Archive", &[(40, message("<z@example.org>"))]),
    ]);
    remote.copyuid = false;
    let outcomes = place_fake(
        &mut remote,
        &[item("1:INBOX", place_of("Archive", "<a@example.org>"))],
    )
    .await;
    assert_eq!(outcomes, vec![moved("41:Archive")]);
    assert!(
        remote
            .log
            .contains(&"SEARCH <a@example.org> None".to_owned())
    );
}

#[tokio::test]
async fn a_moved_message_without_message_id_or_copyuid_is_lost() {
    let mut remote = fake(&[("INBOX", &[(1, message(""))]), ("Archive", &[])]);
    remote.copyuid = false;
    let outcomes = place_fake(&mut remote, &[item("1:INBOX", place_of("Archive", ""))]).await;
    assert_eq!(outcomes, vec![Err(LOST)]);
    assert_eq!(remote.boxes["Archive"].len(), 1);
    assert!(!remote.log.iter().any(|entry| entry.starts_with("SEARCH")));
}

#[tokio::test]
async fn a_message_already_in_place_is_not_written() {
    let mut remote = fake(&[
        ("INBOX", &[(1, message("<a@example.org>"))]),
        ("Archive", &[]),
    ]);
    let outcomes = place_fake(
        &mut remote,
        &[item("1:inbox", place_of("INBOX", "<a@example.org>"))],
    )
    .await;
    assert_eq!(outcomes, vec![stayed("1:inbox")]);
    assert_eq!(remote.log, ["LIST", "EXAMINE inbox", "FETCH 1"]);
}

#[tokio::test]
async fn a_destination_that_is_gone_refuses_before_any_read() {
    let mut remote = fake(&[("INBOX", &[(1, message("<a@example.org>"))])]);
    let outcomes = place_fake(
        &mut remote,
        &[item("1:INBOX", place_of("Old Projects", "<a@example.org>"))],
    )
    .await;
    assert_eq!(outcomes, vec![Err(GONE)]);
    assert_eq!(remote.log, ["LIST"]);
}

#[tokio::test]
async fn messages_that_are_gone_or_replaced_are_lost_and_never_moved() {
    let mut remote = fake(&[
        ("INBOX", &[(1, message("<other@example.org>"))]),
        ("Archive", &[]),
    ]);
    let outcomes = place_fake(
        &mut remote,
        &[
            // The UID now names another message.
            item("1:INBOX", place_of("Archive", "<a@example.org>")),
            item("2:INBOX", place_of("Archive", "<b@example.org>")),
            item("3:Deleted Folder", place_of("Archive", "")),
        ],
    )
    .await;
    assert_eq!(outcomes, vec![Err(LOST), Err(LOST), Err(LOST)]);
    assert!(remote.writes().is_empty());
}

#[tokio::test]
async fn leaving_the_trash_clears_deleted_first() {
    let mut trashed = message("<a@example.org>");
    trashed.deleted = true;
    let mut remote = fake(&[
        ("INBOX", &[]),
        ("Trash", &[(3, trashed), (4, message("<b@example.org>"))]),
    ]);
    let outcomes = place_fake(
        &mut remote,
        &[
            item("3:Trash", place_of("INBOX", "<a@example.org>")),
            item("4:Trash", place_of("INBOX", "<b@example.org>")),
        ],
    )
    .await;
    assert_eq!(outcomes, vec![moved("1:INBOX"), moved("2:INBOX")]);
    assert_eq!(remote.writes(), ["UNDELETE 3", "MOVE 3,4 INBOX"]);
    assert!(!remote.boxes["INBOX"][&1].deleted);
}

#[tokio::test]
async fn a_refused_move_fails_only_its_own_messages() {
    let mut remote = fake(&[
        (
            "INBOX",
            &[
                (1, message("<a@example.org>")),
                (2, message("<b@example.org>")),
            ],
        ),
        ("Archive", &[]),
        ("Trash", &[]),
    ]);
    remote.refuse = Some("MOVE 1 Archive".into());
    let outcomes = place_fake(
        &mut remote,
        &[
            item("1:INBOX", place_of("Archive", "<a@example.org>")),
            item("2:INBOX", place_of("Trash", "<b@example.org>")),
        ],
    )
    .await;
    assert_eq!(outcomes, vec![Err(REFUSED), moved("1:Trash")]);
}

#[tokio::test]
async fn a_broken_connection_answers_every_open_item() {
    let mut remote = fake(&[
        ("INBOX", &[(1, message("<a@example.org>"))]),
        ("Archive", &[]),
    ]);
    let items = [item("1:INBOX", place_of("Archive", "<a@example.org>"))];
    let mut answers = vec![None; 1];
    remote.refuse = Some("SELECT".into());
    // A refusal is per message; the wire's other failures end the call.
    place_with(&mut remote, &items, &mut answers).await.unwrap();
    assert_eq!(answers, vec![Some(Err(REFUSED))]);
    assert_eq!(
        settle::<Placed>(vec![None, Some(Err(LOST))], Err("mail_network_failed")),
        Ok(vec![Err("mail_network_failed"), Err(LOST)])
    );
    assert_eq!(
        settle::<Placed>(vec![None, None], Err("mail_auth_failed")),
        Err("mail_auth_failed")
    );
}

#[tokio::test]
async fn locate_and_place_cycles_are_exact() {
    for (uidplus, copyuid) in [(true, true), (false, false), (true, false)] {
        let mut deleted = message("<c@example.org>");
        deleted.deleted = true;
        let mut seen = message("<a@example.org>");
        seen.seen = true;
        let mut remote = fake(&[
            (
                "INBOX",
                &[(1, seen.clone()), (2, message("<b@example.org>"))],
            ),
            ("Archive", &[]),
            ("Trash", &[(7, deleted)]),
            ("Lists/Rust", &[(3, message(""))]),
        ]);
        remote.uidplus = uidplus;
        remote.copyuid = copyuid;
        let mut current = ids(&["1:INBOX", "2:INBOX", "7:Trash", "3:Lists/Rust"]);
        let before: Vec<Value> = locate_fake(&mut remote, &current)
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();
        // The user's action: everything to the archive.
        let action: Vec<PlaceItem> = current
            .iter()
            .map(|id| item(id, place_of("Archive", "")))
            .collect();
        let outcomes = place_fake(&mut remote, &action).await;
        // A message without a Message-ID is only found again through COPYUID.
        let findable = if copyuid { 4 } else { 3 };
        assert_eq!(outcomes.iter().flatten().count(), findable);
        current = outcomes
            .into_iter()
            .zip(&current)
            .map(|(outcome, id)| outcome.map_or(id.clone(), |placed| placed.new_id))
            .collect();
        let after: Vec<Value> = locate_fake(&mut remote, &current[..findable])
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();
        for round in 0..6 {
            let (step, wanted) = if round % 2 == 0 {
                ("undo", &before)
            } else {
                ("redo", &after)
            };
            // What undo and redo do first: read where the messages are now.
            assert!(
                locate_fake(&mut remote, &current[..findable])
                    .await
                    .iter()
                    .all(Result::is_ok)
            );
            let items: Vec<PlaceItem> = current
                .iter()
                .zip(wanted)
                .take(findable)
                .map(|(id, place)| item(id, place.clone()))
                .collect();
            let outcomes = place_fake(&mut remote, &items).await;
            for (index, outcome) in outcomes.into_iter().enumerate() {
                current[index] = outcome
                    .unwrap_or_else(|code| panic!("{step}: {code}"))
                    .new_id;
            }
            let located: Vec<Value> = locate_fake(&mut remote, &current[..findable])
                .await
                .into_iter()
                .map(Result::unwrap)
                .collect();
            assert_eq!(located, wanted[..findable], "{step} {round}");
        }
        let found = remote.find("<a@example.org>");
        assert_eq!(found, [current[0].clone()]);
        assert!(found[0].ends_with(":Archive"));
        // Read state rode along untouched; the trash's \Deleted was cleared on
        // the way out and a later return to the trash does not set it again.
        let (uid, _) = message_id(&current[0]).unwrap();
        assert!(remote.boxes["Archive"][&uid].seen);
        assert!(
            remote
                .boxes
                .values()
                .flat_map(BTreeMap::values)
                .all(|message| !message.deleted)
        );
    }
}

#[test]
fn copyuid_pairs_follow_set_order_and_only_asked_uids() {
    let data =
        b"* OK [COPYUID 7 3:5,9 40:42,50] Moved\r\n* 1 EXPUNGE\r\nO1 OK [copyuid 7 11 60] Done\r\n";
    assert_eq!(
        copied(data, &[3, 4, 5, 9, 11]),
        BTreeMap::from([(3, 40), (4, 41), (5, 42), (9, 50), (11, 60)])
    );
    assert_eq!(copied(data, &[4]), BTreeMap::new(), "longer than asked");
    assert_eq!(
        copied(b"O1 OK [COPYUID 7 1:2 40] Done\r\n", &[1, 2]),
        BTreeMap::new()
    );
    assert_eq!(
        copied(b"O1 OK [COPYUID 7 1:4294967295 1:4294967295]\r\n", &[1]),
        BTreeMap::new()
    );
    assert_eq!(uid_set("5:3", 3), Some(vec![3, 4, 5]));
    assert_eq!(uid_set("0", 3), None);
    assert_eq!(uid_set("1,x", 3), None);
}

#[test]
fn fetched_reads_message_id_from_the_peeked_header_only() {
    let header = b"Message-ID:\r\n <a@example.org>\r\n\r\n";
    let data = [
        format!(
            "* 1 FETCH (UID 7 FLAGS (\\Seen \\Deleted) BODY[HEADER.FIELDS (MESSAGE-ID)] {{{}}}\r\n",
            header.len()
        )
        .as_bytes(),
        header,
        b")\r\n* 2 FETCH (FLAGS (\\Seen) UID 8)\r\n* 3 FETCH (UID 9 BODY[HEADER.FIELDS (MESSAGE-ID)] \"\\r\\n\")\r\nO1 OK done\r\n",
    ]
    .concat();
    let facts = fetched(&data).unwrap();
    assert_eq!(
        facts,
        BTreeMap::from([
            (
                7,
                Fetched {
                    message_id: "<a@example.org>".into(),
                    deleted: true
                }
            ),
            (9, Fetched::default()),
        ])
    );
}

// The real conversation, against a scripted peer.

fn params(port: u16) -> Value {
    json!({"settings":{"imapHost":"127.0.0.1","imapPort":port,"username":"synthetic","insecure":true,"testPlaintext":true},"credential":"synthetic:password","oauth":false})
}

const LIST: &str = "* LIST () \"/\" INBOX\r\n* LIST (\\Archive) \"/\" Archive\r\n* LIST (\\Trash) \"/\" Trash\r\n* LIST (\\Noselect) \"/\" Lists\r\nO1 OK listed\r\n";

fn header_fetch(uid: u32, message_id: &str) -> String {
    let header = format!("Message-ID: {message_id}\r\n\r\n");
    format!(
        "* 1 FETCH (UID {uid} FLAGS (\\Seen) BODY[HEADER.FIELDS (MESSAGE-ID)] {{{}}}\r\n{header})\r\nO1 OK fetched\r\n",
        header.len()
    )
}

/// One connection: greeting, login and the capability check, then each
/// command in order with its scripted reply.
fn peer(
    listener: TcpListener,
    capabilities: &'static str,
    steps: Vec<(String, String)>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut w: Wire = BufReader::new(Box::new(socket));
        write(&mut w, b"* OK ready\r\n").await.unwrap();
        assert!(line(&mut w).await.unwrap().starts_with(b"O1 LOGIN"));
        write(&mut w, b"O1 OK login\r\n").await.unwrap();
        let capability = format!("* CAPABILITY {capabilities}\r\nO1 OK capability\r\n");
        let mut steps = steps;
        steps.insert(0, ("CAPABILITY".into(), capability));
        for (wanted, reply) in steps {
            let got = String::from_utf8(line(&mut w).await.unwrap()).unwrap();
            assert_eq!(got, format!("O1 {wanted}\r\n"));
            write(&mut w, reply.as_bytes()).await.unwrap();
        }
    })
}

fn discover(capabilities: &str) -> Vec<(String, String)> {
    vec![
        (
            "CAPABILITY".into(),
            format!("* CAPABILITY {capabilities}\r\nO1 OK capability\r\n"),
        ),
        ("LIST \"\" \"*\"".into(), LIST.into()),
    ]
}

fn step(command: &str, reply: &str) -> (String, String) {
    (command.to_owned(), reply.to_owned())
}

async fn wire_place(port: u16, items: &[PlaceItem]) -> Vec<PlaceOutcome> {
    let mut answers = vec![None; items.len()];
    let ended = place_params(&params(port), items, &mut answers).await;
    settle(answers, ended).unwrap()
}

#[tokio::test]
async fn wire_locate_examines_and_peeks() {
    let (listener, port) = super::super::tests::server().await;
    let server = peer(
        listener,
        "IMAP4rev1",
        vec![
            step("EXAMINE \"Archive\"", "O1 OK [READ-ONLY] examined\r\n"),
            (
                "UID FETCH 12,13 (UID FLAGS BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])".into(),
                header_fetch(12, "<a@example.org>"),
            ),
        ],
    );
    let wanted = ids(&["12:Archive", "13:Archive"]);
    let mut answers = vec![None; 2];
    let ended = locate_params(&params(port), &wanted, &mut answers).await;
    assert_eq!(
        settle(answers, ended).unwrap(),
        vec![Ok(place_of("Archive", "<a@example.org>")), Err(LOST)]
    );
    server.await.unwrap();
}

#[tokio::test]
async fn wire_place_moves_and_reads_untagged_copyuid() {
    let (listener, port) = super::super::tests::server().await;
    let caps = "IMAP4rev1 MOVE UIDPLUS";
    let mut steps = discover(caps);
    steps.extend([
        step("SELECT \"INBOX\"", "O1 OK selected\r\n"),
        (
            "UID FETCH 7 (UID FLAGS BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])".into(),
            header_fetch(7, "<a@example.org>"),
        ),
        step(
            "UID MOVE 7 \"Archive\"",
            "* OK [COPYUID 99 7 41] Moved\r\n* 1 EXPUNGE\r\nO1 OK done\r\n",
        ),
    ]);
    let server = peer(listener, caps, steps);
    let outcomes = wire_place(
        port,
        &[item("7:INBOX", place_of("Archive", "<a@example.org>"))],
    )
    .await;
    assert_eq!(outcomes, vec![moved("41:Archive")]);
    server.await.unwrap();
}

#[tokio::test]
async fn wire_place_without_move_copies_flags_and_expunges_its_own_uids() {
    let (listener, port) = super::super::tests::server().await;
    let caps = "IMAP4rev1 UIDPLUS";
    let mut steps = discover(caps);
    steps.extend([
        step("SELECT \"Trash\"", "O1 OK selected\r\n"),
        (
            "UID FETCH 3 (UID FLAGS BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])".into(),
            "* 1 FETCH (UID 3 FLAGS (\\Deleted) BODY[HEADER.FIELDS (MESSAGE-ID)] \"Message-ID: <t@example.org>\")\r\nO1 OK fetched\r\n".into(),
        ),
        step("UID STORE 3 -FLAGS.SILENT (\\Deleted)", "O1 OK stored\r\n"),
        step("UID COPY 3 \"INBOX\"", "O1 OK [COPYUID 99 3 8] Copied\r\n"),
        step("UID STORE 3 +FLAGS.SILENT (\\Deleted)", "O1 OK stored\r\n"),
        step("UID EXPUNGE 3", "* 1 EXPUNGE\r\nO1 OK expunged\r\n"),
    ]);
    let server = peer(listener, caps, steps);
    let outcomes = wire_place(
        port,
        &[item("3:Trash", place_of("INBOX", "<t@example.org>"))],
    )
    .await;
    assert_eq!(outcomes, vec![moved("8:INBOX")]);
    server.await.unwrap();
}

#[tokio::test]
async fn wire_place_without_uidplus_searches_from_uidnext() {
    let (listener, port) = super::super::tests::server().await;
    let caps = "IMAP4rev1 MOVE";
    let mut steps = discover(caps);
    steps.extend([
        step("SELECT \"INBOX\"", "O1 OK selected\r\n"),
        (
            "UID FETCH 7 (UID FLAGS BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])".into(),
            header_fetch(7, "<a\"b@example.org>"),
        ),
        step(
            "STATUS \"Archive\" (UIDNEXT)",
            "* STATUS Archive (UIDNEXT 50)\r\nO1 OK status\r\n",
        ),
        step("UID MOVE 7 \"Archive\"", "* 1 EXPUNGE\r\nO1 OK done\r\n"),
        step("EXAMINE \"Archive\"", "O1 OK examined\r\n"),
        step(
            "UID SEARCH UID 50:* HEADER MESSAGE-ID \"<a\\\"b@example.org>\"",
            "* SEARCH 49 52\r\nO1 OK searched\r\n",
        ),
    ]);
    let server = peer(listener, caps, steps);
    let outcomes = wire_place(
        port,
        &[item("7:INBOX", place_of("Archive", "<a\"b@example.org>"))],
    )
    .await;
    assert_eq!(outcomes, vec![moved("52:Archive")]);
    server.await.unwrap();
}

#[tokio::test]
async fn wire_place_into_an_unselectable_folder_is_gone() {
    let (listener, port) = super::super::tests::server().await;
    let caps = "IMAP4rev1 MOVE UIDPLUS";
    let server = peer(listener, caps, discover(caps));
    let outcomes = wire_place(port, &[item("7:INBOX", place_of("Lists", ""))]).await;
    assert_eq!(outcomes, vec![Err(GONE)]);
    server.await.unwrap();
}
