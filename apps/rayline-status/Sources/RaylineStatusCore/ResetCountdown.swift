import Foundation

/// Compact "time left in this window" text for a limit reset.
///
/// The popover shows this under each allowance so the wait is readable at a
/// glance: `1h 20m` for the five-hour window, `3d 5h` for the weekly windows.
public enum ResetCountdown {
  public static func text(until reset: Date?, from now: Date) -> String? {
    guard let reset else { return nil }
    let seconds = reset.timeIntervalSince(now)
    guard seconds > 0 else { return "due" }

    let minutes = Int((seconds / 60).rounded(.up))
    if minutes < 60 { return "\(max(minutes, 1))m" }

    let hours = minutes / 60
    let leftoverMinutes = minutes % 60
    if hours < 24 {
      return leftoverMinutes == 0 ? "\(hours)h" : "\(hours)h \(leftoverMinutes)m"
    }

    let days = hours / 24
    let leftoverHours = hours % 24
    return leftoverHours == 0 ? "\(days)d" : "\(days)d \(leftoverHours)h"
  }
}
