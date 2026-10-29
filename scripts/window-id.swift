// Prints the CGWindowID of the first on-screen window owned by a process name (for screencapture -l).
import CoreGraphics
import Foundation

let owner = CommandLine.arguments.dropFirst().first ?? "scopewerk-app"
let windows = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as? [[String: Any]] ?? []
for window in windows where (window[kCGWindowOwnerName as String] as? String) == owner {
    if let layer = window[kCGWindowLayer as String] as? Int, layer == 0, let id = window[kCGWindowNumber as String] as? Int {
        print(id)
        exit(0)
    }
}
exit(1)
