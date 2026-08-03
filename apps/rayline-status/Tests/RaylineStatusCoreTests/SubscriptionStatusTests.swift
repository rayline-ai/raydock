import XCTest

@testable import RaylineStatusCore

final class SubscriptionStatusTests: XCTestCase {
  func testClientUsesTheNonCredentialFallbackStatusMode() {
    XCTAssertEqual(
      RaylineStatusClient.arguments(poolID: "work"),
      ["subscriptions", "status", "--pool", "work", "--json", "--live-only"]
    )
  }

  func testDecodesLiveStatusAndBuildsAccountPresentation() throws {
    let status = try JSONDecoder().decode(PoolRuntimeStatus.self, from: Data(fixture.utf8))
    let now = try XCTUnwrap(ResetTimestamp.parse("2030-01-01T12:00:00Z"))
    let presentation = status.presentation(at: now)

    XCTAssertEqual(presentation.poolID, "default")
    XCTAssertEqual(presentation.availableAccountCount, 1)
    XCTAssertEqual(presentation.totalAccountCount, 2)
    XCTAssertEqual(presentation.accounts[0].availability, .all)
    XCTAssertEqual(presentation.accounts[0].activeLaunches, 1)
    XCTAssertEqual(
      presentation.accounts[0].limits.first { $0.kind == .fable }?.remainingFraction, 0.8)
    XCTAssertEqual(presentation.accounts[1].availability, .none)
  }

  func testForecastDistinguishesRiskFromResetFirst() throws {
    let status = try JSONDecoder().decode(PoolRuntimeStatus.self, from: Data(fixture.utf8))
    let now = try XCTUnwrap(ResetTimestamp.parse("2030-01-01T12:00:00Z"))
    let account = status.accounts[0]
    let fiveHour = account.claims[0]
    let sevenDay = account.claims[1]

    XCTAssertEqual(
      depletionForecast(
        claim: fiveHour,
        snapshotFresh: true,
        windowSeconds: 5 * 60 * 60,
        now: now
      ),
      .runsOut(try XCTUnwrap(ResetTimestamp.parse("2030-01-01T14:00:00Z")))
    )
    XCTAssertEqual(
      depletionForecast(
        claim: sevenDay,
        snapshotFresh: true,
        windowSeconds: 7 * 24 * 60 * 60,
        now: now
      ),
      .resetFirst
    )
  }

  private let fixture = #"""
    {
      "pool_id": "default",
      "accounts": [
        {
          "id": "af",
          "credential_health": "healthy",
          "subscription_type": "max",
          "usage_snapshot_fresh": true,
          "complete_global_snapshot": true,
          "claims": [
            {"key":"five_hour","scope":{"kind":"global"},"utilization":0.5,"status":"allowed","resets_at":"2030-01-01T15:00:00Z"},
            {"key":"seven_day","scope":{"kind":"global"},"utilization":0.1,"status":"allowed","resets_at":"2030-01-07T10:00:00Z"},
            {"key":"weekly","scope":{"kind":"model","value":"fable"},"utilization":0.2,"status":"allowed","resets_at":"2030-01-07T10:00:00Z"}
          ],
          "last_error": null
        },
        {
          "id": "mx",
          "credential_health": "healthy",
          "subscription_type": "max",
          "usage_snapshot_fresh": true,
          "complete_global_snapshot": true,
          "claims": [
            {"key":"five_hour","scope":{"kind":"global"},"utilization":0.0,"status":"allowed","resets_at":null},
            {"key":"seven_day","scope":{"kind":"global"},"utilization":1.0,"status":"allowed","resets_at":"2030-01-02T02:00:00Z"},
            {"key":"weekly","scope":{"kind":"model","value":"fable"},"utilization":1.0,"status":"allowed","resets_at":"2030-01-03T21:00:00Z"}
          ],
          "last_error": null
        }
      ],
      "placement": {
        "active_lease_ttl_seconds": 900,
        "accounts": [
          {"id":"af","active_launch_leases":1,"active_model_leases":{"fable":1}},
          {"id":"mx","active_launch_leases":0,"active_model_leases":{}}
        ]
      }
    }
    """#
}
