// keylog: selects the keyboard input source in the second argument, such as
// com.apple.keylayout.SwissGerman, and covers the main display with one text
// view. It appends lines to the file in the first argument: `source <id>`
// with the input source in use, `ready` once the window shows, `text <json>`
// with the whole text after each change, and `flags <names>` with the held
// modifiers (command, option, control, shift) after each modifier change.
// Run it in an app bundle with `open`, so that it gets a window in the login
// session.
//
// Build: swiftc -O -o keylog keylog.swift
import AppKit
import Carbon

let log = FileHandle(forWritingAtPath: CommandLine.arguments[1])!

func write(_ line: String) {
  log.seekToEndOfFile()
  log.write("\(line)\n".data(using: .utf8)!)
}

func sourceID(_ source: TISInputSource) -> String {
  Unmanaged<CFString>.fromOpaque(TISGetInputSourceProperty(source, kTISPropertyInputSourceID))
    .takeUnretainedValue() as String
}

let wanted = CommandLine.arguments[2]
let filter = [kTISPropertyInputSourceID: wanted] as CFDictionary
if let source = (TISCreateInputSourceList(filter, true).takeRetainedValue() as! [TISInputSource]).first {
  TISEnableInputSource(source)
  TISSelectInputSource(source)
}
write("source \(sourceID(TISCopyCurrentKeyboardLayoutInputSource().takeRetainedValue()))")

final class View: NSTextView, NSTextViewDelegate {
  override func flagsChanged(with event: NSEvent) {
    let names: [(NSEvent.ModifierFlags, String)] = [
      (.command, "command"), (.option, "option"), (.control, "control"), (.shift, "shift"),
    ]
    let held = names.filter { event.modifierFlags.contains($0.0) }.map { $0.1 }
    write("flags \(held.joined(separator: " "))")
    super.flagsChanged(with: event)
  }

  func textDidChange(_ notification: Notification) {
    let json = try! JSONEncoder().encode(string)
    write("text \(String(data: json, encoding: .utf8)!)")
  }
}

// A borderless window takes the keys only when it says so.
final class KeyWindow: NSWindow {
  override var canBecomeKey: Bool { true }
}

let app = NSApplication.shared
app.setActivationPolicy(.regular)
let frame = NSScreen.screens[0].frame
let window = KeyWindow(contentRect: frame, styleMask: [.borderless], backing: .buffered, defer: false)
let view = View(frame: NSRect(origin: .zero, size: frame.size))
view.delegate = view
// Type the characters as they arrive, as a code editor does.
view.isAutomaticQuoteSubstitutionEnabled = false
view.isAutomaticDashSubstitutionEnabled = false
view.isAutomaticTextReplacementEnabled = false
view.isAutomaticSpellingCorrectionEnabled = false
window.level = .floating
window.contentView = view
window.makeKeyAndOrderFront(nil)
window.makeFirstResponder(view)
app.activate(ignoringOtherApps: true)
write("ready")
app.run()
