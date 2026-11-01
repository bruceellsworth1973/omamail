.pragma library

// The moves and deletes that can be taken back, newest last, and the ones taken
// back that can be done again. A step is `{accountId, label, items}`, one item
// per message: `{id, place}` — the message's id now and the place `mail.locate`
// reported for it before the step moved it. Taking a step back or doing it again
// is the same act, a `mail.place` to the items' places, so both lists hold the
// same shape and each turns into the other.

var LIMIT = 100

function empty() {
  return { undo: [], redo: [] }
}

function bounded(list, limit) {
  var cap = Number(limit) > 0 ? Math.floor(Number(limit)) : LIMIT
  return list.length > cap ? list.slice(list.length - cap) : list
}

function validStep(step) {
  return !!step && String(step.accountId || "") !== "" && Array.isArray(step.items)
    && step.items.length > 0
}

// A new action. What had been taken back is gone: doing it again would now
// move messages out from under the action that followed.
function record(history, step, limit) {
  var current = history || empty()
  if (!validStep(step)) return current
  return { undo: bounded(current.undo.concat([step]), limit), redo: [] }
}

function canUndo(history) { return !!history && history.undo.length > 0 }
function canRedo(history) { return !!history && history.redo.length > 0 }

function nextUndo(history) { return canUndo(history) ? history.undo[history.undo.length - 1] : null }
function nextRedo(history) { return canRedo(history) ? history.redo[history.redo.length - 1] : null }

// The step taken back has left `undo`; what it can be redone with — the
// messages' new ids and the places they were in just before — joins `redo`.
// An empty `reverse` (every message failed) is simply not kept.
function undone(history, reverse, limit) {
  var current = history || empty()
  var undo = current.undo.slice(0, Math.max(0, current.undo.length - 1))
  var redo = validStep(reverse) ? bounded(current.redo.concat([reverse]), limit) : current.redo
  return { undo: undo, redo: redo }
}

function redone(history, reverse, limit) {
  var current = history || empty()
  var redo = current.redo.slice(0, Math.max(0, current.redo.length - 1))
  var undo = validStep(reverse) ? bounded(current.undo.concat([reverse]), limit) : current.undo
  return { undo: undo, redo: redo }
}

// The reverse of a step once it has been placed: each message that moved, under
// its id afterwards, paired with where it was just before. `before` is the
// `mail.locate` answer for the step's ids, `results` the `mail.place` answer,
// both one entry per item in order.
function reverseOf(step, before, results) {
  var items = []
  var located = Array.isArray(before) ? before : []
  var placed = Array.isArray(results) ? results : []
  for (var i = 0; i < step.items.length; i++) {
    var was = located[i]
    var now = placed[i]
    if (!was || !now || now.error || was.error || !was.place) continue
    if (String(was.id || "") !== String(step.items[i].id)) continue
    if (String(now.id || "") !== String(step.items[i].id)) continue
    var id = String(now.newId || "")
    if (id === "") continue
    items.push({ id: id, place: was.place })
  }
  return { accountId: step.accountId, label: step.label, items: items }
}

// Items `mail.place` could not move, for the note.
function failures(results) {
  var placed = Array.isArray(results) ? results : []
  var count = 0
  for (var i = 0; i < placed.length; i++) if (!placed[i] || placed[i].error) count++
  return count
}

// A mailbox removed takes its steps with it: its ids mean nothing any more.
function withoutAccount(history, accountId) {
  var current = history || empty()
  var id = String(accountId || "")
  function keep(step) { return String(step.accountId) !== id }
  return { undo: current.undo.filter(keep), redo: current.redo.filter(keep) }
}

// The item list for a step from the places located before the action and the
// targets that did not fail.
function stepItems(located, failedTargets) {
  var failed = Array.isArray(failedTargets) ? failedTargets : []
  var places = Array.isArray(located) ? located : []
  var items = []
  for (var i = 0; i < places.length; i++) {
    var entry = places[i]
    if (!entry || entry.error || !entry.place) continue
    if (failed.indexOf(String(entry.id)) >= 0) continue
    items.push({ id: String(entry.id), place: entry.place })
  }
  return items
}

// The verbs that move or delete mail, the ones the history keeps. Read, star
// and their opposites change no place, and a place never carries them.
function recordsMove(action) {
  var verb = String(action || "")
  return ["archive", "unarchive", "trash", "untrash", "spam"].indexOf(verb) >= 0
    || (verb.indexOf("label:") === 0 && verb.length > "label:".length)
}
