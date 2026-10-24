import Foundation

public struct LimitResetStatus: Decodable, Sendable {
  public let checkedAtUnixMs: Int64
  public let fresh: Bool
  public let programs: [LimitResetProgram]

  enum CodingKeys: String, CodingKey {
    case checkedAtUnixMs = "checked_at_unix_ms"
    case fresh, programs
  }
}

public struct LimitResetProgram: Decodable, Sendable {
  public let program: String
  public let eligible: Bool
  public let reason: String?
  public let cooldownUntil: String?
  public let nextAvailableAt: String?
  public let grants: [LimitResetGrant]

  enum CodingKeys: String, CodingKey {
    case program, eligible, reason, grants
    case cooldownUntil = "cooldown_until"
    case nextAvailableAt = "next_available_at"
  }
}

public struct LimitResetGrant: Decodable, Sendable {
  public let id: String
  public let title: String
  public let remainingCount: UInt64
  public let usableNow: Bool
  public let requiresLimit: Bool
  public let clears: [String]
  public let startsAt: String?
  public let expiresAt: String?
  public let paused: Bool

  enum CodingKeys: String, CodingKey {
    case id, title, clears, paused
    case remainingCount = "remaining_count"
    case usableNow = "usable_now"
    case requiresLimit = "requires_limit"
    case startsAt = "starts_at"
    case expiresAt = "expires_at"
  }
}

public struct ResetOfferPresentation: Identifiable, Sendable {
  public let id: String
  public let title: String
  public let count: UInt64
  public let usableNow: Bool
  public let windows: [String]
  public let expiry: Date?
  public let details: String
}

public struct ResetInventoryPresentation: Sendable {
  public enum State: Equatable, Sendable {
    case unknown, stale, current
  }

  public let state: State
  public let bankedCount: UInt64?
  public let usableCount: UInt64?
  public let offers: [ResetOfferPresentation]
  public let programDetails: [String]
  public let hasWebOnlyOffers: Bool

  public static let manualRedemptionURL = URL(string: "https://claude.ai/settings/usage")!
  public static let manualRedemptionWarning =
    "Sign your browser into the intended Claude account before redeeming. Redemption is irreversible; Rayline never resets automatically."
  public static let coverageNote = "Check Claude Usage; web-only offers may exist."

  public var summary: String {
    switch state {
    case .unknown: "Saved resets: unknown"
    case .stale: "Saved resets (OAuth): \(bankedCount ?? 0) last known · usable unknown (stale)"
    case .current where hasWebOnlyOffers && bankedCount == 0:
      "Saved resets: check Claude Usage (web-only)"
    case .current: "Saved resets (OAuth): \(bankedCount ?? 0) banked · \(usableCount ?? 0) usable now"
    }
  }

  public init(inventory: LimitResetStatus?, at now: Date = Date()) {
    guard let inventory, !inventory.programs.isEmpty else {
      state = .unknown
      bankedCount = nil
      usableCount = nil
      offers = []
      programDetails = []
      hasWebOnlyOffers = false
      return
    }
    let age = now.timeIntervalSince1970 * 1_000 - Double(inventory.checkedAtUnixMs)
    let current = inventory.fresh && age >= 0 && age <= 600_000
    state = current ? .current : .stale
    hasWebOnlyOffers = inventory.programs.contains { $0.reason == "surface" }
    var banked: UInt64 = 0
    var usable: UInt64 = 0
    var rows: [ResetOfferPresentation] = []
    var programs: [String] = []
    for program in inventory.programs {
      var conditions: [String] = []
      if !program.eligible {
        conditions.append(program.reason == "surface" ? Self.coverageNote : "Ineligible: \(program.reason ?? "unspecified")")
      } else if let reason = program.reason {
        conditions.append(reason)
      }
      if let cooldown = program.cooldownUntil {
        conditions.append("Cooldown until \(Self.dateText(cooldown))")
      }
      if let next = program.nextAvailableAt {
        conditions.append("Next available \(Self.dateText(next))")
      }
      if !conditions.isEmpty {
        programs.append("\(program.program): \(conditions.joined(separator: " · "))")
      }
      let ready = program.eligible
        && Self.hasArrived(program.cooldownUntil, at: now)
        && Self.hasArrived(program.nextAvailableAt, at: now)
      for grant in program.grants where grant.remainingCount > 0 {
        let expiry = grant.expiresAt.flatMap(ResetTimestamp.parse)
        if grant.expiresAt != nil {
          guard let expiry, expiry > now else { continue }
        }
        banked = Self.add(banked, grant.remainingCount)
        let usableNow = current && ready && grant.usableNow && !grant.paused
          && Self.hasArrived(grant.startsAt, at: now)
          && (grant.expiresAt == nil || expiry != nil)
        if usableNow { usable = Self.add(usable, grant.remainingCount) }
        let windows = grant.clears.map { window in
          switch window {
          case "five_hour", "session": "session (5 hour)"
          case "seven_day", "weekly": "weekly (7 day)"
          default: window.replacingOccurrences(of: "_", with: " ")
          }
        }
        var details = ["\(grant.remainingCount) × \(grant.title)"]
        details.append(windows.isEmpty ? "Affected windows unspecified" : "Clears \(windows.joined(separator: ", "))")
        if let expires = grant.expiresAt {
          details.append("Expires \(Self.dateText(expires))")
        } else {
          details.append("No expiry reported")
        }
        if let starts = grant.startsAt { details.append("Starts \(Self.dateText(starts))") }
        if grant.paused { details.append("Paused") }
        if grant.requiresLimit { details.append("Requires reaching a limit") }
        details.append(current ? (usableNow ? "Usable now" : "Not usable now") : "Usability unknown (stale inventory)")
        rows.append(ResetOfferPresentation(
          id: "\(program.program):\(grant.id)", title: grant.title,
          count: grant.remainingCount, usableNow: usableNow, windows: windows,
          expiry: expiry, details: details.joined(separator: " · ")
        ))
      }
    }
    bankedCount = banked
    usableCount = current ? usable : nil
    offers = rows
    programDetails = programs
  }

  private static func hasArrived(_ timestamp: String?, at now: Date) -> Bool {
    guard let timestamp else { return true }
    guard let date = ResetTimestamp.parse(timestamp) else { return false }
    return date <= now
  }

  private static func add(_ left: UInt64, _ right: UInt64) -> UInt64 {
    let (sum, overflow) = left.addingReportingOverflow(right)
    return overflow ? UInt64.max : sum
  }

  private static func dateText(_ timestamp: String) -> String {
    guard let date = ResetTimestamp.parse(timestamp) else { return timestamp }
    let formatter = DateFormatter()
    formatter.locale = Locale(identifier: "en_US_POSIX")
    formatter.timeZone = TimeZone(secondsFromGMT: 0)
    formatter.dateFormat = "MMM d, HH:mm 'UTC'"
    return formatter.string(from: date)
  }
}
