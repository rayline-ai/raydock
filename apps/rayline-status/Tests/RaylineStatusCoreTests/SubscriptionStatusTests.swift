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

  func testPresentationCarriesResetCountdownsForEveryWindow() throws {
    let status = try JSONDecoder().decode(PoolRuntimeStatus.self, from: Data(fixture.utf8))
    let now = try XCTUnwrap(ResetTimestamp.parse("2030-01-01T12:00:00Z"))
    let limits = status.presentation(at: now).accounts[0].limits

    XCTAssertEqual(limits.first { $0.kind == .fiveHour }?.resetCountdown, "3h")
    XCTAssertEqual(limits.first { $0.kind == .sevenDay }?.resetCountdown, "5d 22h")
    XCTAssertEqual(limits.first { $0.kind == .fable }?.resetCountdown, "5d 22h")
    XCTAssertEqual(limits.first { $0.kind == .fiveHour }?.isAtRisk, true)
    XCTAssertEqual(limits.first { $0.kind == .sevenDay }?.isAtRisk, false)
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
      .runsOut(try XCTUnwrap(ResetTimestamp.parse("2030-01-01T14:00:00Z")), .windowAverage)
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

  func testAMeasuredBurnBeatsTheWindowAverageWhenTheRecentPaceIsFaster() throws {
    let status = try JSONDecoder().decode(PoolRuntimeStatus.self, from: Data(fixture.utf8))
    let now = try XCTUnwrap(ResetTimestamp.parse("2030-01-01T12:00:00Z"))
    let fiveHour = status.accounts[0].claims[0]
    // Half the allowance is gone and the last 30 minutes burned a full
    // allowance per hour, so the window average of 14:00 is too optimistic.
    let measured = BurnRate(
      fractionPerSecond: 1.0 / 3_600, span: 1_800, sampleCount: 4, measuredAt: now)

    XCTAssertEqual(
      depletionForecast(
        claim: fiveHour,
        snapshotFresh: true,
        windowSeconds: 5 * 60 * 60,
        burnRate: measured,
        now: now
      ),
      .runsOut(try XCTUnwrap(ResetTimestamp.parse("2030-01-01T12:30:00Z")), .measured(1_800))
    )
  }

  func testAMeasuredRateOfZeroExpectsTheWindowToResetFirst() throws {
    let status = try JSONDecoder().decode(PoolRuntimeStatus.self, from: Data(fixture.utf8))
    let now = try XCTUnwrap(ResetTimestamp.parse("2030-01-01T12:00:00Z"))
    let fiveHour = status.accounts[0].claims[0]

    XCTAssertEqual(
      depletionForecast(
        claim: fiveHour,
        snapshotFresh: true,
        windowSeconds: 5 * 60 * 60,
        burnRate: BurnRate(
          fractionPerSecond: 0, span: 1_800, sampleCount: 4, measuredAt: now),
        now: now
      ),
      .resetFirst
    )
  }

  func testAMeasuredRateSkipsTheLearningGuardEarlyInAWindow() throws {
    let now = try XCTUnwrap(ResetTimestamp.parse("2030-01-01T12:00:00Z"))
    let claim = LimitClaim(
      key: "five_hour",
      scope: ClaimScope(kind: "global", value: nil),
      utilization: 0.1,
      status: "allowed",
      resetsAt: "2030-01-01T16:45:00Z"
    )

    XCTAssertEqual(
      depletionForecast(
        claim: claim,
        snapshotFresh: true,
        windowSeconds: 5 * 60 * 60,
        now: now
      ),
      .learning
    )
    XCTAssertEqual(
      depletionForecast(
        claim: claim,
        snapshotFresh: true,
        windowSeconds: 5 * 60 * 60,
        burnRate: BurnRate(
          fractionPerSecond: 0.9 / 3_600, span: 900, sampleCount: 3, measuredAt: now),
        now: now
      ),
      .runsOut(try XCTUnwrap(ResetTimestamp.parse("2030-01-01T13:00:00Z")), .measured(900))
    )
  }

  func testPresentationCarriesARunOutCountdownOnlyForAnAtRiskLimit() throws {
    let status = try JSONDecoder().decode(PoolRuntimeStatus.self, from: Data(fixture.utf8))
    let now = try XCTUnwrap(ResetTimestamp.parse("2030-01-01T12:00:00Z"))
    let window = try XCTUnwrap(ResetTimestamp.parse("2030-01-01T15:00:00Z"))
    let rates = [
      BurnRateKey(account: "af", kind: .fiveHour, window: window):
        BurnRate(fractionPerSecond: 1.0 / 3_600, span: 2_700, sampleCount: 6, measuredAt: now)
    ]
    let limits = status.presentation(at: now, burnRates: rates).accounts[0].limits

    let fiveHour = try XCTUnwrap(limits.first { $0.kind == .fiveHour })
    XCTAssertEqual(fiveHour.isAtRisk, true)
    XCTAssertEqual(fiveHour.runOutCountdown, "30m")
    XCTAssertEqual(fiveHour.resetCountdown, "3h")

    let sevenDay = try XCTUnwrap(limits.first { $0.kind == .sevenDay })
    XCTAssertEqual(sevenDay.isAtRisk, false)
    XCTAssertNil(sevenDay.runOutCountdown)
  }

  func testARateFromAnotherWindowIsNotAppliedToThisOne() throws {
    let status = try JSONDecoder().decode(PoolRuntimeStatus.self, from: Data(fixture.utf8))
    let now = try XCTUnwrap(ResetTimestamp.parse("2030-01-01T12:00:00Z"))
    let otherWindow = try XCTUnwrap(ResetTimestamp.parse("2030-01-01T16:00:00Z"))
    let rates = [
      BurnRateKey(account: "af", kind: .fiveHour, window: otherWindow):
        BurnRate(fractionPerSecond: 1.0 / 3_600, span: 2_700, sampleCount: 6, measuredAt: now)
    ]
    let limits = status.presentation(at: now, burnRates: rates).accounts[0].limits
    let fiveHour = try XCTUnwrap(limits.first { $0.kind == .fiveHour })

    // The displayed claim resets at 15:00, so the 16:00 rate is a different
    // allowance. The forecast falls back to the window average.
    XCTAssertEqual(
      fiveHour.forecast,
      .runsOut(try XCTUnwrap(ResetTimestamp.parse("2030-01-01T14:00:00Z")), .windowAverage)
    )
  }

  func testTheDisplayedFableClaimUsesItsOwnWindowRate() throws {
    let start = try XCTUnwrap(ResetTimestamp.parse("2030-01-01T12:00:00Z"))
    let near = start.addingTimeInterval(3 * 60 * 60)
    let far = start.addingTimeInterval(7 * 24 * 60 * 60)

    var history = UsageHistory()
    history.record(fablePool(near: (0.50, near), far: (0.10, far)), at: start)
    history.record(
      fablePool(near: (0.53, near), far: (0.10, far)), at: start.addingTimeInterval(3_000))
    history.record(
      fablePool(near: (0.53, near), far: (0.60, far)), at: start.addingTimeInterval(3_600))

    let now = start.addingTimeInterval(6_600)
    let pool = fablePool(near: (0.53, near), far: (0.90, far))
    history.record(pool, at: now)

    let limits = pool.presentation(at: now, burnRates: history.burnRates(at: now)).accounts[0]
      .limits
    let fable = try XCTUnwrap(limits.first { $0.kind == .fable })

    // The popover shows the far window, at 0.90 used and 0.30 burned in the
    // last 3000 seconds. That is 1000 seconds of allowance left. The near
    // window, which resets sooner, was burning 100 times slower.
    XCTAssertEqual(fable.forecast, .runsOut(now.addingTimeInterval(1_000), .measured(3_000)))
    XCTAssertEqual(fable.runOutCountdown, "17m")
  }

  func testAProjectionHoldsStillBetweenFetches() throws {
    let status = try JSONDecoder().decode(PoolRuntimeStatus.self, from: Data(fixture.utf8))
    let fetchedAt = try XCTUnwrap(ResetTimestamp.parse("2030-01-01T12:00:00Z"))
    let window = try XCTUnwrap(ResetTimestamp.parse("2030-01-01T15:00:00Z"))
    // Half the allowance is left and it is burning out over 1830 seconds.
    let rates = [
      BurnRateKey(account: "af", kind: .fiveHour, window: window):
        BurnRate(
          fractionPerSecond: 0.5 / 1_830, span: 1_800, sampleCount: 6, measuredAt: fetchedAt)
    ]

    func fiveHour(at now: Date) throws -> LimitPresentation {
      let limits = status.presentation(at: now, burnRates: rates).accounts[0].limits
      return try XCTUnwrap(limits.first { $0.kind == .fiveHour })
    }

    let onFetch = try fiveHour(at: fetchedAt)
    let laterTick = try fiveHour(at: fetchedAt.addingTimeInterval(30))

    // The clock ticked but no new utilization arrived, so the moment of
    // depletion is unchanged and the countdown to it has shrunk.
    XCTAssertEqual(onFetch.forecast, laterTick.forecast)
    XCTAssertEqual(
      onFetch.forecast, .runsOut(fetchedAt.addingTimeInterval(1_830), .measured(1_800)))
    XCTAssertEqual(onFetch.runOutCountdown, "31m")
    XCTAssertEqual(laterTick.runOutCountdown, "30m")
  }

  /// Two live Fable claims, the way the pool reports an overage claim and a
  /// scoped claim. The popover shows whichever carries more pressure.
  private func fablePool(near: (Double, Date), far: (Double, Date)) -> PoolRuntimeStatus {
    func claim(_ utilization: Double, _ reset: Date) -> LimitClaim {
      LimitClaim(
        key: "weekly",
        scope: ClaimScope(kind: "model", value: "fable"),
        utilization: utilization,
        status: "allowed",
        resetsAt: String(Int64(reset.timeIntervalSince1970))
      )
    }
    return PoolRuntimeStatus(
      poolID: "default",
      accounts: [
        AccountRuntimeStatus(
          id: "af",
          credentialHealth: "healthy",
          subscriptionType: "max",
          usageSnapshotFresh: true,
          completeGlobalSnapshot: true,
          claims: [claim(near.0, near.1), claim(far.0, far.1)],
          lastError: nil
        )
      ],
      placement: nil
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
