//! Undo history: where messages are now (`mail.locate`) and putting them back
//! (`mail.place`). A place is an opaque, provider-specific object; this module
//! owns the provider-neutral request shapes and checks, and each provider owns
//! its place: `validate_place`, `locate` and `place` in
//! `providers::gmail::place`, `providers::imap::place`, `providers::jmap::place`,
//! `providers::m365::place` and `providers::hey_place`.
use super::types::opaque_id;
use super::{Account, Provider};
use serde_json::{Map, Value, json};
use std::{collections::HashSet, future::Future, pin::Pin};

const MAX_ITEMS: usize = 1_000;
const MAX_ACCOUNT: usize = 8192;
/// A place arrives from the UI: its serialized size is bounded before a
/// provider reads its shape.
pub(crate) const MAX_PLACE_BYTES: usize = 64 * 1024;

/// One message's place now, or the error code that kept it unread.
pub(crate) type Located = Result<Value, &'static str>;

/// A message put at its place. `new_id` is its id afterwards (the same id on
/// providers with stable ids); `moved` is false when it was already there and
/// nothing was written.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Placed {
    pub new_id: String,
    pub moved: bool,
}

pub(crate) type PlaceOutcome = Result<Placed, &'static str>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PlaceItem {
    pub id: String,
    pub place: Value,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct LocateRequest {
    pub account: Account,
    /// Unique, validated ids in first-request order.
    pub ids: Vec<String>,
    /// The request's ids as sent, repeats included: one answer each.
    pub requested: Vec<String>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct PlaceRequest {
    pub account: Account,
    pub items: Vec<PlaceItem>,
}

type Answer<'a, T> = Pin<Box<dyn Future<Output = Result<Vec<T>, &'static str>> + Send + 'a>>;

/// The per-provider halves. A provider is handed validated, unique ids (and,
/// for `place`, places that passed its own `validate_place`) and answers one
/// entry per input, in input order. A whole-call `Err` means nothing more
/// specific can be said for any message.
pub(crate) trait Places: Send + Sync {
    fn locate<'a>(&'a self, account: &'a Account, ids: &'a [String]) -> Answer<'a, Located>;
    fn place<'a>(
        &'a self,
        account: &'a Account,
        items: &'a [PlaceItem],
    ) -> Answer<'a, PlaceOutcome>;
}

/// The exact shape of a provider's place. Every provider refuses unknown keys
/// and out-of-range values rather than guessing what a damaged place meant.
pub(crate) fn validate_place(provider: Provider, place: &Value) -> Result<(), &'static str> {
    if !place.is_object()
        || serde_json::to_vec(place).map_or(true, |bytes| bytes.len() > MAX_PLACE_BYTES)
    {
        return Err("invalid_params");
    }
    match provider {
        Provider::Gmail => crate::providers::gmail::place::validate_place(place),
        Provider::Imap | Provider::Outlook => crate::providers::imap::place::validate_place(place),
        Provider::Jmap => crate::providers::jmap::place::validate_place(place),
        Provider::M365 => crate::providers::m365::place::validate_place(place),
        Provider::Hey => crate::providers::hey_place::validate_place(place),
    }
}

fn params_object<'a>(
    value: &'a Value,
    fields: &[&str],
) -> Result<&'a Map<String, Value>, &'static str> {
    let object = value.as_object().ok_or("invalid_params")?;
    if object.keys().any(|key| !fields.contains(&key.as_str()))
        || fields.iter().any(|field| !object.contains_key(*field))
    {
        return Err("invalid_params");
    }
    Ok(object)
}

/// `accountId` is required: an undo step never falls back to the active account.
fn account(
    object: &Map<String, Value>,
    resolve: impl FnOnce(&str) -> Result<Account, &'static str>,
) -> Result<Account, &'static str> {
    let id = object["accountId"].as_str().ok_or("invalid_params")?;
    if id.trim().is_empty() || id.len() > MAX_ACCOUNT || id.chars().any(char::is_control) {
        return Err("invalid_params");
    }
    resolve(id)
}

fn message_id(provider: Provider, value: &Value) -> Result<String, &'static str> {
    let id = opaque_id(value.as_str().ok_or("invalid_params")?)?;
    super::action::validate_message_id(provider, &id)?;
    Ok(id)
}

pub(crate) fn locate_request(
    params: &Value,
    resolve: impl FnOnce(&str) -> Result<Account, &'static str>,
) -> Result<LocateRequest, &'static str> {
    let object = params_object(params, &["accountId", "ids"])?;
    let values = object["ids"].as_array().ok_or("invalid_params")?;
    if values.is_empty() || values.len() > MAX_ITEMS {
        return Err("invalid_params");
    }
    let account = account(object, resolve)?;
    let requested = values
        .iter()
        .map(|value| message_id(account.provider, value))
        .collect::<Result<Vec<_>, _>>()?;
    let mut seen = HashSet::new();
    let ids = requested
        .iter()
        .filter(|id| seen.insert(id.as_str()))
        .cloned()
        .collect();
    Ok(LocateRequest {
        account,
        ids,
        requested,
    })
}

/// Every item is checked before any provider is asked: a malformed later
/// place must never let an earlier message move.
pub(crate) fn place_request(
    params: &Value,
    resolve: impl FnOnce(&str) -> Result<Account, &'static str>,
) -> Result<PlaceRequest, &'static str> {
    let object = params_object(params, &["accountId", "items"])?;
    let values = object["items"].as_array().ok_or("invalid_params")?;
    if values.is_empty() || values.len() > MAX_ITEMS {
        return Err("invalid_params");
    }
    let account = account(object, resolve)?;
    let mut seen = HashSet::new();
    let mut items = Vec::with_capacity(values.len());
    for value in values {
        let item = params_object(value, &["id", "place"])?;
        let id = message_id(account.provider, &item["id"])?;
        // One message, one destination: a repeat could only mean two places.
        if !seen.insert(id.clone()) {
            return Err("invalid_params");
        }
        validate_place(account.provider, &item["place"])?;
        items.push(PlaceItem {
            id,
            place: item["place"].clone(),
        });
    }
    Ok(PlaceRequest { account, items })
}

pub(crate) async fn locate_with(
    request: &LocateRequest,
    places: &impl Places,
) -> Result<Value, &'static str> {
    let located = places.locate(&request.account, &request.ids).await?;
    if located.len() != request.ids.len() {
        return Err("mail_place_invalid_response");
    }
    let rows: Vec<Value> = request
        .requested
        .iter()
        .map(|id| {
            let index = request.ids.iter().position(|unique| unique == id);
            // Whatever locate says must be something place accepts.
            match index.map(|index| &located[index]) {
                Some(Ok(place)) => match validate_place(request.account.provider, place) {
                    Ok(()) => json!({"id":id,"place":place}),
                    Err(_) => json!({"id":id,"error":"mail_place_invalid_response"}),
                },
                Some(Err(code)) => json!({"id":id,"error":code}),
                None => json!({"id":id,"error":"mail_place_invalid_response"}),
            }
        })
        .collect();
    Ok(json!({"places":rows}))
}

/// Answers the result and whether caches may now be stale. Any item that did
/// not end as a plain no-op may have written something.
pub(crate) async fn place_with(
    request: &PlaceRequest,
    places: &impl Places,
) -> Result<(Value, bool), &'static str> {
    let outcomes = places.place(&request.account, &request.items).await?;
    if outcomes.len() != request.items.len() {
        return Err("mail_place_invalid_response");
    }
    let mut stale = false;
    let rows: Vec<Value> = request
        .items
        .iter()
        .zip(outcomes)
        .map(|(item, outcome)| match outcome {
            Ok(placed) => {
                stale |= placed.moved;
                match super::action::validate_message_id(request.account.provider, &placed.new_id) {
                    Ok(()) => json!({"id":item.id,"newId":placed.new_id}),
                    Err(_) => json!({"id":item.id,"error":"mail_place_invalid_response"}),
                }
            }
            Err(code) => {
                stale = true;
                json!({"id":item.id,"error":code})
            }
        })
        .collect();
    Ok((json!({"results":rows}), stale))
}
