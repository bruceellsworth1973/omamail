import QtQuick 2.15
import QtTest 1.3
import "../.." as Omamail
import "BackendFixture.js" as BackendFixture
import "ModelFixture.js" as NativeModel
import "NativeIntentFixture.js" as NativeIntentFixture
import "../../account/Accounts.js" as Accounts
import "../../account/Unified.js" as Unified

// The undo history of moves and deletes, through the real Service, App and
// MailAccount: a move asks the backend where its messages are first
// (`mail.locate`), and Super+Z / Super+Shift+Z and the two curved arrows beside
// refresh put them back (`mail.place`) or move them again. The fake backend
// keeps each message's place, and the recording client moves it on trash.
//
// The rest of this harness is tst_trash_restore.qml's:
// Restore in the trash and Move to Inbox in Archive, through the real Service,
// App and MailAccount, with the native intent engine deciding the optimistic
// change and a client the test controls recording what reaches the provider.
//
// HEY declares neither archive nor move, so a message in its Trash had no way
// out: the menu offered "Move to trash" again, `d` trashed it again, and the
// untrash every client implements was reachable from nothing. HEY's posting
// carries no TRASH label either, so the Trash listing itself says where it is.
//
// Move to Inbox in Archive comes from the same rule. Every control decides its
// verb over the rows its action will reach: a ticked row's menu or
// middle-click, `e` and `d` with rows ticked, and the reader on a ticked
// message all mean the selection, and the reader on a conversation member
// means the conversation's row.
Item {
  width: 900
  height: 600

  QtObject {
    id: shellStore
    function updateEntryInline(_id, _entry) {}
    function hide(_id) {}
  }

  QtObject {
    id: record
    property var trashed: []
    property var untrashed: []
    property var modified: []
    // Where the fake server holds each message: its location labels.
    property var where: ({})
    function reset() { trashed = []; untrashed = []; modified = []; where = ({}) }
    function put(id, labels) {
      var next = ({})
      for (var key in where) next[key] = where[key]
      next[id] = labels
      where = next
    }
  }

  Component {
    id: controlledClient
    QtObject {
      property var auth: null
      property string email: ""
      function handle() { return ({ aborted: false }) }
      function later(fn) { Qt.callLater(fn); return handle() }
      function trashMessage(id, callback) {
        record.trashed = record.trashed.concat([String(id)])
        record.put(String(id), ["TRASH"])
        return later(function() { callback(null, "") })
      }
      function untrashMessage(id, callback) {
        record.untrashed = record.untrashed.concat([String(id)])
        return later(function() { callback(null, "") })
      }
      function batchModify(ids, add, remove, callback) {
        record.modified = record.modified.concat([{ ids: ids.slice().sort().join(","),
          add: add.join(","), remove: remove.join(",") }])
        return later(function() { callback(null, "") })
      }
      function modifyMessage(id, add, remove, callback) { return batchModify([id], add, remove, callback) }
      function listMessages() { return handle() }
      function getMessages() { return handle() }
      function getMessage() { return handle() }
      function getSummaries() { return handle() }
      function getLabels(callback) { return later(function() { callback([], "") }) }
      function getLabelCounts(id, callback) { return later(function() { callback({ id: id, unread: 0, total: 0, threadsUnread: 0 }, "") }) }
      function getProfile(callback) { return later(function() { callback({ email: email }, "") }) }
      function getSendAs(callback) { return later(function() { callback([], "") }) }
      function getAttachment() { return handle() }
      function saveDraft() { return handle() }
      function deleteDraft() { return handle() }
      function sendMessage() { return handle() }
      function createLabel() { return handle() }
      function renameLabel() { return handle() }
      function deleteLabel() { return handle() }
      function abortRequest(h) { if (h) h.aborted = true }
    }
  }

  Omamail.Service {
    id: mailService
    shell: shellStore
    manifest: ({ id: "omamail", __sourceDir: "/tmp/omamail-test" })
  }

  Omamail.App { id: app; service: mailService }

  TestCase {
    name: "UndoMoves"
    when: windowShown

    property var fixture: null

    function requests(method) {
      return fixture.requests.filter(function(request) { return request.method === method })
    }

    function initTestCase() {
      // Every mailbox made here asks for its cached list. Unanswered, those
      // requests fill the backend's 64 slots and the actions under test wait
      // behind them.
      fixture = BackendFixture.markReady(mailService, 9)
      fixture.answers = {"cache.queryRestore": null,
        "mail.locate": function(params) {
          return { places: params.ids.map(function(id) {
            return { id: id, place: { labels: record.where[id] || ["INBOX"] } }
          }) }
        },
        "mail.place": function(params) {
          return { results: params.items.map(function(item) {
            record.put(item.id, item.place.labels)
            return { id: item.id, newId: item.id }
          }) }
        }}
      NativeModel.install(mailService)
      NativeIntentFixture.install(mailService)
      // The model fixture marks the backend ready at API 1; undo needs 9.
      BackendFixture.markReady(mailService, 9)
    }

    function entry(provider, email) {
      var value = { email: email, provider: provider, clientSecret: "", label: "", signature: "",
        clientId: provider === "outlook" ? "87654321-4321-4abc-9def-1234567890ab" : "" }
      if (provider === "imap") value.imap = { imapHost: "imap.example.com", imapPort: 993,
        smtpHost: "smtp.example.com", smtpPort: 465, username: email, aliases: [], insecure: false }
      if (provider === "jmap") value.jmap = { sessionUrl: "https://api.example.org/jmap/session",
        username: email, authScheme: "basic", accountId: "u1" }
      return value
    }

    // Ready an account the way each sign-in leaves it, with the recording client.
    function ready(account, provider) {
      account.clientOverride = controlledClient
      account.auth.toolsChecked = true
      account.auth.missingTools = []
      if (provider === "imap") {
        account.auth.passwordChecked = true
        account.auth.password = "test-password"
      } else if (provider === "jmap") {
        account.auth.secret = "app-password"
        account.auth.secretChecked = true
      } else if (provider === "gmail") {
        account.auth.credentials = ({ clientId: "123-test.apps.googleusercontent.com",
          clientSecret: "test", projectId: "test" })
        account.auth.refreshBusy = true
        account.auth.loggedIn = true
      } else {
        account.auth.loggedIn = true
      }
      tryCompare(account, "ready", true)
    }

    // Accounts on `mailboxKey`, holding `rows` each. A new address each time:
    // the native ledger is one process for the whole file, and an action a
    // replaced account left unsettled must not hold up the next one.
    property int seeded: 0
    function seedMany(providers, mailboxKey, rowsEach) {
      var list = Accounts.emptyList()
      var emails = []
      for (var i = 0; i < providers.length; i++) {
        seeded++
        emails.push("me" + seeded + "@example.com")
        list = Accounts.add(list, entry(providers[i], emails[i]))
      }
      mailService.setUnifiedMailboxes(false)
      mailService.activeIndex = -1
      mailService.accountList = list
      mailService.accountsLoaded = true
      wait(0)
      mailService.refreshCurrent()
      var accounts = []
      for (var h = 0; h < providers.length; h++) {
        var account = mailService.accountAt(h)
        verify(account !== null)
        ready(account, providers[h])
        // A host can outlive the account it held, so nothing a previous test
        // left on it is allowed to stand.
        account.searchQuery = ""
        account.rawQuery = ""
        account.rawLabelId = ""
        account.mailboxKey = mailboxKey
        account.messages = rowsEach[h]
        account.listLoaded = true
        accounts.push(account)
      }
      return accounts
    }

    function seed(provider, mailboxKey, rows) { return seedMany([provider], mailboxKey, [rows])[0] }

    // The same, merged: turning the merged view on moves every mailbox to the
    // merged view's mailbox, so the rows are put back after it has.
    function unified(providers, mailboxKey, rowsEach) {
      var accounts = seedMany(providers, mailboxKey, rowsEach)
      mailService.setUnifiedMailboxes(true)
      mailService.selectMailbox(mailboxKey)
      wait(0)
      for (var i = 0; i < accounts.length; i++) {
        accounts[i].mailboxKey = mailboxKey
        accounts[i].messages = rowsEach[i]
        accounts[i].listLoading = false
        accounts[i].listLoaded = true
      }
      tryVerify(function() { return mailService.messages.length === accounts.length })
      compare(mailService.mailboxKey, mailboxKey)
      return accounts
    }

    // A message with these labels, and the flags the summary derives from them.
    // A HEY posting in Trash has none: nothing in it says where it is.
    function message(id, labels) {
      var list = labels || []
      return ({ id: id, threadId: "", from: { email: "x@example.com", display: "X" }, subject: id,
        snippet: "", time: "", date: "", unread: false, starred: list.indexOf("STARRED") >= 0,
        inInbox: list.indexOf("INBOX") >= 0, inTrash: list.indexOf("TRASH") >= 0,
        inSpam: false, isSent: false, isDraft: false, labelIds: list })
    }

    function named(item, objectName) {
      if (!item) return null
      if (item.objectName === objectName) return item
      var values = item.children || []
      for (var i = 0; i < values.length; i++) {
        var found = named(values[i], objectName)
        if (found) return found
      }
      return null
    }

    function having(item, accept) {
      if (!item) return null
      if (accept(item)) return item
      var values = item.children || []
      for (var i = 0; i < values.length; i++) {
        var found = having(values[i], accept)
        if (found) return found
      }
      return null
    }

    // The archive or trash button under `item`, by the key its tooltip names
    // (`e`, `d`) rather than by a name the change gave it, so the same lookup
    // finds the button before the change and after.
    function slot(item, key) {
      return having(item, function(candidate) {
        return candidate.iconName !== undefined && candidate.visible
          && String(candidate.tooltipText || "").slice(-4) === " · " + key
      })
    }

    function reader() {
      var found = having(app, function(item) { return item.forceRichAnyway !== undefined })
      verify(found !== null)
      return found
    }

    // The row drawing `id`, found by the summary it was handed.
    function rowFor(id) {
      return having(app, function(item) {
        return item.summary !== undefined && item.trashRequested !== undefined
          && !!item.summary && item.summary.id === id
      })
    }

    // A popup's rows live under the overlay, not under the menu item, so they
    // are found in the list the menu's own cursor walks, by what they say.
    function menuRow(text) {
      var menu = named(app, "rowMenu")
      for (var i = 0; i < menu.menuRows.length; i++)
        if (menu.menuRows[i].visible && menu.menuRows[i].text === text) return menu.menuRows[i]
      return null
    }

    function ids(account) {
      return account.messages.map(function(message) { return message.id }).join(",")
    }

    function hintLabels() {
      var hints = named(app, "status-key-hints")
      verify(hints !== null)
      return hints.hints.map(function(hint) { return hint.key + " " + hint.label })
    }

    function tick(ids) {
      app.checkedAccountId = mailService.activeAccountId
      app.checkedIds = ids
      verify(app.selectionActive)
    }

    // The reader on `id`, in a window wide enough to keep the list beside it.
    function openReader(account, id, summary) {
      account.selectedId = id
      account.selectedMessage = summary
      app.pushEntry("reader", { id: id })
      wait(0)
      verify(!app.compact)
      verify(app.listOnScreen)
    }


    function init() {
      record.reset()
      fixture.requests = []
      mailService.moveHistory = ({ undo: [], redo: [] })
      app.checkedIds = []
      app.checkedAccountId = ""
      app.cursorId = ""
      app.resetNavigation()
      app.open()
    }

    function cleanup() {
      app.close()
    }

    function trashFirst() {
      var account = seed("gmail", "inbox", [message("1:2", ["INBOX"]), message("3:4", ["INBOX"])])
      app.cursorId = "1:2"
      app.runShortcut("trash", "d")
      tryCompare(record, "trashed", ["1:2"])
      tryCompare(mailService, "canUndoMove", true)
      return account
    }

    function test_a_move_is_located_first_and_recorded() {
      trashFirst()
      var located = requests("mail.locate")
      compare(located.length, 1, "asked where the message was before moving it")
      compare(located[0].params.ids, ["1:2"])
      compare(mailService.undoMoveLabel, "Moved to trash")
      compare(mailService.canRedoMove, false)
    }

    function test_super_z_puts_it_back_and_super_shift_z_moves_it_again() {
      trashFirst()
      app.runShortcut("undoMove", "Meta+Z")
      tryCompare(mailService, "canRedoMove", true)
      compare(record.where["1:2"], ["INBOX"], "back where it was")
      compare(mailService.canUndoMove, false)
      compare(mailService.redoMoveLabel, "Moved to trash")
      app.runShortcut("redoMove", "Meta+Shift+Z")
      tryCompare(mailService, "canUndoMove", true)
      compare(record.where["1:2"], ["TRASH"], "trashed again")
      compare(mailService.canRedoMove, false)
      compare(record.trashed, ["1:2"], "redo moved it through mail.place, not a second trash")
    }

    function test_a_new_action_drops_what_was_undone() {
      trashFirst()
      mailService.undoMove()
      tryCompare(mailService, "canRedoMove", true)
      app.cursorId = "3:4"
      app.runShortcut("trash", "d")
      tryCompare(record, "trashed", ["1:2", "3:4"])
      tryCompare(mailService, "undoMoveLabel", "Moved to trash")
      compare(mailService.canRedoMove, false)
    }

    function test_star_is_not_a_move() {
      seed("gmail", "inbox", [message("1:2", ["INBOX"])])
      app.cursorId = "1:2"
      app.runShortcut("star", "s")
      tryVerify(function() { return record.modified.length === 1 })
      compare(requests("mail.locate").length, 0)
      compare(mailService.canUndoMove, false)
    }

    function test_the_buttons_beside_refresh_follow_the_history() {
      seed("gmail", "inbox", [message("1:2", ["INBOX"])])
      var undo = named(app, "undo-move-button")
      var redo = named(app, "redo-move-button")
      verify(undo && redo)
      compare(undo.visible, true)
      compare(undo.enabled, false, "nothing to undo yet")
      compare(redo.enabled, false)
      app.cursorId = "1:2"
      app.runShortcut("trash", "d")
      tryCompare(undo, "enabled", true)
      compare(undo.tooltipText, "Undo: Moved to trash · Super+Z")
      undo.clicked()
      tryCompare(redo, "enabled", true)
      compare(undo.enabled, false)
      compare(redo.tooltipText, "Redo: Moved to trash · Super+Shift+Z")
    }

    function test_a_failed_undo_keeps_the_step_to_try_again() {
      trashFirst()
      var placeAnswer = fixture.answers["mail.place"]
      var answers = ({})
      for (var key in fixture.answers) answers[key] = fixture.answers[key]
      answers["mail.place"] = function(params, request) {
        BackendFixture.respond(mailService, request, null, { code: -32000, message: "network_unavailable" })
        return undefined
      }
      fixture.answers = answers
      mailService.undoMove()
      tryCompare(mailService, "moveHistoryBusy", false)
      compare(mailService.canUndoMove, true, "still there to try again")
      compare(mailService.canRedoMove, false)
      compare(record.where["1:2"], ["TRASH"])
      answers["mail.place"] = placeAnswer
      fixture.answers = answers
      mailService.undoMove()
      tryCompare(mailService, "canRedoMove", true)
      compare(record.where["1:2"], ["INBOX"])
    }

    function test_an_older_backend_records_nothing_and_shows_no_buttons() {
      mailService.backend.protocolInfo = { apiVersion: 8, protocol: 1, version: "0.0.0" }
      try {
        seed("gmail", "inbox", [message("1:2", ["INBOX"])])
        compare(named(app, "undo-move-button").visible, false)
        app.cursorId = "1:2"
        app.runShortcut("trash", "d")
        tryCompare(record, "trashed", ["1:2"])
        compare(requests("mail.locate").length, 0)
        compare(mailService.canUndoMove, false)
      } finally {
        mailService.backend.protocolInfo = { apiVersion: 9, protocol: 1, version: "0.0.0" }
      }
    }
  }
}
