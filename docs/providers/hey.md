# HEY integration

Required when touching HEY authentication, fetching, actions or presentation.

- Use the official HEY CLI (`hey`), never private `app.hey.com` endpoints. It owns OAuth, keyring storage and refresh; Omamail holds authentication status and the executable path, not credentials.
- `hey auth logout` signs the machine's CLI out for every consumer. Explain this beside sign-out; local forgetting alone would be undone by the next status read.
- `HeyCli.js` owns argument vectors and JSON interpretation, not processes or message formatting. Request `--json` and check the envelope's `ok`; a refusal can still exit 0.
- Message IDs are `<posting>:<topic>`. `seen`, `move`, `trash` and `spam` take the posting ID; `threads` and `reply` take the topic ID.
- Missing `seen` means unseen. There is no unread count or unread box; compute the badge from a bounded listing.
- `--limit` truncates and drops the cursor. Ordinary paged listings omit it; `HeyCli.pageOf` combines an offset within the CLI page with its cursor.
- Unsupported optional flags may be removed and retried, with the result remembered. Only boolean flags may use this fallback; removing a value-taking flag would leave a positional argument behind.
- HEY has neither star nor archive; do not emulate them by moving to Set Aside or Paper Trail.
- Write the brand as HEY; lowercase `hey` means the command. In prose call the program the HEY CLI.

## Undo history places

`mail.locate` and `mail.place` (see docs/BACKEND.md "Undo history") live in `src/providers/hey_place.rs`. Both first check, like `hey.act`, that the `hey` on the path is the one Omamail resolves and is signed in as the account.

- A place is `{"box": <kind>, "seen": <bool>}`, with `<kind>` one of `imbox`, `feedbox`, `trailbox`, `asidebox`, `laterbox`, `bubblebox` (the kinds `hey box` answers) or `trash`. `seen` is what the box listing said (a missing `seen` there means unseen). The Trash is read through a search that answers no seen state, so a Trash place is `{"box": "trash"}` alone. `seen` is optional, must be a boolean, and is refused beside `trash`; any other key or value (`spam`, a display name, a short name such as `feed`, a box id) is `invalid_params`. Posting ids do not change, so `newId` is always `id`.
- The CLI answers no box for a single posting. Locate reads `hey box <kind> --json` page by page (following `next_page`, at most 100 pages a box, the CLI's own `--all` bound) in the order above, then `hey search --in trash --json --page N`, and stops as soon as every posting asked about has been seen. A box whose answer names another kind, or a cursor starting with `-`, is `mail_place_invalid_response`. A posting found in none of them is `undo_unsupported`: Spam (no command lists it), the Screener, a thread inside a bundle row (the row's id is not the posting's), or a box beyond 100 pages. A listing that could not be read fails the postings it might have held with its own error.
- Place reads every posting's box the same way and writes only when it differs: `hey trash <postings> --json` into the Trash, and `hey move <postings> --to <kind> --json` into any other box, one command per destination. A posting already in its box is not written.
- `hey move` marks a thread seen when it lands in any box but the Imbox, and undo or redo must never change read state. After a move into a box, a thread is marked `hey unseen` again when it was unseen just before the move: as the box listing place just read showed it, or, coming out of the Trash (whose search shows no seen state), as the place's `seen: false` recorded it. `seen: true`, or no `seen` out of the Trash, writes nothing. When place's own listing showed the seen state, that current state wins over the place's, so a thread marked read or unread since the place was recorded keeps that state. A refused `hey unseen` is `hey_seen_not_restored` for that item, although the move was made. A posting already in its box is never written, whatever its `seen`.
- `hey move --to laterbox` skips a thread that cannot be replied to and still answers success, so a move into Reply Later is read back from Reply Later; one that did not land is `undo_unsupported`.

Exactly undoable, box and read state: any move between the Imbox, The Feed, Paper Trail, Set Aside, Reply Later and the Trash, in either direction. Not undoable or lossy:

- **Spam**: `hey spam` has no reverse and Spam cannot be listed, so a spammed thread has no place and cannot be put back (`undo_unsupported`). Redoing a spam is impossible too, since a place is never Spam.
- **Bubble Up**: it is a scheduled time that `hey move` refuses as a destination and `hey bubble up` would set to a new time; a place of `bubblebox` is answered only for a posting already there, and a move back into it is `undo_unsupported`. Moving a thread out of Bubble Up and back cannot be undone.
- **Out of the Trash without a recorded `seen`**: only a place made by hand (or by an older backend) can name a box without `seen`; a thread put back from the Trash by such a place into any box but the Imbox arrives seen.
- **Read state changed inside the Trash**: the Trash shows no seen state, so a thread marked read or unread while in the Trash comes back with the state its place recorded before it was trashed.
- **Set Aside groups**: a thread moved out of Set Aside and back returns to Set Aside outside its group.
- **Bundles**: a thread HEY shows only inside a bundle row cannot be located (`undo_unsupported`).
- Locate assumes a posting is listed in one box; it answers the first box (in the order above) that lists it.
