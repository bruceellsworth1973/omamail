import QtQuick 2.15
import QtTest 1.3
import "../../components" as Omamail

// The keyboard's highlight wins over the mouse's. Moving the selection from the
// keyboard puts out the hover the pointer left behind, and it stays out until
// the pointer really moves — including when the list scrolls a different row
// under a pointer that has not moved, which Qt reports as a fresh hover
// (tst_hover_under_scroll).
Item {
  id: host
  width: 200; height: 100

  Omamail.HoverGate { id: listGate }

  Flickable {
    id: flick
    anchors.fill: parent
    contentWidth: width
    contentHeight: col.height
    clip: true

    Column {
      id: col
      width: flick.width
      Repeater {
        id: rows
        model: 10
        Rectangle {
          required property int index
          readonly property bool shown: hover.shown
          width: col.width
          height: 40
          Omamail.PointerHover { id: hover; gate: listGate }
        }
      }
    }
  }

  TestCase {
    name: "HoverGate"
    when: windowShown

    function shownRows() {
      var out = []
      for (var i = 0; i < rows.count; i++) if (rows.itemAt(i).shown) out.push(i)
      return out
    }

    function test_keyboard_puts_out_hover_until_the_pointer_moves() {
      mouseMove(host, 100, 50)
      mouseMove(host, 101, 50)
      wait(50)
      compare(shownRows(), [1], "the pointer's row is hovered")

      listGate.keyboardMoved()
      compare(shownRows(), [], "keyboard navigation clears the mouse's hover")

      flick.contentY = 160          // following the cursor; the pointer is still
      wait(80)
      compare(shownRows(), [], "a row scrolled under a still pointer stays dark")

      mouseMove(host, 102, 50)
      wait(50)
      compare(shownRows(), [5], "moving the pointer brings hover back")
    }
  }
}
