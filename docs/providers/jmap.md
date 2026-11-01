# JMAP discovery and streaming

Required when touching JMAP discovery, authentication or event streams.

- Start discovery at the address domain's HTTPS well-known URL or the explicit user-configured server. Unauthenticated DNS SRV answers cannot authorize credential destinations; probing the target over HTTPS proves its identity, not its relationship to the mailbox domain.
- The legacy `scripts/jmap-transport.sh` exchanges base64 stdin fields and a four-line response. Follow [security boundaries](../SECURITY-BOUNDARIES.md) for curl configuration.
- `scripts/jmap-stream.py` owns curl and bounds each event before forwarding it, normalizing CR, LF and CRLF to LF. Checking after QML's `SplitParser` is too late to bound buffering. Stopping or refusing the stream must terminate curl too.

## Undo history places

`src/providers/jmap/place.rs` owns JMAP's half of `mail.locate` and `mail.place` (see [BACKEND.md](../BACKEND.md) "Undo history").

- A place is `{"mailboxIds": {<id>: true, ...}, "keywords": {"$junk": bool, "$notjunk": bool}}`: every mailbox the email is in (1 to 256, each an RFC 8620 Id of 1 to 255 URL-safe base64 letters, each value `true`) and exactly those two keywords. Any other key, keyword or value is refused with `invalid_params`. `$seen`, `$flagged` and every other keyword are never read into a place or written. Email ids are stable, so `newId` is always the id.
- Locate reads `Email/get` (`id`, `threadId`, `mailboxIds`, `keywords`) live through the account's session, in `maxObjectsInGet` chunks four at a time, and matches keywords case-insensitively. An email the server no longer has is `jmap_message_not_found`; a row the request did not ask for, a repeated row or a malformed `mailboxIds` is `jmap_invalid_response`.
- Place reads the same properties live and compares. An email already there is answered without a write. For the others, every mailbox they go back to that they are not in now is checked with one live `Mailbox/get` (`properties: ["id"]`); a missing one is `undo_folder_gone` for that email and nothing is written for it. Then one `Email/set` per `maxObjectsInSet` chunk sends only what differs: `mailboxIds` replaced wholesale, and `keywords/$junk` and `keywords/$notjunk` set to `true` or `null`. Each email's outcome comes from `updated` or `notUpdated` (`notFound` is `jmap_message_not_found`, `forbidden` `jmap_forbidden`, anything else `jmap_update_failed`); an answer that does not name every id of its chunk exactly once, or a failed request, fails that chunk and every later one unsent, as the planned action path does. A write clears the session's retained rows like any action.

What cannot be put back exactly:

- An email destroyed since (an emptied Trash or Junk, an expiry rule) is `jmap_message_not_found`.
- A mailbox deleted since is `undo_folder_gone`, even if one with the same name and role was created again: JMAP gives it a new id.
- `mailboxIds` is replaced, not merged: mailboxes another client added or removed between the action and its undo are reverted too.
- Server side effects of the move are not undone. Servers that train their spam filter on `$junk` (Stalwart, Fastmail) learn the message again when its keywords are put back; undoing a spam action does not erase the first lesson. Servers that set or clear keywords themselves when mail enters or leaves Junk or Trash keep whatever they did to other keywords.
- Mailboxes the account may no longer write to (a shared mailbox whose rights changed) fail with the server's `notUpdated` error.
