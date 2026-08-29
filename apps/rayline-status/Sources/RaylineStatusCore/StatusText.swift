import Foundation

/// The words the popover shows.
///
/// This is text derivation only: it reads a presentation and returns a string.
/// Colours, symbols and layout stay in the view, and so does date formatting.
/// Anything that needs a formatted date takes it as a parameter, so Core never
/// holds a locale or a time zone and the tests never depend on one.
public enum StatusText {

  // MARK: - One allowance cell

  /// The large number: `OUT` when spent, otherwise whole percent left.
  public static func cellValue(_ limit: LimitPresentation?) -> String {
    guard let limit else { return "—" }
    if limit.exhausted { return "OUT" }
    guard let remaining = limit.remainingFraction else { return "—" }
    return String(format: "%.0f", remaining * 100)
  }

  /// The percent sign, shown only when the number is a percent.
  public static func cellUnit(_ limit: LimitPresentation?) -> String? {
    guard let limit, !limit.exhausted, limit.remainingFraction != nil else { return nil }
    return "%"
  }

  /// A cell at risk trades the reset countdown for the projected time to
  /// empty, which is the number that matters when the allowance may not last.
  public static func cellCountdown(_ limit: LimitPresentation?) -> String {
    guard let limit else { return "—" }
    if limit.isAtRisk {
      return limit.runOutCountdown ?? limit.resetCountdown ?? "—"
    }
    return limit.resetCountdown ?? "—"
  }

  /// How much is left, in words. An unknown utilization says so rather than
  /// rendering an em dash as a percentage.
  public static func allowance(_ limit: LimitPresentation) -> String {
    if limit.exhausted { return "exhausted" }
    guard let remaining = limit.remainingFraction else { return "usage unknown" }
    return String(format: "%.0f%% left", remaining * 100)
  }

  /// The same fact in the hover header's compact voice.
  public static func remainingSummary(_ limit: LimitPresentation) -> String {
    if limit.exhausted { return "OUT" }
    guard let remaining = limit.remainingFraction else { return "—" }
    return String(format: "%.0f%% LEFT", remaining * 100)
  }

  /// The cell shows one number, so the tooltip must give both without
  /// ambiguity: when the allowance may empty, and when the window resets.
  public static func tooltip(
    _ limit: LimitPresentation?,
    resetDateText: String,
    runOutDateText: String
  ) -> String {
    guard let limit else { return "Allowance unavailable" }
    let resetsIn = limit.resetCountdown.map { "Resets in \($0)" } ?? "Reset time unknown"
    let head = "\(limit.kind.title): \(allowance(limit))"
    let resetLine = "\(resetsIn) · \(resetDateText)"
    if case .runsOut(_, let basis) = limit.forecast {
      return "\(head)\nMay run out in \(runOutIn(limit)) \(burnBasisPhrase(basis))\n\(resetLine)"
    }
    return
      "\(head)\n\(resetLine)\n\(forecastNote(limit.forecast, runOutDateText: runOutDateText))"
  }

  /// The hover header's single line about one allowance.
  public static func limitTiming(_ limit: LimitPresentation, resetDateText: String) -> String {
    let resetsIn = limit.resetCountdown ?? "unknown"
    switch limit.forecast {
    case .exhausted: return "Limit reached · back in \(resetsIn) · \(resetDateText) UTC"
    case .noBurn: return "No current burn · resets in \(resetsIn) · \(resetDateText) UTC"
    case .resetFirst: return "Safe until reset · resets in \(resetsIn) · \(resetDateText) UTC"
    case .runsOut(_, let basis):
      return
        "May run out in \(runOutIn(limit)) \(burnBasisPhrase(basis)) · resets in \(resetsIn)"
    case .learning: return "Learning rate · resets in \(resetsIn) · \(resetDateText) UTC"
    case .stale: return "Usage is stale · resets in \(resetsIn) · \(resetDateText) UTC"
    case .unavailable:
      return "Prediction unavailable · resets in \(resetsIn) · \(resetDateText) UTC"
    }
  }

  public static func forecastNote(
    _ forecast: DepletionForecast,
    runOutDateText: String
  ) -> String {
    switch forecast {
    case .exhausted: "Exhausted until reset"
    case .noBurn: "No current consumption"
    case .resetFirst: "Expected to reset before depletion"
    case .runsOut(_, let basis):
      "Risk: projected to run out \(runOutDateText) \(burnBasisPhrase(basis))"
    case .learning: "Forecast is learning the current rate"
    case .stale: "Forecast unavailable because usage is stale"
    case .unavailable: "Forecast unavailable"
    }
  }

  /// Names the evidence behind a projection, so "1h 20m" is never mistaken for
  /// a promise.
  public static func burnBasisPhrase(_ basis: BurnBasis) -> String {
    switch basis {
    case .measured(let span): "at current burn (last \(spanText(span)))"
    case .windowAverage: "at average burn"
    }
  }

  // MARK: - One account

  /// The soonest thing worth saying about an account, in priority order.
  public static func nextEvent(for account: AccountPresentation) -> AccountEvent {
    if let limit = soonestExhausted(account) { return .exhausted(limit) }
    if let risk = soonestRisk(account) { return .risk(risk.0, risk.1, risk.2) }
    if let limit = soonestReset(account) { return .reset(limit) }
    return .none
  }

  /// The hover header's line when no single allowance is in focus.
  /// `dateText` is `event.date` formatted compactly.
  public static func accountTiming(_ event: AccountEvent, dateText: String) -> String {
    switch event {
    case .exhausted(let limit):
      return "\(limit.kind.title) back in \(resetIn(limit)) · \(dateText) UTC"
    case .risk(let limit, let basis, _):
      return
        "\(limit.kind.title) may hit its limit in \(runOutIn(limit)) \(burnBasisPhrase(basis))"
    case .reset(let limit):
      return "\(limit.kind.title) resets in \(resetIn(limit)) · \(dateText) UTC"
    case .none:
      return "Timing unavailable"
    }
  }

  /// The row tooltip's closing line. `dateText` is `event.date` in full.
  public static func nextEventDescription(_ event: AccountEvent, dateText: String) -> String {
    switch event {
    case .exhausted(let limit):
      return "Next: \(limit.kind.title) allowance resets in \(resetIn(limit)) (\(dateText))"
    case .risk(let limit, let basis, _):
      return
        "Next risk: \(limit.kind.title) allowance may run out in \(runOutIn(limit)) \(burnBasisPhrase(basis)) (\(dateText))"
    case .reset(let limit):
      return
        "Next: \(limit.kind.title) allowance resets in \(resetIn(limit)) (\(dateText)); no earlier depletion projected"
    case .none:
      return "Next event unavailable"
    }
  }

  // MARK: - Pieces

  /// Countdowns come from the presentation, which was built from the store's
  /// tick clock. Reading `Date()` here instead would let the hover header and
  /// the cell disagree by a minute.
  static func resetIn(_ limit: LimitPresentation) -> String {
    limit.resetCountdown ?? "unknown"
  }

  static func runOutIn(_ limit: LimitPresentation) -> String {
    limit.runOutCountdown ?? "unknown"
  }

  /// Reuses the countdown format so a measured span reads like every other
  /// duration in the popover.
  static func spanText(_ span: TimeInterval) -> String {
    let origin = Date(timeIntervalSince1970: 0)
    return ResetCountdown.text(until: origin.addingTimeInterval(max(span, 1)), from: origin)
      ?? "unknown"
  }

  static func soonestExhausted(_ account: AccountPresentation) -> LimitPresentation? {
    account.limits
      .filter(\.exhausted)
      .filter { $0.reset != nil }
      .min { ($0.reset ?? .distantFuture) < ($1.reset ?? .distantFuture) }
  }

  static func soonestRisk(
    _ account: AccountPresentation
  ) -> (LimitPresentation, BurnBasis, Date)? {
    account.limits
      .compactMap { limit -> (LimitPresentation, BurnBasis, Date)? in
        guard case .runsOut(let date, let basis) = limit.forecast else { return nil }
        return (limit, basis, date)
      }
      .min { $0.2 < $1.2 }
  }

  static func soonestReset(_ account: AccountPresentation) -> LimitPresentation? {
    account.limits
      .filter { $0.reset != nil }
      .min { ($0.reset ?? .distantFuture) < ($1.reset ?? .distantFuture) }
  }
}

/// What an account row talks about when no single allowance is in focus.
public enum AccountEvent: Sendable {
  case exhausted(LimitPresentation)
  case risk(LimitPresentation, BurnBasis, Date)
  case reset(LimitPresentation)
  case none

  /// The moment the caller must format for the timing lines.
  public var date: Date? {
    switch self {
    case .exhausted(let limit): limit.reset
    case .risk(_, _, let date): date
    case .reset(let limit): limit.reset
    case .none: nil
    }
  }
}
