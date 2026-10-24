// scrolllog: covers the main display with one window, and appends a line
// `scroll <deltaX> <deltaY>` to the file in the first argument for each scroll
// event that reaches it. The deltas have the sign of the device, also with
// natural scrolling on: a positive deltaY scrolls up, a positive deltaX left.
// It writes `ready` once the window shows. Run it in an app bundle with
// `open`, so that it gets a window in the login session.
//
// Build: swiftc -O -o scrolllog scrolllog.swift
import AppKit

let log = FileHandle(forWritingAtPath: CommandLine.arguments[1])!

func write(_ line: String) {
  log.seekToEndOfFile()
  log.write("\(line)\n".data(using: .utf8)!)
}

final class View: NSView {
  override func scrollWheel(with event: NSEvent) {
    let sign: CGFloat = event.isDirectionInvertedFromDevice ? -1 : 1
    write("scroll \(sign * event.scrollingDeltaX) \(sign * event.scrollingDeltaY)")
  }
}

let app = NSApplication.shared
app.setActivationPolicy(.regular)
let window = NSWindow(
  contentRect: NSScreen.screens[0].frame, styleMask: [.borderless], backing: .buffered, defer: false)
window.level = .floating
window.contentView = View()
window.makeKeyAndOrderFront(nil)
app.activate(ignoringOtherApps: true)
write("ready")
app.run()
