import Foundation

/// How fast an allowance is being consumed, measured from recent samples.
///
/// `fractionPerSecond` is a share of the whole window per second, so 0.0001
/// means the account burns 1% of the allowance every 100 seconds.
public struct BurnRate: Equatable, Sendable {
  /// Share of the allowance consumed per second, never negative.
  public let fractionPerSecond: Double
  /// How long the measurement covers.
  public let span: TimeInterval
  /// Samples the measurement is built from.
  public let sampleCount: Int
  /// When the newest sample was taken. A projection anchors here, not on the
  /// display clock, because the utilization it starts from was read here.
  public let measuredAt: Date

  public init(
    fractionPerSecond: Double,
    span: TimeInterval,
    sampleCount: Int,
    measuredAt: Date
  ) {
    self.fractionPerSecond = fractionPerSecond
    self.span = span
    self.sampleCount = sampleCount
    self.measuredAt = measuredAt
  }
}

/// One measured allowance: an account, a limit kind, and the window instance.
///
/// The window belongs in the identity because an account can hold more than
/// one live claim of the same kind. Fable does: the pool reports both an
/// overage claim and a scoped claim as `Model("Fable")`, each with its own
/// reset. Keying on account and kind alone would let one window's rate
/// describe another window's allowance.
public struct BurnRateKey: Hashable, Sendable {
  public let account: String
  public let kind: LimitKind
  /// Window reset, as whole milliseconds since the epoch.
  public let window: Int64

  public init(account: String, kind: LimitKind, window: Int64) {
    self.account = account
    self.kind = kind
    self.window = window
  }

  public init(account: String, kind: LimitKind, window: Date) {
    self.init(account: account, kind: kind, window: Self.stamp(window))
  }

  /// Whole milliseconds, so the key never depends on a float surviving a JSON
  /// round trip. Absurd dates clamp instead of trapping.
  static func stamp(_ date: Date) -> Int64 {
    let millis = (date.timeIntervalSince1970 * 1_000).rounded()
    guard millis > Double(Int64.min), millis < Double(Int64.max) else {
      return millis < 0 ? Int64.min : Int64.max
    }
    return Int64(millis)
  }
}

/// Recent utilization samples, kept so the app can measure the current burn.
///
/// The type is pure value math. It never reads or writes files; that is
/// `UsageHistoryFile`'s job. Samples live in one series per account, limit
/// kind and window instance. The window reset identifies the instance, so a
/// rollover starts a fresh series and old numbers never blend into the new
/// window.
public struct UsageHistory: Codable, Sendable {
  /// Utilization change that counts as news worth recording.
  static let utilizationEpsilon = 0.0005
  /// A flat period still records this often, so "not burning" is measurable.
  static let heartbeatSeconds: TimeInterval = 300
  /// Upper bound per series. Old samples fall off the front.
  static let maximumSamples = 200
  /// Bumped to 2 when `Series.reset` became whole milliseconds. A version 1
  /// file stored it as fractional seconds, so it must be rejected by the
  /// version guard rather than by a type mismatch deep in the decode.
  private static let formatVersion = 2

  private var series: [Series] = []

  public init() {}

  // MARK: - Recording

  /// Folds one status snapshot into the history and prunes what has expired.
  public mutating func record(_ status: PoolRuntimeStatus, at now: Date) {
    for account in status.accounts {
      for kind in LimitKind.allCases {
        guard
          let claim = account.claim(for: kind, at: now),
          let rawUtilization = claim.utilization,
          let reset = claim.resetDate()
        else { continue }
        append(
          account: account.id,
          kind: kind,
          reset: BurnRateKey.stamp(reset),
          utilization: min(max(rawUtilization, 0), 1),
          at: now.timeIntervalSince1970
        )
      }
    }
    prune(at: now)
  }

  private mutating func append(
    account: String,
    kind: LimitKind,
    reset: Int64,
    utilization: Double,
    at time: TimeInterval
  ) {
    let sample = Sample(time: time, utilization: utilization)
    guard
      let index = series.firstIndex(where: {
        $0.account == account && $0.kind == kind && $0.reset == reset
      })
    else {
      series.append(Series(account: account, kind: kind, reset: reset, samples: [sample]))
      return
    }
    guard let last = series[index].samples.last else {
      series[index].samples.append(sample)
      return
    }
    // The clock can move backward on an NTP correction or a wake from sleep.
    // Drop the stranded future and restart on the new timeline, rather than
    // going quiet and serving a rate nobody can refresh.
    if time <= last.time {
      series[index].samples.removeAll { $0.time >= time }
      series[index].samples.append(sample)
      return
    }
    let moved = abs(utilization - last.utilization) >= Self.utilizationEpsilon
    let heartbeatDue = time - last.time >= Self.heartbeatSeconds
    guard moved || heartbeatDue else { return }
    series[index].samples.append(sample)
  }

  private mutating func prune(at now: Date) {
    let stamp = BurnRateKey.stamp(now)
    let time = now.timeIntervalSince1970
    series.removeAll { $0.reset <= stamp }
    for index in series.indices {
      let cutoff = time - series[index].kind.burnLookbackSeconds
      series[index].samples.removeAll { $0.time < cutoff }
      let overflow = series[index].samples.count - Self.maximumSamples
      if overflow > 0 {
        series[index].samples.removeFirst(overflow)
      }
    }
    series.removeAll { $0.samples.isEmpty }
  }

  // MARK: - Measuring

  /// Measured burn for one allowance, or nil when the record is too thin.
  public func burnRate(for key: BurnRateKey, at now: Date) -> BurnRate? {
    let entry = series.first {
      $0.account == key.account && $0.kind == key.kind && $0.reset == key.window
    }
    guard let entry, entry.reset > BurnRateKey.stamp(now) else { return nil }
    return entry.rate(at: now.timeIntervalSince1970)
  }

  /// Measured burn for every live window that has enough recent samples. One
  /// entry per series, keyed by that series' own reset.
  public func burnRates(at now: Date) -> [BurnRateKey: BurnRate] {
    let stamp = BurnRateKey.stamp(now)
    let time = now.timeIntervalSince1970
    var rates: [BurnRateKey: BurnRate] = [:]
    for entry in series where entry.reset > stamp {
      guard let rate = entry.rate(at: time) else { continue }
      let key = BurnRateKey(account: entry.account, kind: entry.kind, window: entry.reset)
      rates[key] = rate
    }
    return rates
  }

  // MARK: - Storage

  private struct Sample: Codable, Sendable {
    let time: TimeInterval
    let utilization: Double

    enum CodingKeys: String, CodingKey {
      case time = "t"
      case utilization = "u"
    }
  }

  private struct Series: Codable, Sendable {
    let account: String
    let kind: LimitKind
    /// Whole milliseconds since the epoch. This names the window instance.
    let reset: Int64
    var samples: [Sample]

    enum CodingKeys: String, CodingKey {
      case account = "a"
      case kind = "k"
      case reset = "r"
      case samples = "s"
    }

    func rate(at time: TimeInterval) -> BurnRate? {
      let cutoff = time - kind.burnLookbackSeconds
      let recent = samples.filter { $0.time >= cutoff }
      guard let first = recent.first, let last = recent.last, recent.count >= 2 else {
        return nil
      }
      let span = last.time - first.time
      guard span >= kind.minimumBurnSpanSeconds else { return nil }
      // A negative delta is a correction or a rollover, not negative burn.
      let consumed = max(last.utilization - first.utilization, 0)
      return BurnRate(
        fractionPerSecond: consumed / span,
        span: span,
        sampleCount: recent.count,
        measuredAt: Date(timeIntervalSince1970: last.time)
      )
    }
  }

  private enum CodingKeys: String, CodingKey {
    case version = "v"
    case series = "s"
  }

  public init(from decoder: Decoder) throws {
    let container = try decoder.container(keyedBy: CodingKeys.self)
    let version = try container.decode(Int.self, forKey: .version)
    guard version == Self.formatVersion else {
      throw DecodingError.dataCorruptedError(
        forKey: .version,
        in: container,
        debugDescription: "unsupported usage history version \(version)"
      )
    }
    series = try container.decode([Series].self, forKey: .series)
  }

  public func encode(to encoder: Encoder) throws {
    var container = encoder.container(keyedBy: CodingKeys.self)
    try container.encode(Self.formatVersion, forKey: .version)
    try container.encode(series, forKey: .series)
  }
}

/// Keeps usage history on disk between app runs.
///
/// Every operation is best effort. A missing, unreadable or unknown file reads
/// as an empty history, and a failed write is dropped. History only sharpens a
/// forecast, so losing it must never break the app.
public final class UsageHistoryFile: Sendable {
  private let fileURL: URL?

  public init(fileURL: URL? = nil) {
    self.fileURL = fileURL ?? Self.defaultFileURL()
  }

  public func load() -> UsageHistory {
    guard
      let fileURL,
      let data = try? Data(contentsOf: fileURL),
      let history = try? JSONDecoder().decode(UsageHistory.self, from: data)
    else { return UsageHistory() }
    return history
  }

  public func save(_ history: UsageHistory) {
    guard let fileURL else { return }
    do {
      try FileManager.default.createDirectory(
        at: fileURL.deletingLastPathComponent(), withIntermediateDirectories: true)
      let data = try JSONEncoder().encode(history)
      try data.write(to: fileURL, options: [.atomic])
    } catch {
      // Best effort. The next write gets another chance.
    }
  }

  private static func defaultFileURL() -> URL? {
    let support = try? FileManager.default.url(
      for: .applicationSupportDirectory, in: .userDomainMask, appropriateFor: nil, create: false)
    guard let support else { return nil }
    return
      support
      .appendingPathComponent("ai.rayline.status", isDirectory: true)
      .appendingPathComponent("usage-history.json", isDirectory: false)
  }
}
