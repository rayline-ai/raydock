import XCTest

@testable import RaylineStatusCore

final class WeeklyGlanceTests: XCTestCase {
  private func pool() throws -> PoolGlance {
    let status = try JSONDecoder().decode(PoolRuntimeStatus.self, from: Data(fixture.utf8))
    let now = try XCTUnwrap(ResetTimestamp.parse("2030-01-01T12:00:00Z"))
    return PoolGlance(status.presentation(at: now), now: now)
  }

  private func glance(_ id: String) throws -> WeeklyGlance {
    try XCTUnwrap(pool().accounts.first { $0.accountID == id })
  }

  func testWeeklyStateFollowsTheSevenDayForecast() throws {
    XCTAssertEqual(
      try glance("fast").state,
      .runningFast(runOut: try XCTUnwrap(ResetTimestamp.parse("2030-01-02T20:00:00Z"))))
    XCTAssertEqual(
      try glance("out").state,
      .out(back: try XCTUnwrap(ResetTimestamp.parse("2030-01-02T21:00:00Z"))))
    XCTAssertEqual(try glance("steady").state, .onPace)
    XCTAssertEqual(try glance("idle").state, .idle)
    XCTAssertEqual(try glance("stale").state, .unknown)
  }

  func testPaceMarkIsTheShareOfTheWeekStillToGo() throws {
    XCTAssertEqual(try XCTUnwrap(glance("steady").paceMark), 4.0 / 7.0, accuracy: 1e-9)
    XCTAssertEqual(try XCTUnwrap(glance("fast").paceMark), 3.0 / 7.0, accuracy: 1e-9)
    XCTAssertNil(try glance("out").paceMark)
    XCTAssertNil(try glance("idle").paceMark)
  }

  func testFiveHourAndFableOnlyFlagWhenTheyNeedAttention() throws {
    XCTAssertEqual(try glance("fast").flags.map(\.text), ["5h 32m", "Fable"])
    XCTAssertEqual(try glance("fast").flags.map(\.tone), [.warning, .warning])
    XCTAssertEqual(try glance("out").flags.map(\.text), ["5h out"])
    XCTAssertEqual(try glance("out").flags.map(\.tone), [.critical])
    XCTAssertEqual(try glance("steady").flags, [])
  }

  func testUrgencyPutsOutThenEarliestRunOutFirst() throws {
    let pool = try pool()
    XCTAssertEqual(
      pool.byUrgency.map(\.accountID), ["out", "late", "fast", "steady", "idle", "stale"])
    XCTAssertEqual(pool.runningFastCount, 2)
    XCTAssertEqual(pool.outCount, 1)
    XCTAssertEqual(pool.firstRunOut?.account.accountID, "late")
  }

  func testMenuBarKeepsPoolOrderAndDropsTheLeastUrgentPastTheLimit() throws {
    let pool = try pool()
    XCTAssertEqual(pool.menuBarAccounts(limit: 10).map(\.accountID), pool.accounts.map(\.accountID))
    XCTAssertEqual(
      pool.menuBarAccounts(limit: 5).map(\.accountID), ["fast", "out", "steady", "idle", "late"])
    XCTAssertEqual(pool.menuBarAccounts(limit: 2).map(\.accountID), ["out", "late"])
  }

  func testChipCountdownStaysShort() {
    let now = Date(timeIntervalSince1970: 0)
    XCTAssertEqual(chipCountdown(until: now.addingTimeInterval(31 * 60 + 10), from: now), "32m")
    XCTAssertEqual(chipCountdown(until: now.addingTimeInterval(2 * 3600 + 21 * 60), from: now), "~2h")
    XCTAssertEqual(chipCountdown(until: now, from: now), "1m")
  }

  private let fixture = #"""
    {
      "pool_id": "default",
      "accounts": [
        {
          "id": "fast", "credential_health": "healthy", "subscription_type": "max",
          "usage_snapshot_fresh": true, "complete_global_snapshot": true, "last_error": null,
          "claims": [
            {"key":"five_hour","scope":{"kind":"global"},"utilization":0.85,"status":"allowed","resets_at":"2030-01-01T14:00:00Z"},
            {"key":"seven_day","scope":{"kind":"global"},"utilization":0.75,"status":"allowed","resets_at":"2030-01-04T12:00:00Z"},
            {"key":"weekly","scope":{"kind":"model","value":"fable"},"utilization":0.9,"status":"allowed","resets_at":"2030-01-04T12:00:00Z"}
          ]
        },
        {
          "id": "out", "credential_health": "healthy", "subscription_type": "pro",
          "usage_snapshot_fresh": true, "complete_global_snapshot": true, "last_error": null,
          "claims": [
            {"key":"five_hour","scope":{"kind":"global"},"utilization":1.0,"status":"rejected","resets_at":"2030-01-01T13:18:00Z"},
            {"key":"seven_day","scope":{"kind":"global"},"utilization":1.0,"status":"rejected","resets_at":"2030-01-02T21:00:00Z"}
          ]
        },
        {
          "id": "steady", "credential_health": "healthy", "subscription_type": "max",
          "usage_snapshot_fresh": true, "complete_global_snapshot": true, "last_error": null,
          "claims": [
            {"key":"five_hour","scope":{"kind":"global"},"utilization":0.1,"status":"allowed","resets_at":"2030-01-01T16:00:00Z"},
            {"key":"seven_day","scope":{"kind":"global"},"utilization":0.35,"status":"allowed","resets_at":"2030-01-05T12:00:00Z"}
          ]
        },
        {
          "id": "idle", "credential_health": "healthy", "subscription_type": "max",
          "usage_snapshot_fresh": true, "complete_global_snapshot": true, "last_error": null,
          "claims": [
            {"key":"five_hour","scope":{"kind":"global"},"utilization":0.0,"status":"allowed","resets_at":null},
            {"key":"seven_day","scope":{"kind":"global"},"utilization":0.0,"status":"allowed","resets_at":null}
          ]
        },
        {
          "id": "stale", "credential_health": "healthy", "subscription_type": "max",
          "usage_snapshot_fresh": false, "complete_global_snapshot": true, "last_error": null,
          "claims": [
            {"key":"seven_day","scope":{"kind":"global"},"utilization":0.5,"status":"allowed","resets_at":"2030-01-03T12:00:00Z"}
          ]
        },
        {
          "id": "late", "credential_health": "healthy", "subscription_type": "max",
          "usage_snapshot_fresh": true, "complete_global_snapshot": true, "last_error": null,
          "claims": [
            {"key":"seven_day","scope":{"kind":"global"},"utilization":0.9,"status":"allowed","resets_at":"2030-01-02T08:00:00Z"}
          ]
        }
      ],
      "placement": null
    }
    """#
}
