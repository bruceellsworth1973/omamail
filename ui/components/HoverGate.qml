import QtQuick

// Who owns the highlight in one list: the keyboard or the mouse.
//
// Moving the selection from the keyboard takes it, and the mouse's hover goes
// dark until the pointer really moves again. Without that the list showed two
// highlights at once — the cursor row, and whatever row the pointer had been
// left resting on — and nothing said which one `Return` or `d` would act on.
//
// "Really moves" is the pointer's position in the scene changing. Qt re-reports
// hover when content scrolls under a still pointer, and following the cursor
// scrolls the list, so a hover report alone is not a movement; a report at the
// position already known is ignored. The first report after the gate closes
// only records where the pointer is, because there is nothing to compare it
// with yet.
//
// One per list, held by whatever moves that list's cursor: a menu, a picker,
// the message list. PointerHover feeds it and reads it.
QtObject {
  property bool keyboard: false
  property bool known: false
  property real pointerX: 0
  property real pointerY: 0

  function keyboardMoved() { keyboard = true }

  function pointerAt(position) {
    if (known && position.x === pointerX && position.y === pointerY) return
    var moved = known
    pointerX = position.x
    pointerY = position.y
    known = true
    if (moved) keyboard = false
  }
}
