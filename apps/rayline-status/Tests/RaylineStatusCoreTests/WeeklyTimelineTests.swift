import XCTest

@testable import RaylineStatusCore

final class WeeklyTimelineTests: XCTestCase {
  private let now = Date(timeIntervalSince1970: 1_900_000_000)

  private func account(_ id: String, dry: TimeInterval?, reset: TimeInterval?, stale: Bool = false)
    -> AccountPresentation
  {
    let resetDate = reset.map { now.addingTimeInterval($0 * 3600) }
    let forecast: DepletionForecast =
      stale
      ? .stale
      : dry.map {
        $0 <= 0 ? .exhausted : .runsOut(now.addingTimeInterval($0 * 3600), .windowAverage)
      } ?? .resetFirst
    let limit = LimitPresentation(
      kind: .sevenDay, remainingFraction: dry == 0 ? 0 : 0.4,
      reset: resetDate, resetCountdown: nil, exhausted: dry == 0,
      forecast: forecast, runOutCountdown: nil)
    return AccountPresentation(
      id: id, plan: "max", availability: .unknown, activeLaunches: 0,
      limits: [limit], warning: nil,
      limitResets: ResetInventoryPresentation(inventory: nil, at: now))
  }

  private func timeline(_ accounts: [AccountPresentation]) -> WeeklyTimeline {
    WeeklyTimeline(
      pool: PoolGlance(PoolPresentation(poolID: "default", accounts: accounts), now: now), now: now)
  }

  func testGapRunsFromLastDepletionToFirstResetAndUnknownQualifiesIt() throws {
    let result = timeline([
      account("af", dry: 0, reset: 47), account("mx", dry: 0, reset: 72),
      account("cl", dry: 26, reset: 81), account("ws", dry: 41, reset: 95),
      account("mt", dry: nil, reset: 70, stale: true),
    ])
    let gap = try XCTUnwrap(result.gap)
    XCTAssertEqual(gap.start, now.addingTimeInterval(41 * 3600))
    XCTAssertEqual(gap.end, now.addingTimeInterval(47 * 3600))
    XCTAssertEqual(result.unknownCount, 1)
    XCTAssertEqual(result.accounts.map(\.accountID), ["af", "mx", "cl", "ws", "mt"])
  }

  func testNoGapWhenResetPrecedesLastDepletionOrAnotherAccountLastsUntilReset() {
    for accounts in [
      [account("a", dry: 0, reset: 10), account("b", dry: 11, reset: 20)],
      [account("a", dry: 0, reset: 10), account("b", dry: 10, reset: 20)],
      [account("a", dry: 0, reset: 10), account("b", dry: nil, reset: 20)],
    ] {
      XCTAssertNil(timeline(accounts).gap)
    }
  }

  func testUnknownOrUntimedAccountsCannotInventAnOutage() {
    for accounts in [
      [], [account("a", dry: nil, reset: 10, stale: true)],
      [account("a", dry: 0, reset: 10, stale: true)],
      [account("a", dry: 0, reset: nil)], [account("a", dry: 0, reset: -1)],
    ] {
      XCTAssertNil(timeline(accounts).gap)
    }
    let result = timeline([account("a", dry: -2, reset: 3)])
    XCTAssertEqual(result.gap?.start, now)
    XCTAssertEqual(result.gap?.duration, 3 * 3600)
  }

  func testLocalMidnightsStayAlignedAcrossClockChange() throws {
    var calendar = Calendar(identifier: .gregorian)
    calendar.timeZone = try XCTUnwrap(TimeZone(identifier: "Europe/London"))
    let date = try XCTUnwrap(ResetTimestamp.parse("2026-10-23T09:21:00+01:00"))
    let result = WeeklyTimeline(
      pool: PoolGlance(PoolPresentation(poolID: "default", accounts: []), now: date), now: date)
    let ticks = result.dayTicks(calendar: calendar)
    XCTAssertTrue(ticks.allSatisfy { calendar.component(.hour, from: $0) == 0 })
    XCTAssertTrue(zip(ticks, ticks.dropFirst()).contains { $1.timeIntervalSince($0) == 25 * 3600 })
    XCTAssertEqual(result.fraction(date.addingTimeInterval(-1)), 0)
    XCTAssertEqual(result.fraction(result.end.addingTimeInterval(1)), 1)
  }
}
