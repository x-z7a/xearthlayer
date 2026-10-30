// XEarthLayer.app exists to carry the FSKit module: macOS only registers a
// file system extension that ships inside an app. Opening the app explains
// the one-time step of turning the extension on and offers to open the
// System Settings pane where that is done.

import AppKit

let app = NSApplication.shared
app.setActivationPolicy(.regular)
app.activate()

let alert = NSAlert()
alert.messageText = "Turn on the XEarthLayer file system extension"
alert.informativeText = """
    XEarthLayer mounts its scenery through a file system extension, so no \
    kernel extension or Recovery mode change is needed.

    In System Settings, open General > Login Items & Extensions, click the \
    info button next to File System Extensions, and turn on XEarthLayer. \
    You only need to do this once; then run `xearthlayer` as usual.
    """
alert.addButton(withTitle: "Open System Settings")
alert.addButton(withTitle: "Done")

if alert.runModal() == .alertFirstButtonReturn,
   let url = URL(string: "x-apple.systempreferences:com.apple.LoginItems-Settings.extension") {
    NSWorkspace.shared.open(url)
}
