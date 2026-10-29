// Prints the CGWindowID of the first on-screen window owned by a process name (for screencapture -l).
import CoreGraphics
import Foundation

let owner = CommandLine.arguments.dropFirst().first ?? "scopewerk-app"
let windows = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as? [[String: Any]] ?? []
for window in windows where (window[kCGWindowOwnerName as String] as? String) == owner {
    if let id = window[kCGWindowNumber as String] as? Int, let bounds = window[kCGWindowBounds as String] as? [String: Any], (bounds["Width"] as? Double ?? 0) > 200 {
        print(id)
        exit(0)
    }
}
exit(1)
