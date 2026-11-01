import QtQuick

// A row's hover, as far as the row may draw it: hovered, and the keyboard has
// not taken the highlight since the pointer last moved (HoverGate). Without a
// gate it is a plain HoverHandler.
HoverHandler {
  id: handler

  property HoverGate gate: null
  readonly property bool shown: hovered && !(gate && gate.keyboard)

  onPointChanged: if (gate && hovered) gate.pointerAt(point.scenePosition)
}
