// displaycfg: makes vdisplay (vendor 0x1AB2) the only display of the login
// session, as on the Mac mini. It moves vdisplay to 0,0 and switches off every
// other display through the private SkyLight call SLSConfigureDisplayEnabled.
// WindowServer applies the change about 2 s after the call returns.
//
//   displaycfg disable
//
// Build: swiftc -O -o displaycfg displaycfg.swift
import CoreGraphics
import Foundation

let VDISPLAY_VENDOR: UInt32 = 0x1AB2

func online() -> [CGDirectDisplayID] {
  var ids = [CGDirectDisplayID](repeating: 0, count: 32)
  var n: UInt32 = 0
  CGGetOnlineDisplayList(32, &ids, &n)
  return Array(ids[0..<Int(n)])
}

func die(_ message: String) -> Never {
  FileHandle.standardError.write("displaycfg: \(message)\n".data(using: .utf8)!)
  exit(1)
}

func check(_ what: String, _ err: CGError) {
  if err != .success { die("\(what) failed: CGError \(err.rawValue)") }
}

typealias EnableFn = @convention(c) (CGDisplayConfigRef, CGDirectDisplayID, Bool) -> CGError

guard CommandLine.arguments.dropFirst().first == "disable" else { die("usage: displaycfg disable") }
let sky = dlopen("/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight", RTLD_NOW)
guard let p = dlsym(sky, "SLSConfigureDisplayEnabled") else { die("no SLSConfigureDisplayEnabled") }
let enable = unsafeBitCast(p, to: EnableFn.self)

let ids = online()
guard let vd = ids.first(where: { CGDisplayVendorNumber($0) == VDISPLAY_VENDOR }) else { die("no vdisplay online") }

var cfg: CGDisplayConfigRef?
check("CGBeginDisplayConfiguration", CGBeginDisplayConfiguration(&cfg))
check("origin of vdisplay", CGConfigureDisplayOrigin(cfg, vd, 0, 0))
for id in ids where id != vd {
  check("disable 0x\(String(id, radix: 16))", enable(cfg!, id, false))
}
check("CGCompleteDisplayConfiguration", CGCompleteDisplayConfiguration(cfg, .forSession))
