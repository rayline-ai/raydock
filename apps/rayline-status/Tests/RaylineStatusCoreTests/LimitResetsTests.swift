import Foundation
import XCTest
@testable import RaylineStatusCore

final class LimitResetsTests: XCTestCase {
  private let now = Date(timeIntervalSince1970: 1_900_000_000)

  func testOldAndNewDaemonStatusDecoding() throws {
    let old = try decodeAccount(inventory: nil)
    XCTAssertNil(old.limitResets)
    let unknown = ResetInventoryPresentation(inventory: old.limitResets, at: now)
    XCTAssertEqual(unknown.state, .unknown)
    XCTAssertNil(unknown.bankedCount)
    XCTAssertNil(unknown.usableCount)
    let unevaluated = ResetInventoryPresentation(inventory: LimitResetStatus(
      checkedAtUnixMs: 1_900_000_000_000, fresh: true, programs: []
    ), at: now)
    XCTAssertEqual(unevaluated.state, .unknown)
    XCTAssertNil(unevaluated.bankedCount)
    XCTAssertNil(unevaluated.usableCount)

    let account = try decodeAccount(inventory: #"{"checked_at_unix_ms":1900000000000,"fresh":true,"programs":[{"program":"cedar_ember","eligible":true,"reason":null,"cooldown_until":null,"next_available_at":null,"grants":[{"id":"saved","title":"Saved session reset","remaining_count":2,"usable_now":true,"requires_limit":false,"clears":["five_hour"],"starts_at":null,"expires_at":"2030-03-18T00:00:00Z","paused":false}]}]}"#)
    let status = try XCTUnwrap(account.limitResets)
    XCTAssertEqual(status.checkedAtUnixMs, 1_900_000_000_000)
    XCTAssertEqual(status.programs.first?.grants.first?.remainingCount, 2)
    let pool = PoolRuntimeStatus(poolID: "default", accounts: [account], placement: nil)
    let row = try XCTUnwrap(pool.presentation(at: now).accounts.first)
    XCTAssertEqual(row.limitResets.state, .current)
    XCTAssertEqual(row.limitResets.bankedCount, 2)
    XCTAssertEqual(row.limitResets.usableCount, 2)
    XCTAssertEqual(row.limitResets.offers.first?.windows, ["session (5 hour)"])
    XCTAssertNotNil(row.limitResets.offers.first?.expiry)
  }

  func testFreshnessIncludesEndpointsButNotFutureChecksOrOlderInventory() {
    for age in [0.0, 600.0] {
      let result = present(checked: now.addingTimeInterval(-age))
      XCTAssertEqual(result.state, .current)
      XCTAssertEqual(result.usableCount, 1)
    }
    for age in [-0.001, 600.001] {
      let result = present(checked: now.addingTimeInterval(-age))
      XCTAssertEqual(result.state, .stale)
      XCTAssertEqual(result.bankedCount, 1)
      XCTAssertNil(result.usableCount)
      XCTAssertFalse(result.offers[0].usableNow)
    }
    let partial = present(fresh: false)
    XCTAssertEqual(partial.state, .stale)
    XCTAssertNil(partial.usableCount)
  }

  func testBankedIncludesPausedAndFutureGrantsButNotExpiredOrEmptyGrants() {
    let grants = [
      grant(id: "ready", count: 2),
      grant(id: "paused", count: 3, paused: true),
      grant(id: "future", count: 4, starts: stamp(now.addingTimeInterval(1))),
      grant(id: "expired", count: 8, expires: stamp(now)),
      grant(id: "empty", count: 0),
      grant(id: "invalid-expiry", count: 7, expires: "invalid"),
      grant(id: "server-blocked", count: 5, usable: false, requiresLimit: true),
    ]
    let result = present(grants: grants)
    XCTAssertEqual(result.bankedCount, 14)
    XCTAssertEqual(result.usableCount, 2)
    XCTAssertEqual(Set(result.offers.filter(\.usableNow).map(\.id)), ["test:ready"])
    XCTAssertEqual(result.offers.count, 4)
  }

  func testStartAndCooldownBoundaries() {
    XCTAssertEqual(present(grants: [grant(starts: stamp(now))]).usableCount, 1)
    XCTAssertEqual(present(cooldown: stamp(now)).usableCount, 1)
    XCTAssertEqual(present(cooldown: stamp(now.addingTimeInterval(1))).usableCount, 0)
    XCTAssertEqual(present(next: stamp(now.addingTimeInterval(1))).usableCount, 0)
    XCTAssertEqual(present(cooldown: "invalid").usableCount, 0)
    XCTAssertEqual(present(grants: [grant(expires: "invalid")]).usableCount, 0)
  }

  func testWebOnlyEligibilityDoesNotClaimUsableOAuthOffers() {
    let result = present(eligible: false, reason: "surface")
    XCTAssertTrue(result.hasWebOnlyOffers)
    XCTAssertEqual(result.bankedCount, 1)
    XCTAssertEqual(result.usableCount, 0)
    let gated = present(eligible: false, reason: "surface", grants: [])
    XCTAssertTrue(gated.hasWebOnlyOffers)
    XCTAssertEqual(gated.bankedCount, 0)
    XCTAssertEqual(gated.usableCount, 0)
    XCTAssertFalse(result.programDetails.isEmpty)
    let unknown = ResetInventoryPresentation(inventory: nil, at: now)
    XCTAssertNil(unknown.bankedCount)
    let empty = present(grants: [])
    XCTAssertEqual(empty.bankedCount, 0)
    XCTAssertEqual(empty.usableCount, 0)
  }

  func testCountOverflowSaturatesRatherThanTrapping() {
    let result = present(grants: [grant(id: "a", count: .max), grant(id: "b", count: 1)])
    XCTAssertEqual(result.bankedCount, UInt64.max)
    XCTAssertEqual(result.usableCount, UInt64.max)
  }

  private func present(
    checked: Date? = nil, fresh: Bool = true, eligible: Bool = true,
    reason: String? = nil, cooldown: String? = nil, next: String? = nil,
    grants: [LimitResetGrant]? = nil
  ) -> ResetInventoryPresentation {
    ResetInventoryPresentation(inventory: LimitResetStatus(
      checkedAtUnixMs: Int64(((checked ?? now).timeIntervalSince1970 * 1_000).rounded()),
      fresh: fresh,
      programs: [LimitResetProgram(program: "test", eligible: eligible, reason: reason,
        cooldownUntil: cooldown, nextAvailableAt: next, grants: grants ?? [grant()])]
    ), at: now)
  }

  private func grant(
    id: String = "saved", count: UInt64 = 1, usable: Bool = true,
    requiresLimit: Bool = false, paused: Bool = false,
    starts: String? = nil, expires: String? = nil
  ) -> LimitResetGrant {
    LimitResetGrant(id: id, title: "Saved reset", remainingCount: count,
      usableNow: usable, requiresLimit: requiresLimit, clears: ["seven_day"],
      startsAt: starts, expiresAt: expires, paused: paused)
  }

  private func stamp(_ date: Date) -> String {
    ISO8601DateFormatter().string(from: date)
  }

  private func decodeAccount(inventory: String?) throws -> AccountRuntimeStatus {
    let field = inventory.map { ",\"limit_resets\":\($0)" } ?? ""
    let json = """
      {"id":"af","credential_health":"healthy","subscription_type":"max",
       "usage_snapshot_fresh":true,"complete_global_snapshot":true,"claims":[],
       "last_error":null\(field)}
      """
    return try JSONDecoder().decode(AccountRuntimeStatus.self, from: Data(json.utf8))
  }
}
