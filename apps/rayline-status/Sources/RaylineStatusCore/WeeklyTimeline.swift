import Foundation

/// Weekly forecasts only. A reset ends the known depleted interval; availability
/// after that reset is deliberately not extrapolated into another weekly cycle.
public struct WeeklyTimeline: Sendable {
  public let now: Date
  public let end: Date
  public let accounts: [WeeklyGlance]
  public let unknownCount: Int
  public let gap: DateInterval?

  public init(pool: PoolGlance, now: Date) {
    self.now = now
    accounts = pool.accounts
    end = min(
      now.addingTimeInterval(7 * 86400),
      max(now.addingTimeInterval(4 * 86400), pool.accounts.compactMap(\.reset).max() ?? now))
    let known = accounts.filter(\.hasTimedForecast)
    unknownCount = accounts.count - known.count
    let intervals = known.compactMap { Self.depletedInterval($0, now: now) }
    // An account without a depleted interval prevents a shared gap. Unknown
    // accounts cannot prove coverage, but must qualify the resulting warning.
    if !known.isEmpty, intervals.count == known.count,
      let start = intervals.map(\.start).max(),
      let finish = intervals.map(\.end).min(), max(now, start) < min(end, finish)
    {
      gap = DateInterval(start: max(now, start), end: min(end, finish))
    } else {
      gap = nil
    }
  }

  public func fraction(_ date: Date) -> Double {
    min(1, max(0, date.timeIntervalSince(now) / end.timeIntervalSince(now)))
  }

  /// Calendar midnights preserve local date labels through daylight saving.
  public func dayTicks(calendar: Calendar = .current) -> [Date] {
    var dates: [Date] = []
    var day = calendar.startOfDay(for: now)
    while let next = calendar.date(byAdding: .day, value: 1, to: day), next > day, next < end {
      if next.timeIntervalSince(now) >= 8 * 3600,
        end.timeIntervalSince(next) >= 8 * 3600
      {
        dates.append(next)
      }
      day = next
    }
    return dates
  }

  public func depletedInterval(for account: WeeklyGlance) -> DateInterval? {
    Self.depletedInterval(account, now: now)
  }

  private static func depletedInterval(_ account: WeeklyGlance, now: Date) -> DateInterval? {
    guard account.hasTimedForecast, let reset = account.reset, reset > now else { return nil }
    let start: Date
    switch account.state {
    case .out: start = now
    case .runningFast(let date): start = max(now, date)
    default: return nil
    }
    guard start < reset else { return nil }
    return DateInterval(start: start, end: reset)
  }
}
