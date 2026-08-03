import Foundation

public struct PoolRuntimeStatus: Decodable, Sendable {
  public let poolID: String
  public let accounts: [AccountRuntimeStatus]
  public let placement: PoolPlacementRuntimeStatus?

  enum CodingKeys: String, CodingKey {
    case poolID = "pool_id"
    case accounts
    case placement
  }
}

public struct AccountRuntimeStatus: Decodable, Sendable {
  public let id: String
  public let credentialHealth: String
  public let subscriptionType: String?
  public let usageSnapshotFresh: Bool
  public let completeGlobalSnapshot: Bool
  public let claims: [LimitClaim]
  public let lastError: String?

  enum CodingKeys: String, CodingKey {
    case id
    case credentialHealth = "credential_health"
    case subscriptionType = "subscription_type"
    case usageSnapshotFresh = "usage_snapshot_fresh"
    case completeGlobalSnapshot = "complete_global_snapshot"
    case claims
    case lastError = "last_error"
  }
}

public struct LimitClaim: Decodable, Sendable {
  public let key: String
  public let scope: ClaimScope
  public let utilization: Double?
  public let status: String
  public let resetsAt: String?

  enum CodingKeys: String, CodingKey {
    case key
    case scope
    case utilization
    case status
    case resetsAt = "resets_at"
  }

  public func resetDate() -> Date? {
    guard let resetsAt else { return nil }
    return ResetTimestamp.parse(resetsAt)
  }

  public func isCurrent(at now: Date) -> Bool {
    guard let reset = resetDate() else { return true }
    return reset > now
  }

  public func isHardExhausted(at now: Date) -> Bool {
    isCurrent(at: now) && (status == "rejected" || (utilization ?? 0) >= 1)
  }
}

public struct ClaimScope: Decodable, Sendable {
  public let kind: String
  public let value: String?
}

public struct PoolPlacementRuntimeStatus: Decodable, Sendable {
  public let activeLeaseTTLSeconds: UInt64
  public let accounts: [AccountPlacementRuntimeStatus]

  enum CodingKeys: String, CodingKey {
    case activeLeaseTTLSeconds = "active_lease_ttl_seconds"
    case accounts
  }
}

public struct AccountPlacementRuntimeStatus: Decodable, Sendable {
  public let id: String
  public let activeLaunchLeases: Int
  public let activeModelLeases: [String: Int]

  enum CodingKeys: String, CodingKey {
    case id
    case activeLaunchLeases = "active_launch_leases"
    case activeModelLeases = "active_model_leases"
  }
}

public enum LimitKind: String, CaseIterable, Identifiable, Sendable {
  case fiveHour
  case sevenDay
  case fable

  public var id: String { rawValue }

  public var title: String {
    switch self {
    case .fiveHour: "5 hour"
    case .sevenDay: "7 day"
    case .fable: "Fable"
    }
  }

  var windowSeconds: TimeInterval {
    switch self {
    case .fiveHour: 5 * 60 * 60
    case .sevenDay, .fable: 7 * 24 * 60 * 60
    }
  }
}

public enum AccountAvailability: Sendable {
  case all
  case nonFable
  case none
  case unknown

  public var label: String {
    switch self {
    case .all: "All models"
    case .nonFable: "Non-Fable"
    case .none: "Unavailable"
    case .unknown: "Unknown"
    }
  }
}

public struct LimitPresentation: Identifiable, Sendable {
  public let kind: LimitKind
  public let remainingFraction: Double?
  public let reset: Date?
  public let exhausted: Bool
  public let forecast: DepletionForecast

  public var id: String { kind.id }
}

public struct AccountPresentation: Identifiable, Sendable {
  public let id: String
  public let plan: String
  public let availability: AccountAvailability
  public let activeLaunches: Int
  public let limits: [LimitPresentation]
  public let warning: String?
}

public struct PoolPresentation: Sendable {
  public let poolID: String
  public let accounts: [AccountPresentation]

  public var availableAccountCount: Int {
    accounts.filter { $0.availability == .all }.count
  }

  public var totalAccountCount: Int { accounts.count }
}

extension PoolRuntimeStatus {
  public func presentation(at now: Date = Date()) -> PoolPresentation {
    let rows = accounts.map { account in
      let placement = placement?.accounts.first { $0.id == account.id }
      let claims = Dictionary(
        uniqueKeysWithValues: LimitKind.allCases.map { kind in
          (kind, account.claim(for: kind, at: now))
        })
      let availability = account.availability(
        fiveHour: claims[.fiveHour] ?? nil,
        sevenDay: claims[.sevenDay] ?? nil,
        fable: claims[.fable] ?? nil,
        at: now
      )
      let limits = LimitKind.allCases.map { kind in
        let claim = claims[kind] ?? nil
        let utilization = claim?.utilization.map { min(max($0, 0), 1) }
        return LimitPresentation(
          kind: kind,
          remainingFraction: utilization.map { 1 - $0 },
          reset: claim?.resetDate(),
          exhausted: claim?.isHardExhausted(at: now) ?? false,
          forecast: depletionForecast(
            claim: claim,
            snapshotFresh: account.usageSnapshotFresh,
            windowSeconds: kind.windowSeconds,
            now: now
          )
        )
      }
      return AccountPresentation(
        id: account.id,
        plan: account.subscriptionType ?? "—",
        availability: availability,
        activeLaunches: placement?.activeLaunchLeases ?? 0,
        limits: limits,
        warning: account.lastError
      )
    }
    return PoolPresentation(poolID: poolID, accounts: rows)
  }
}

extension AccountRuntimeStatus {
  fileprivate func claim(for kind: LimitKind, at now: Date) -> LimitClaim? {
    switch kind {
    case .fiveHour:
      return claims.first {
        $0.key == "five_hour" && $0.scope.kind == "global" && $0.isCurrent(at: now)
      }
    case .sevenDay:
      return claims.first {
        $0.key == "seven_day" && $0.scope.kind == "global" && $0.isCurrent(at: now)
      }
    case .fable:
      return
        claims
        .filter {
          $0.scope.kind == "model" && $0.scope.value == "fable" && $0.isCurrent(at: now)
        }
        .max { left, right in
          pressure(of: left, at: now) < pressure(of: right, at: now)
        }
    }
  }

  fileprivate func availability(
    fiveHour: LimitClaim?,
    sevenDay: LimitClaim?,
    fable: LimitClaim?,
    at now: Date
  ) -> AccountAvailability {
    guard credentialHealth == "healthy" else { return .none }
    guard usageSnapshotFresh, completeGlobalSnapshot else { return .unknown }
    if [fiveHour, sevenDay].compactMap({ $0 }).contains(where: { $0.isHardExhausted(at: now) }) {
      return .none
    }
    if fable?.isHardExhausted(at: now) == true {
      return .nonFable
    }
    return .all
  }
}

private func pressure(of claim: LimitClaim, at now: Date) -> Double {
  if claim.isHardExhausted(at: now) { return 2 }
  return claim.utilization ?? -1
}

enum ResetTimestamp {
  static func parse(_ value: String) -> Date? {
    let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
    if let integer = Int64(trimmed) {
      let seconds = integer.magnitude >= 100_000_000_000 ? integer / 1_000 : integer
      return Date(timeIntervalSince1970: TimeInterval(seconds))
    }

    let fractional = ISO8601DateFormatter()
    fractional.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
    if let date = fractional.date(from: trimmed) { return date }

    let standard = ISO8601DateFormatter()
    standard.formatOptions = [.withInternetDateTime]
    return standard.date(from: trimmed)
  }
}
