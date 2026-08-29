import XCTest

@testable import RaylineStatusCore

final class StatusTextTests: XCTestCase {
  private let now = Date(timeIntervalSince1970: 1_893_499_200)

  // MARK: - How much is left

  func testAnUnknownUtilizationSaysSoRatherThanRenderingADash() {
    let limit = make(remaining: nil, forecast: .unavailable)

    XCTAssertEqual(StatusText.allowance(limit), "usage unknown")
    XCTAssertEqual(StatusText.cellValue(limit), "—")
    XCTAssertNil(StatusText.cellUnit(limit))
    XCTAssertEqual(
      StatusText.tooltip(limit, resetDateText: "Mar 3, 14:00 UTC", runOutDateText: "unknown"),
      """
      5 hour: usage unknown
      Resets in 3d 5h · Mar 3, 14:00 UTC
      Forecast unavailable
      """
    )
  }

  func testASpentAllowanceReadsAsExhausted() {
    let limit = make(remaining: 0, exhausted: true, forecast: .exhausted)

    XCTAssertEqual(StatusText.allowance(limit), "exhausted")
    XCTAssertEqual(StatusText.cellValue(limit), "OUT")
    XCTAssertNil(StatusText.cellUnit(limit))
    XCTAssertEqual(StatusText.remainingSummary(limit), "OUT")
  }

  func testANormalAllowanceReadsAsAPercentage() {
    let limit = make(remaining: 0.62, forecast: .resetFirst)

    XCTAssertEqual(StatusText.allowance(limit), "62% left")
    XCTAssertEqual(StatusText.cellValue(limit), "62")
    XCTAssertEqual(StatusText.cellUnit(limit), "%")
    XCTAssertEqual(StatusText.remainingSummary(limit), "62% LEFT")
  }

  // MARK: - Which countdown the cell shows

  func testAnAtRiskCellShowsTheProjectedTimeToEmpty() {
    let limit = make(
      remaining: 0.62,
      runOutCountdown: "1h 20m",
      forecast: .runsOut(now.addingTimeInterval(4_800), .measured(2_700))
    )

    XCTAssertEqual(StatusText.cellCountdown(limit), "1h 20m")
  }

  func testAHealthyCellShowsTheResetCountdown() {
    let limit = make(remaining: 0.62, forecast: .resetFirst)

    XCTAssertEqual(StatusText.cellCountdown(limit), "3d 5h")
  }

  func testAnAtRiskCellFallsBackToTheResetCountdownWhenTheProjectionIsMissing() {
    let limit = make(
      remaining: 0.62,
      runOutCountdown: nil,
      forecast: .runsOut(now.addingTimeInterval(4_800), .measured(2_700))
    )

    XCTAssertEqual(StatusText.cellCountdown(limit), "3d 5h")
  }

  func testAMissingLimitShowsADash() {
    XCTAssertEqual(StatusText.cellCountdown(nil), "—")
    XCTAssertEqual(StatusText.cellValue(nil), "—")
    XCTAssertEqual(
      StatusText.tooltip(nil, resetDateText: "unknown", runOutDateText: "unknown"),
      "Allowance unavailable"
    )
  }

  // MARK: - The tooltip names both numbers

  func testAnAtRiskTooltipGivesBothTheRunOutAndTheReset() {
    let limit = make(
      remaining: 0.62,
      runOutCountdown: "1h 20m",
      forecast: .runsOut(now.addingTimeInterval(4_800), .measured(2_700))
    )

    XCTAssertEqual(
      StatusText.tooltip(
        limit, resetDateText: "Mar 3, 14:00 UTC", runOutDateText: "Mar 1, 09:20 UTC"),
      """
      5 hour: 62% left
      May run out in 1h 20m at current burn (last 45m)
      Resets in 3d 5h · Mar 3, 14:00 UTC
      """
    )
  }

  func testTheWindowAverageBasisIsNamedDifferently() {
    let limit = make(
      remaining: 0.62,
      runOutCountdown: "1h 20m",
      forecast: .runsOut(now.addingTimeInterval(4_800), .windowAverage)
    )

    XCTAssertEqual(
      StatusText.tooltip(
        limit, resetDateText: "Mar 3, 14:00 UTC", runOutDateText: "Mar 1, 09:20 UTC"),
      """
      5 hour: 62% left
      May run out in 1h 20m at average burn
      Resets in 3d 5h · Mar 3, 14:00 UTC
      """
    )
    XCTAssertEqual(StatusText.burnBasisPhrase(.windowAverage), "at average burn")
    XCTAssertEqual(
      StatusText.burnBasisPhrase(.measured(4_800)), "at current burn (last 1h 20m)")
  }

  func testTheHoverLineNamesTheBasisToo() {
    let limit = make(
      remaining: 0.62,
      runOutCountdown: "1h 20m",
      forecast: .runsOut(now.addingTimeInterval(4_800), .measured(2_700))
    )

    XCTAssertEqual(
      StatusText.limitTiming(limit, resetDateText: "Mar 3 14:00"),
      "May run out in 1h 20m at current burn (last 45m) · resets in 3d 5h"
    )
    XCTAssertEqual(
      StatusText.limitTiming(make(remaining: 1, forecast: .noBurn), resetDateText: "Mar 3 14:00"),
      "No current burn · resets in 3d 5h · Mar 3 14:00 UTC"
    )
  }

  func testTheForecastNoteSpellsOutEveryOutcome() {
    let runOut = DepletionForecast.runsOut(now, .measured(2_700))
    XCTAssertEqual(
      StatusText.forecastNote(runOut, runOutDateText: "Mar 1, 09:20 UTC"),
      "Risk: projected to run out Mar 1, 09:20 UTC at current burn (last 45m)"
    )
    XCTAssertEqual(StatusText.forecastNote(.noBurn, runOutDateText: ""), "No current consumption")
    XCTAssertEqual(
      StatusText.forecastNote(.stale, runOutDateText: ""),
      "Forecast unavailable because usage is stale"
    )
  }

  // MARK: - What the account row talks about next

  func testTheNextEventPrefersExhaustedThenRiskThenReset() throws {
    let exhausted = make(
      kind: .sevenDay,
      remaining: 0,
      exhausted: true,
      reset: now.addingTimeInterval(7_200),
      forecast: .exhausted
    )
    let risk = make(
      remaining: 0.2,
      reset: now.addingTimeInterval(3_600),
      runOutCountdown: "20m",
      forecast: .runsOut(now.addingTimeInterval(1_200), .measured(2_700))
    )
    let healthy = make(
      kind: .fable, remaining: 0.9, reset: now.addingTimeInterval(600), forecast: .resetFirst)

    if case .exhausted(let limit) = StatusText.nextEvent(for: account([exhausted, risk, healthy]))
    {
      XCTAssertEqual(limit.kind, .sevenDay)
    } else {
      XCTFail("an exhausted allowance outranks every other event")
    }

    if case .risk(let limit, _, _) = StatusText.nextEvent(for: account([risk, healthy])) {
      XCTAssertEqual(limit.kind, .fiveHour)
    } else {
      XCTFail("a projected run-out outranks a plain reset")
    }

    if case .reset(let limit) = StatusText.nextEvent(for: account([healthy])) {
      XCTAssertEqual(limit.kind, .fable)
    } else {
      XCTFail("a healthy account still reports its next reset")
    }

    if case .none = StatusText.nextEvent(for: account([])) {
    } else {
      XCTFail("an account with no limits has no next event")
    }
  }

  func testTheSoonestOfEachKindWins() throws {
    let early = make(reset: now.addingTimeInterval(600), resetCountdown: "10m")
    let late = make(kind: .fable, reset: now.addingTimeInterval(7_200), resetCountdown: "2h")

    let event = StatusText.nextEvent(for: account([late, early]))
    XCTAssertEqual(event.date, now.addingTimeInterval(600))
    XCTAssertEqual(
      StatusText.accountTiming(event, dateText: "Mar 3 14:00"),
      "5 hour resets in 10m · Mar 3 14:00 UTC"
    )
  }

  func testEachEventComposesItsOwnSentence() {
    let exhausted = make(
      kind: .sevenDay, remaining: 0, exhausted: true, resetCountdown: "2h", forecast: .exhausted)
    let risk = make(
      remaining: 0.2,
      runOutCountdown: "20m",
      forecast: .runsOut(now.addingTimeInterval(1_200), .measured(2_700))
    )

    let exhaustedEvent = StatusText.nextEvent(for: account([exhausted]))
    XCTAssertEqual(
      StatusText.accountTiming(exhaustedEvent, dateText: "Mar 3 14:00"),
      "7 day back in 2h · Mar 3 14:00 UTC"
    )
    XCTAssertEqual(
      StatusText.nextEventDescription(exhaustedEvent, dateText: "Mar 3, 14:00 UTC"),
      "Next: 7 day allowance resets in 2h (Mar 3, 14:00 UTC)"
    )

    let riskEvent = StatusText.nextEvent(for: account([risk]))
    XCTAssertEqual(riskEvent.date, now.addingTimeInterval(1_200))
    XCTAssertEqual(
      StatusText.accountTiming(riskEvent, dateText: "Mar 3 14:00"),
      "5 hour may hit its limit in 20m at current burn (last 45m)"
    )
    XCTAssertEqual(
      StatusText.nextEventDescription(riskEvent, dateText: "Mar 1, 09:20 UTC"),
      "Next risk: 5 hour allowance may run out in 20m at current burn (last 45m) (Mar 1, 09:20 UTC)"
    )

    let healthyEvent = StatusText.nextEvent(for: account([make(resetCountdown: "3d 5h")]))
    XCTAssertEqual(
      StatusText.nextEventDescription(healthyEvent, dateText: "Mar 3, 14:00 UTC"),
      "Next: 5 hour allowance resets in 3d 5h (Mar 3, 14:00 UTC); no earlier depletion projected"
    )
    XCTAssertEqual(
      StatusText.accountTiming(.none, dateText: "unknown"), "Timing unavailable")
    XCTAssertEqual(
      StatusText.nextEventDescription(.none, dateText: "unknown"), "Next event unavailable")
  }

  // MARK: - Helpers

  private func make(
    kind: LimitKind = .fiveHour,
    remaining: Double? = 0.62,
    exhausted: Bool = false,
    reset: Date? = nil,
    resetCountdown: String? = "3d 5h",
    runOutCountdown: String? = nil,
    forecast: DepletionForecast = .resetFirst
  ) -> LimitPresentation {
    LimitPresentation(
      kind: kind,
      remainingFraction: remaining,
      reset: reset ?? now.addingTimeInterval(3 * 24 * 60 * 60),
      resetCountdown: resetCountdown,
      exhausted: exhausted,
      forecast: forecast,
      runOutCountdown: runOutCountdown
    )
  }

  private func account(_ limits: [LimitPresentation]) -> AccountPresentation {
    AccountPresentation(
      id: "af",
      plan: "max",
      availability: .all,
      activeLaunches: 1,
      limits: limits,
      warning: nil
    )
  }
}
