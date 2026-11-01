use super::place::{
    Located, PlaceItem, PlaceOutcome, Placed, Places, locate_request, locate_with, place_request,
    place_with, validate_place,
};
use super::{Account, Provider};
use serde_json::json;
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

fn gmail(id: &str) -> Result<Account, &'static str> {
    Ok(Account {
        id: id.to_lowercase(),
        provider: Provider::Gmail,
    })
}

fn unknown(_: &str) -> Result<Account, &'static str> {
    Err("mail_account_unknown")
}

#[test]
fn locate_requests_are_checked_before_any_provider_is_asked() {
    for params in [
        json!([]),
        json!({"accountId":"me@example.org"}),
        json!({"ids":["m1"]}),
        json!({"accountId":"me@example.org","ids":["m1"],"execute":true}),
        json!({"accountId":"","ids":["m1"]}),
        json!({"accountId":"me\n@example.org","ids":["m1"]}),
        json!({"accountId":7,"ids":["m1"]}),
        json!({"accountId":"me@example.org","ids":[]}),
        json!({"accountId":"me@example.org","ids":"m1"}),
        json!({"accountId":"me@example.org","ids":[1]}),
        json!({"accountId":"me@example.org","ids":[".."]}),
        json!({"accountId":"me@example.org","ids":["a\u{202e}b"]}),
        json!({"accountId":"me@example.org","ids":vec!["m1"; 1_001]}),
    ] {
        assert_eq!(
            locate_request(&params, gmail),
            Err("invalid_params"),
            "{params}"
        );
    }
    assert_eq!(
        locate_request(&json!({"accountId":"x@example.org","ids":["m1"]}), unknown),
        Err("mail_account_unknown")
    );
}

#[test]
fn a_repeated_id_is_located_once_and_answered_each_time() {
    let request = locate_request(
        &json!({"accountId":"Me@Example.org","ids":["m1","m2","m1"]}),
        gmail,
    )
    .unwrap();
    assert_eq!(request.account.id, "me@example.org");
    assert_eq!(request.ids, ["m1", "m2"]);
    assert_eq!(request.requested, ["m1", "m2", "m1"]);
}

#[test]
fn place_requests_refuse_every_malformed_item_before_any_move() {
    let good = json!({"id":"m1","place":{"labels":["INBOX"]}});
    for items in [
        json!([]),
        json!("m1"),
        json!([{"id":"m1"}]),
        json!([{"place":{"labels":[]}}]),
        json!([{"id":"m1","place":{"labels":[]},"extra":1}]),
        json!([{"id":"..","place":{"labels":[]}}]),
        json!([good, {"id":"m1","place":{"labels":[]}}]),
        json!([good, {"id":"m2","place":["INBOX"]}]),
        json!([good, {"id":"m2","place":null}]),
        json!([good, {"id":"m2","place":{"labels":["UNREAD"]}}]),
        json!([good, {"id":"m2","place":{"labels":["INBOX"],"folder":"x"}}]),
        json!(
            (0..1_001)
                .map(|n| json!({"id":format!("m{n}"),"place":{"labels":[]}}))
                .collect::<Vec<_>>()
        ),
    ] {
        let params = json!({"accountId":"me@example.org","items":items});
        assert_eq!(
            place_request(&params, gmail),
            Err("invalid_params"),
            "{params}"
        );
    }
    for params in [
        json!({"accountId":"me@example.org"}),
        json!({"accountId":"me@example.org","items":[good],"ids":[]}),
    ] {
        assert_eq!(place_request(&params, gmail), Err("invalid_params"));
    }
    let request = place_request(
        &json!({"accountId":"me@example.org","items":[good,{"id":"m2","place":{"labels":[]}}]}),
        gmail,
    )
    .unwrap();
    assert_eq!(
        request.items,
        [
            PlaceItem {
                id: "m1".into(),
                place: json!({"labels":["INBOX"]})
            },
            PlaceItem {
                id: "m2".into(),
                place: json!({"labels":[]})
            }
        ]
    );
}

#[test]
fn a_place_is_an_object_of_bounded_size_for_every_provider() {
    let huge = json!({"labels":["x".repeat(super::place::MAX_PLACE_BYTES)]});
    for provider in [
        Provider::Gmail,
        Provider::Imap,
        Provider::Outlook,
        Provider::Jmap,
        Provider::M365,
        Provider::Hey,
    ] {
        for place in [json!(null), json!([]), json!("INBOX"), huge.clone()] {
            assert_eq!(validate_place(provider, &place), Err("invalid_params"));
        }
    }
}

type Answer<'a, T> = Pin<Box<dyn Future<Output = Result<Vec<T>, &'static str>> + Send + 'a>>;

struct Scripted {
    located: Mutex<Option<Result<Vec<Located>, &'static str>>>,
    placed: Mutex<Option<Result<Vec<PlaceOutcome>, &'static str>>>,
    asked: Mutex<Vec<Vec<String>>>,
    calls: AtomicUsize,
}

fn scripted(
    located: Result<Vec<Located>, &'static str>,
    placed: Result<Vec<PlaceOutcome>, &'static str>,
) -> Scripted {
    Scripted {
        located: Mutex::new(Some(located)),
        placed: Mutex::new(Some(placed)),
        asked: Mutex::default(),
        calls: AtomicUsize::new(0),
    }
}

impl Places for Scripted {
    fn locate<'a>(&'a self, _account: &'a Account, ids: &'a [String]) -> Answer<'a, Located> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.asked.lock().unwrap().push(ids.to_vec());
        let answer = self.located.lock().unwrap().take().unwrap();
        Box::pin(async move { answer })
    }

    fn place<'a>(
        &'a self,
        _account: &'a Account,
        items: &'a [PlaceItem],
    ) -> Answer<'a, PlaceOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.asked
            .lock()
            .unwrap()
            .push(items.iter().map(|item| item.id.clone()).collect());
        let answer = self.placed.lock().unwrap().take().unwrap();
        Box::pin(async move { answer })
    }
}

fn placed(id: &str, moved: bool) -> PlaceOutcome {
    Ok(Placed {
        new_id: id.into(),
        moved,
    })
}

#[tokio::test]
async fn locate_answers_one_row_per_requested_id_in_order() {
    let request = locate_request(
        &json!({"accountId":"me@example.org","ids":["m1","m2","m3","m1"]}),
        gmail,
    )
    .unwrap();
    let places = scripted(
        Ok(vec![
            Ok(json!({"labels":["INBOX"]})),
            Err("gmail_http_failed"),
            // A place the provider itself would refuse is never handed out.
            Ok(json!({"labels":["UNREAD"]})),
        ]),
        Ok(vec![]),
    );
    assert_eq!(
        locate_with(&request, &places).await,
        Ok(json!({"places":[
            {"id":"m1","place":{"labels":["INBOX"]}},
            {"id":"m2","error":"gmail_http_failed"},
            {"id":"m3","error":"mail_place_invalid_response"},
            {"id":"m1","place":{"labels":["INBOX"]}},
        ]}))
    );
    assert_eq!(*places.asked.lock().unwrap(), [vec!["m1", "m2", "m3"]]);
}

#[tokio::test]
async fn provider_answers_of_the_wrong_length_are_refused_whole() {
    let request = locate_request(
        &json!({"accountId":"me@example.org","ids":["m1","m2"]}),
        gmail,
    )
    .unwrap();
    let places = scripted(Ok(vec![Ok(json!({"labels":[]}))]), Ok(vec![]));
    assert_eq!(
        locate_with(&request, &places).await,
        Err("mail_place_invalid_response")
    );
    let request = place_request(
        &json!({"accountId":"me@example.org","items":[{"id":"m1","place":{"labels":[]}}]}),
        gmail,
    )
    .unwrap();
    let places = scripted(Ok(vec![]), Ok(vec![]));
    assert_eq!(
        place_with(&request, &places).await,
        Err("mail_place_invalid_response")
    );
    let places = scripted(Ok(vec![]), Err("undo_unsupported"));
    assert_eq!(place_with(&request, &places).await, Err("undo_unsupported"));
}

#[tokio::test]
async fn place_answers_new_ids_and_says_when_caches_are_stale() {
    let params = json!({"accountId":"me@example.org","items":[
        {"id":"m1","place":{"labels":["INBOX"]}},
        {"id":"m2","place":{"labels":[]}},
        {"id":"m3","place":{"labels":["TRASH"]}},
    ]});
    let request = place_request(&params, gmail).unwrap();
    let places = scripted(
        Ok(vec![]),
        Ok(vec![
            placed("m1", false),
            placed("m2", false),
            placed("..", true),
        ]),
    );
    let (result, stale) = place_with(&request, &places).await.unwrap();
    assert_eq!(
        result,
        json!({"results":[
            {"id":"m1","newId":"m1"},
            {"id":"m2","newId":"m2"},
            {"id":"m3","error":"mail_place_invalid_response"},
        ]})
    );
    assert!(stale);
    let quiet = scripted(
        Ok(vec![]),
        Ok(vec![
            placed("m1", false),
            placed("m2", false),
            placed("m3", false),
        ]),
    );
    assert!(!place_with(&request, &quiet).await.unwrap().1);
    let failed = scripted(
        Ok(vec![]),
        Ok(vec![
            placed("m1", false),
            Err("gmail_forbidden"),
            placed("m3", false),
        ]),
    );
    let (result, stale) = place_with(&request, &failed).await.unwrap();
    assert_eq!(
        result["results"][1],
        json!({"id":"m2","error":"gmail_forbidden"})
    );
    assert!(stale);
}
