// Prints the displays of the guest, one per line: first each online display
// of CoreGraphics, with its size and origin in points, its vendor, and its
// state, then the main display of CoreGraphics, which can be offline, then
// each screen that AppKit gives to apps. Run with `osascript -l JavaScript`.
ObjC.import("AppKit");
ObjC.import("CoreGraphics");

// JXA cannot read the C array of CGGetOnlineDisplayList, so this asks each
// small display id. A call for an unknown id returns 0xffffffff, not 0 or 1.
function cgDisplays() {
  const lines = [];
  for (let id = 1; id <= 64; id++) {
    if ($.CGDisplayIsOnline(id) !== 1) continue;
    const b = $.CGDisplayBounds(id);
    const mirrors = $.CGDisplayMirrorsDisplay(id);
    const vendor = $.CGDisplayVendorNumber(id).toString(16);
    const state = [
      $.CGDisplayIsActive(id) === 1 ? "active" : "inactive",
      $.CGDisplayIsMain(id) === 1 && "main",
      $.CGDisplayIsAsleep(id) === 1 && "asleep",
      mirrors !== 0 && `mirrors 0x${mirrors.toString(16)}`,
    ].filter(Boolean);
    lines.push(`cg 0x${id.toString(16)} ${b.size.width}x${b.size.height}+${b.origin.x}+${b.origin.y} vendor 0x${vendor} ${state.join(" ")}`);
  }
  return lines;
}

function screens() {
  return $.NSScreen.screens.js.map((s) => {
    const f = s.frame;
    const id = s.deviceDescription.objectForKey("NSScreenNumber").js;
    return `screen 0x${id.toString(16)} ${f.size.width}x${f.size.height}+${f.origin.x}+${f.origin.y} ${s.localizedName.js}`;
  });
}

function run() {
  const main = $.CGMainDisplayID();
  const state = $.CGDisplayIsOnline(main) === 1 ? "online" : "offline";
  return [...cgDisplays(), `main 0x${main.toString(16)} ${state}`, ...screens()].join("\n");
}
