// Prints the size and origin in points of each normal window of the app named
// in the first argument that is on screen, one per line. Run with
// `osascript -l JavaScript - <app>`. A window in native fullscreen covers its
// whole display from 0,0. The window list needs no Accessibility access, which
// the AXFullScreen attribute through System Events would.
ObjC.import("CoreGraphics");

const ON_SCREEN_ONLY = 1;
const EXCLUDE_DESKTOP_ELEMENTS = 16;
const NULL_WINDOW = 0;

function run(argv) {
  const ref = $.CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP_ELEMENTS, NULL_WINDOW);
  return ObjC.deepUnwrap(ObjC.castRefToObject(ref))
    .filter((w) => w.kCGWindowOwnerName === argv[0] && w.kCGWindowLayer === 0)
    .map((w) => `${w.kCGWindowBounds.Width}x${w.kCGWindowBounds.Height}+${w.kCGWindowBounds.X}+${w.kCGWindowBounds.Y}`)
    .join("\n");
}
