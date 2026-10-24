const assert = require("assert")
const { load, deepEqual } = require("./load")
const palette = load("calendar/Palette.js")

deepEqual(palette.keys(), ["accent", "red", "green", "yellow", "blue", "magenta", "cyan"])

const parsed = palette.parse([
  'accent = "#ff7a00"',
  'red = "#d45941"',
  'color2 = "#578c60"',
  'yellow = "#c9b26d"',
  'color4 = "#5fa2d5"',
  'magenta = "#b07aa1"',
  'color6 = "#7ec0ae"'
].join("\n"))
deepEqual(parsed, {
  accent: "#ff7a00", red: "#d45941", green: "#578c60",
  yellow: "#c9b26d", blue: "#5fa2d5", magenta: "#b07aa1", cyan: "#7ec0ae"
})

assert.strictEqual(palette.normalizeKey("blue"), "blue")
assert.strictEqual(palette.normalizeKey("unknown"), "")
assert.strictEqual(palette.defaultKey("work-team"), palette.defaultKey("work-team"),
  "the fallback is stable")
assert.ok(palette.keys().indexOf(palette.defaultKey("work-team")) >= 0)

// A calendar's own colour: chosen per calendar, never picked by default.
assert.strictEqual(palette.normalizeKey("calendar"), "calendar")
assert.ok(palette.keys().indexOf("calendar") < 0, "the provider colour is not a theme slot")
assert.strictEqual(palette.normalizeHex("#9FE1E7"), "#9fe1e7")
for (const refused of ["9fe1e7", "#9fe1e", "#9fe1e7ff", "red", "url(x)", null, undefined])
  assert.strictEqual(palette.normalizeHex(refused), "", String(refused))
assert.strictEqual(palette.effectiveKey({ id: "g", colorKey: "calendar", providerColor: "#9fe1e7" }), "#9fe1e7")
assert.strictEqual(palette.effectiveKey({ id: "g", colorKey: "blue", providerColor: "#9fe1e7" }), "blue",
  "a theme slot chosen over the provider colour stays")
assert.strictEqual(palette.effectiveKey({ id: "g", colorKey: "calendar", providerColor: "" }), palette.defaultKey("g"),
  "no provider colour falls back to the slot the id picks")
assert.strictEqual(palette.effectiveKey({ id: "g", colorKey: "calendar", providerColor: "javascript:x" }), palette.defaultKey("g"))

console.log("test_calendar_palette.js ok")
