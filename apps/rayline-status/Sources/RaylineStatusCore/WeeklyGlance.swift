import Foundation

/// One account reduced to the weekly limit, which is what the menu bar and the
/// popover show at a glance. The five-hour and Fable limits only surface as
/// flags, and only when they need attention.
public struct WeeklyGlance: Identifiable, Sendable {
  public enum State: Equatable, Sendable {
    /// The weekly limit is spent. The date is when it comes back.
    case out(back: Date?)
    /// At the measured burn the weekly limit empties before it resets.
    case runningFast(runOut: Date)
    case onPace
    /// Nothing used this week.
    case idle
    /// Too little history to project a rate yet.
    case measuring
    case unknown
  }

  public struct Flag: Equatable, Sendable {
    public enum Tone: Sendable {
      case neutral, warning, critical
    }

    public let text: String
    public let tone: Tone
    public let help: String
  }

  public let accountID: String
  /// Share of the weekly allowance left, 0...1.
  public let remaining: Double?
  /// Where the bar should end at even pace: the share of the week still to
  /// go. A bar shorter than this is burning faster than the week allows.
  public let paceMark: Double?
  public let reset: Date?
  public let state: State
  public let flags: [Flag]
  /// Fresh, usable forecast with a future reset; stale exhaustion is not a forecast.
  public let hasTimedForecast: Bool

  public var id: String { accountID }

  public init(account: AccountPresentation, now: Date) {
    let weekly = account.limits.first { $0.kind == .sevenDay }
    accountID = account.id
    remaining = weekly?.remainingFraction
    reset = weekly?.reset
    state = Self.state(weekly)
    switch weekly?.forecast {
    case .exhausted?, .noBurn?, .resetFirst?, .runsOut?:
      hasTimedForecast = weekly?.reset.map { $0 > now } ?? false
    default:
      hasTimedForecast = false
    }
    switch state {
    case .onPace, .runningFast, .measuring:
      paceMark = weekly?.reset.flatMap { reset in
        reset > now
          ? min(1, reset.timeIntervalSince(now) / LimitKind.sevenDay.windowSeconds) : nil
      }
    case .out, .idle, .unknown:
      paceMark = nil
    }
    flags = Self.flags(account, now: now)
  }

  /// Rank used to put the accounts that need action first.
  var urgency: Int {
    switch state {
    case .out: 0
    case .runningFast: 1
    case .onPace, .idle, .measuring: 2
    case .unknown: 3
    }
  }

  private static func state(_ weekly: LimitPresentation?) -> State {
    guard let weekly else { return .unknown }
    if weekly.exhausted { return .out(back: weekly.reset) }
    switch weekly.forecast {
    case .exhausted: return .out(back: weekly.reset)
    case .runsOut(let date, _): return .runningFast(runOut: date)
    case .noBurn: return (weekly.remainingFraction ?? 0) >= 0.995 ? .idle : .onPace
    case .resetFirst: return .onPace
    case .learning: return .measuring
    case .stale, .unavailable: return .unknown
    }
  }

  private static func flags(_ account: AccountPresentation, now: Date) -> [Flag] {
    var flags: [Flag] = []
    if let fiveHour = account.limits.first(where: { $0.kind == .fiveHour }) {
      if fiveHour.exhausted {
        flags.append(
          Flag(
            text: "5h out", tone: .critical,
            help: "5-hour limit reached · back in \(fiveHour.resetCountdown ?? "unknown")"))
      } else if let runOut = fiveHour.runOutCountdown, let date = fiveHour.runOutDate {
        flags.append(
          Flag(
            text: "5h \(chipCountdown(until: date, from: now))", tone: .warning,
            help: "5-hour burst empties in \(runOut) at the current rate"))
      }
    }
    if let fable = account.limits.first(where: { $0.kind == .fable }) {
      if fable.exhausted {
        flags.append(
          Flag(
            text: "Fable out", tone: .critical,
            help: "Fable limit reached · back in \(fable.resetCountdown ?? "unknown")"))
      } else if let runOut = fable.runOutCountdown {
        flags.append(
          Flag(
            text: "Fable", tone: .warning,
            help: "Fable empties in \(runOut), before its weekly reset"))
      }
    }
    let resets = account.limitResets
    if resets.state == .current, let usable = resets.usableCount, usable > 0 {
      flags.append(
        Flag(
          text: "↺\(usable)", tone: .neutral,
          help: "\(resets.summary)\n\(ResetInventoryPresentation.manualRedemptionWarning)"))
    }
    return flags
  }
}

/// Coarse enough to fit a chip: minutes under an hour, then approximate hours.
func chipCountdown(until date: Date, from now: Date) -> String {
  let minutes = max(1, Int((date.timeIntervalSince(now) / 60).rounded(.up)))
  return minutes < 60 ? "\(minutes)m" : "~\(minutes / 60)h"
}

public struct PoolGlance: Sendable {
  /// Pool order, as the daemon reports it.
  public let accounts: [WeeklyGlance]

  public init(_ presentation: PoolPresentation, now: Date) {
    accounts = presentation.accounts.map { WeeklyGlance(account: $0, now: now) }
  }

  public var runningFastCount: Int {
    accounts.filter { if case .runningFast = $0.state { true } else { false } }.count
  }

  public var outCount: Int {
    accounts.filter { if case .out = $0.state { true } else { false } }.count
  }

  /// The account whose weekly limit is projected to run out first.
  public var firstRunOut: (account: WeeklyGlance, date: Date)? {
    accounts.compactMap { glance -> (WeeklyGlance, Date)? in
      if case .runningFast(let date) = glance.state { return (glance, date) }
      return nil
    }
    .min { $0.1 < $1.1 }
    .map { (account: $0.0, date: $0.1) }
  }

  /// Most urgent first: out, then running fast by run-out time, then the rest
  /// by weekly allowance left. Unknown accounts go last.
  public var byUrgency: [WeeklyGlance] {
    accounts.enumerated().sorted { left, right in
      let (l, r) = (left.element, right.element)
      if l.urgency != r.urgency { return l.urgency < r.urgency }
      switch (l.state, r.state) {
      case (.runningFast(let a), .runningFast(let b)) where a != b: return a < b
      case (.out(let a?), .out(let b?)) where a != b: return a < b
      default: break
      }
      let (lr, rr) = (l.remaining ?? 2, r.remaining ?? 2)
      if lr != rr { return lr < rr }
      return left.offset < right.offset
    }
    .map(\.element)
  }

  /// Accounts drawn in the menu-bar glyph. Pool order keeps each bar in a
  /// stable place. Past `limit`, the most urgent accounts win so trouble is
  /// never hidden behind a healthy account.
  public func menuBarAccounts(limit: Int = 5) -> [WeeklyGlance] {
    guard accounts.count > limit else { return accounts }
    let chosen = Set(byUrgency.prefix(limit).map(\.id))
    return accounts.filter { chosen.contains($0.id) }
  }
}
