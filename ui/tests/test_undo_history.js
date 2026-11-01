const assert = require("assert")
const { load, deepEqual } = require("./load")

const history = load("account/UndoHistory.js")

function step(label, ids) {
  return { accountId: "a", label: label,
    items: ids.map(function(id) { return { id: id, place: { labels: ["INBOX"] } } }) }
}

// Moves and deletes are kept; read and star are not.
for (const verb of ["archive", "unarchive", "trash", "untrash", "spam", "label:Label_7"])
  assert.strictEqual(history.recordsMove(verb), true, verb)
for (const verb of ["markRead", "markUnread", "star", "unstar", "label:", ""])
  assert.strictEqual(history.recordsMove(verb), false, verb)

// A new action is the newest step and clears what had been taken back.
let h = history.empty()
assert.strictEqual(history.canUndo(h), false)
h = history.record(h, step("Archived", ["m1"]))
h = history.record(h, step("Moved to trash", ["m2"]))
assert.strictEqual(history.nextUndo(h).label, "Moved to trash")
h = history.undone(h, step("Moved to trash", ["m2"]))
assert.strictEqual(history.nextUndo(h).label, "Archived")
assert.strictEqual(history.nextRedo(h).label, "Moved to trash")
h = history.record(h, step("Reported as spam", ["m3"]))
assert.strictEqual(history.canRedo(h), false, "a new action drops the redo steps")

// A step with nothing in it is not kept.
assert.strictEqual(history.record(h, step("Archived", [])), h)
assert.strictEqual(history.record(h, { accountId: "", label: "x", items: [{ id: "m", place: {} }] }), h)

// A hundred steps, the oldest dropped first.
h = history.empty()
for (let i = 0; i < 130; i++) h = history.record(h, step("s" + i, ["m" + i]))
assert.strictEqual(h.undo.length, 100)
assert.strictEqual(h.undo[0].label, "s30")
assert.strictEqual(history.nextUndo(h).label, "s129")
for (let i = 0; i < 120; i++) h = history.undone(h, step("r" + i, ["m" + i]))
assert.strictEqual(h.undo.length, 0)
assert.strictEqual(h.redo.length, 100, "redo is bounded too")

// Undo and redo hand the step across, and a fully failed one is not kept.
h = history.record(history.empty(), step("Archived", ["m1"]))
h = history.undone(h, { accountId: "a", label: "Archived", items: [] })
assert.strictEqual(history.canUndo(h), false)
assert.strictEqual(history.canRedo(h), false)
h = history.record(history.empty(), step("Archived", ["m1"]))
h = history.undone(h, step("Archived", ["m1"]))
h = history.redone(h, step("Archived", ["m1"]))
assert.strictEqual(history.canRedo(h), false)
assert.strictEqual(history.nextUndo(h).label, "Archived")

// The reverse of a placed step: new ids, paired with where each message was
// just before; failed items drop out.
const placed = { accountId: "a", label: "Archived",
  items: [{ id: "1:INBOX", place: { folder: "INBOX" } }, { id: "2:INBOX", place: { folder: "INBOX" } },
    { id: "3:INBOX", place: { folder: "INBOX" } }] }
const before = [{ id: "1:INBOX", place: { folder: "Archive" } }, { id: "2:INBOX", place: { folder: "Archive" } },
  { id: "3:INBOX", error: "undo_message_lost" }]
const results = [{ id: "1:INBOX", newId: "41:INBOX" }, { id: "2:INBOX", error: "undo_folder_gone" },
  { id: "3:INBOX", newId: "43:INBOX" }]
const reverse = history.reverseOf(placed, before, results)
deepEqual(reverse.items, [{ id: "41:INBOX", place: { folder: "Archive" } }])
assert.strictEqual(history.failures(results), 1)
deepEqual(history.reverseOf(placed, [{ id: "other", place: {} }], results).items, [],
  "an answer for another id is never paired")

// The items a new step keeps: located before, and not among the failures.
deepEqual(history.stepItems([{ id: "a", place: { labels: ["INBOX"] } },
  { id: "b", error: "x" }, { id: "c", place: { labels: [] } }], ["c"]),
  [{ id: "a", place: { labels: ["INBOX"] } }])

// A removed mailbox takes its steps with it.
h = history.record(history.empty(), step("Archived", ["m1"]))
h = history.record(h, { accountId: "b", label: "Archived", items: [{ id: "x", place: {} }] })
h = history.withoutAccount(h, "a")
assert.strictEqual(h.undo.length, 1)
assert.strictEqual(h.undo[0].accountId, "b")

console.log("test_undo_history.js ok")
