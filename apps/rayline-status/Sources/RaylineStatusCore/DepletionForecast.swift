import Foundation

public enum DepletionForecast: Equatable, Sendable {
  case exhausted
  case noBurn
  case resetFirst
  case runsOut(Date)
  case learning
  case stale
  case unavailable
}

func depletionForecast(
  claim: LimitClaim?,
  snapshotFresh: Bool,
  windowSeconds: TimeInterval,
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

  let windowStart = reset.addingTimeInterval(-windowSeconds)
  let elapsed = now.timeIntervalSince(windowStart)
  guard elapsed > 0, elapsed <= windowSeconds else { return .learning }
  if elapsed < windowSeconds / 10, utilization < 0.2 { return .learning }

  let remainingSeconds = ceil(elapsed * (1 - utilization) / utilization)
  guard remainingSeconds.isFinite else { return .unavailable }
  let runOut = now.addingTimeInterval(remainingSeconds)
  return runOut >= reset ? .resetFirst : .runsOut(runOut)
}
