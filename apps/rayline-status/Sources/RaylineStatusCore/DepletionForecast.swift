import Foundation

/// Where a projected run-out time came from.
public enum BurnBasis: Equatable, Sendable {
  /// Measured from recent samples. The value is the trailing span they cover.
  case measured(TimeInterval)
  /// Even burn since the window opened. Used when no measurement exists yet.
  case windowAverage
}

public enum DepletionForecast: Equatable, Sendable {
  case exhausted
  case noBurn
  case resetFirst
  case runsOut(Date, BurnBasis)
  case learning
  case stale
  case unavailable
}

func depletionForecast(
  claim: LimitClaim?,
  snapshotFresh: Bool,
  windowSeconds: TimeInterval,
  burnRate: BurnRate? = nil,
  now: Date
) -> DepletionForecast {
  guard let claim else { return .unavailable }
  guard snapshotFresh else { return .stale }
  if claim.isHardExhausted(at: now) { return .exhausted }
  guard let rawUtilization = claim.utilization else { return .unavailable }
  let utilization = min(max(rawUtilization, 0), 1)
  if utilization <= Double.ulpOfOne { return .noBurn }
  guard let reset = claim.resetDate(), reset > now, windowSeconds > 0 else {
    return .unavailable
  }

  // A measured rate answers the real question: at the pace of the last few
  // minutes, when does this run out? It needs no warm-up heuristics, because
  // it does not guess how the window was spent before the app was watching.
  if let burnRate {
    guard burnRate.fractionPerSecond > 0 else { return .resetFirst }
    let remainingSeconds = ceil((1 - utilization) / burnRate.fractionPerSecond)
    guard remainingSeconds.isFinite else { return .unavailable }
    // Anchored to the sample the utilization came from, not to the display
    // clock. Otherwise the projection is pushed later on every tick and never
    // counts down.
    let runOut = burnRate.measuredAt.addingTimeInterval(remainingSeconds)
    return runOut >= reset ? .resetFirst : .runsOut(runOut, .measured(burnRate.span))
  }

  let windowStart = reset.addingTimeInterval(-windowSeconds)
  let elapsed = now.timeIntervalSince(windowStart)
  guard elapsed > 0, elapsed <= windowSeconds else { return .learning }
  if elapsed < windowSeconds / 10, utilization < 0.2 { return .learning }

  let remainingSeconds = ceil(elapsed * (1 - utilization) / utilization)
  guard remainingSeconds.isFinite else { return .unavailable }
  let runOut = now.addingTimeInterval(remainingSeconds)
  return runOut >= reset ? .resetFirst : .runsOut(runOut, .windowAverage)
}
