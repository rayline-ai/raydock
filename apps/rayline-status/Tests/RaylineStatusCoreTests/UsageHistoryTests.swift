import XCTest

@testable import RaylineStatusCore

final class UsageHistoryTests: XCTestCase {
  private let start = Date(timeIntervalSince1970: 1_893_499_200)
  private var scratch: URL?

  override func tearDownWithError() throws {
    if let scratch {
      try? FileManager.default.removeItem(at: scratch)
    }
    scratch = nil
  }

  // MARK: - Thinning

  func testHoldsBackRepeatedSamplesUntilSomethingChanges() {
    var history = UsageHistory()
    history.record(status(utilization: 0.10), at: start)
    history.record(status(utilization: 0.10), at: at(60))
    history.record(status(utilization: 0.10), at: at(120))
    history.record(status(utilization: 0.20), at: at(660))

    let rate = fiveHourRate(history, at: at(660))
    XCTAssertEqual(rate?.sampleCount, 2)
    XCTAssertEqual(rate?.span, 660)
  }

  func testRecordsAChangeImmediately() {
    var history = UsageHistory()
    history.record(status(utilization: 0.10), at: start)
    history.record(status(utilization: 0.20), at: at(60))
    history.record(status(utilization: 0.20), at: at(700))

    let rate = fiveHourRate(history, at: at(700))
    XCTAssertEqual(rate?.sampleCount, 3)
    XCTAssertEqual(rate?.fractionPerSecond ?? 0, 0.10 / 700, accuracy: 1e-12)
  }

  func testHeartbeatRecordsAFlatPeriodSoItMeasuresAsNoBurn() {
    var history = UsageHistory()
    history.record(status(utilization: 0.40), at: start)
    history.record(status(utilization: 0.40), at: at(60))
    history.record(status(utilization: 0.40), at: at(300))
    history.record(status(utilization: 0.40), at: at(700))

    let rate = fiveHourRate(history, at: at(700))
    XCTAssertEqual(rate?.sampleCount, 3)
    XCTAssertEqual(rate?.fractionPerSecond, 0)
  }

  func testABackwardClockJumpRestartsTheSeriesOnTheNewTimeline() {
    var history = UsageHistory()
    history.record(status(utilization: 0.10), at: start)
    history.record(status(utilization: 0.30), at: at(900))
    XCTAssertEqual(fiveHourRate(history, at: at(900))?.sampleCount, 2)

    // The clock falls back an hour. The stranded future samples must go, or
    // they would keep feeding a rate that no later fetch can refresh.
    history.record(status(utilization: 0.05), at: at(-3_600))
    XCTAssertNil(fiveHourRate(history, at: at(-3_600)))

    history.record(status(utilization: 0.15), at: at(-2_700))
    let rate = fiveHourRate(history, at: at(-2_700))
    XCTAssertEqual(rate?.sampleCount, 2)
    XCTAssertEqual(rate?.span, 900)
    XCTAssertEqual(rate?.fractionPerSecond ?? 0, 0.10 / 900, accuracy: 1e-12)
    XCTAssertEqual(rate?.measuredAt, at(-2_700))
  }

  func testTheRateReportsTheTimeOfItsNewestSample() {
    var history = UsageHistory()
    history.record(status(utilization: 0.10), at: start)
    history.record(status(utilization: 0.30), at: at(900))

    XCTAssertEqual(fiveHourRate(history, at: at(1_500))?.measuredAt, at(900))
  }

  func testEachLiveWindowOfTheSameKindKeepsItsOwnRate() {
    var history = UsageHistory()
    let near = at(3 * 60 * 60)
    let far = at(7 * 24 * 60 * 60)
    // The pool reports two live Fable claims. Recording follows whichever the
    // popover displays, so both series can be alive at once.
    history.record(fableStatus(near: (0.50, near), far: (0.10, far)), at: start)
    history.record(fableStatus(near: (0.53, near), far: (0.10, far)), at: at(3_000))
    history.record(fableStatus(near: (0.53, near), far: (0.60, far)), at: at(3_600))
    history.record(fableStatus(near: (0.53, near), far: (0.90, far)), at: at(6_600))

    let rates = history.burnRates(at: at(6_600))
    XCTAssertEqual(rates.count, 2)
    let nearRate = rates[BurnRateKey(account: "af", kind: .fable, window: near)]
    let farRate = rates[BurnRateKey(account: "af", kind: .fable, window: far)]
    XCTAssertEqual(nearRate?.fractionPerSecond ?? 0, 0.03 / 3_000, accuracy: 1e-12)
    XCTAssertEqual(farRate?.fractionPerSecond ?? 0, 0.30 / 3_000, accuracy: 1e-12)
  }

  // MARK: - Window rollover

  func testANewWindowStartsAFreshSeries() {
    var history = UsageHistory()
    let firstReset = at(3_600)
    history.record(status(utilization: 0.50, reset: firstReset), at: start)
    history.record(status(utilization: 0.90, reset: firstReset), at: at(600))
    XCTAssertEqual(
      fiveHourRate(history, at: at(600), window: firstReset)?.fractionPerSecond, 0.40 / 600)

    let secondReset = at(3_700 + 5 * 60 * 60)
    history.record(status(utilization: 0.05, reset: secondReset), at: at(3_700))
    XCTAssertNil(fiveHourRate(history, at: at(3_700), window: secondReset))
    // The rolled-over window is gone, not merely outvoted.
    XCTAssertNil(fiveHourRate(history, at: at(3_700), window: firstReset))

    history.record(status(utilization: 0.10, reset: secondReset), at: at(4_400))
    let rate = fiveHourRate(history, at: at(4_400), window: secondReset)
    XCTAssertEqual(rate?.sampleCount, 2)
    XCTAssertEqual(rate?.span, 700)
    XCTAssertEqual(rate?.fractionPerSecond ?? 0, 0.05 / 700, accuracy: 1e-12)
  }

  // MARK: - Rate math

  func testRateUsesTheEndpointsOfTheRetainedSeries() {
    var history = UsageHistory()
    history.record(status(utilization: 0.10), at: start)
    history.record(status(utilization: 0.50), at: at(300))
    history.record(status(utilization: 0.30), at: at(900))

    let rate = fiveHourRate(history, at: at(900))
    XCTAssertEqual(rate?.sampleCount, 3)
    XCTAssertEqual(rate?.fractionPerSecond ?? 0, 0.20 / 900, accuracy: 1e-12)
  }

  func testAFallingUtilizationClampsToZeroRatherThanNegativeBurn() {
    var history = UsageHistory()
    history.record(status(utilization: 0.50), at: start)
    history.record(status(utilization: 0.20), at: at(900))

    XCTAssertEqual(fiveHourRate(history, at: at(900))?.fractionPerSecond, 0)
  }

  func testRefusesToMeasureBelowTheMinimumSpanOrWithOneSample() {
    var history = UsageHistory()
    history.record(status(utilization: 0.10), at: start)
    XCTAssertNil(fiveHourRate(history, at: start))

    history.record(status(utilization: 0.20), at: at(300))
    XCTAssertNil(fiveHourRate(history, at: at(300)))

    history.record(status(utilization: 0.30), at: at(601))
    XCTAssertNotNil(fiveHourRate(history, at: at(601)))
  }

  func testWeeklyWindowsNeedALongerSpanThanTheFiveHourWindow() {
    var history = UsageHistory()
    let reset = at(3 * 24 * 60 * 60)
    history.record(status(utilization: 0.10, key: "seven_day", reset: reset), at: start)
    history.record(status(utilization: 0.20, key: "seven_day", reset: reset), at: at(1_800))
    let key = BurnRateKey(account: "af", kind: .sevenDay, window: reset)
    XCTAssertNil(history.burnRate(for: key, at: at(1_800)))

    history.record(status(utilization: 0.30, key: "seven_day", reset: reset), at: at(3_000))
    XCTAssertNotNil(history.burnRate(for: key, at: at(3_000)))
  }

  // MARK: - Retention

  func testDropsSamplesOlderThanTheLookback() {
    var history = UsageHistory()
    history.record(status(utilization: 0.10), at: start)
    history.record(status(utilization: 0.20), at: at(600))
    // 60 minutes in, everything before the 45 minute lookback is gone.
    history.record(status(utilization: 0.30), at: at(3_600))
    XCTAssertNil(fiveHourRate(history, at: at(3_600)))

    history.record(status(utilization: 0.34), at: at(4_300))
    let rate = fiveHourRate(history, at: at(4_300))
    XCTAssertEqual(rate?.sampleCount, 2)
    XCTAssertEqual(rate?.fractionPerSecond ?? 0, 0.04 / 700, accuracy: 1e-12)
  }

  func testCapsASeriesAtTwoHundredSamplesAndDropsTheOldestFirst() {
    var history = UsageHistory()
    let reset = at(5 * 60 * 60)
    for step in 0..<205 {
      let utilization = 0.10 + Double(step) * 0.001
      history.record(
        status(utilization: utilization, reset: reset), at: at(TimeInterval(step * 5)))
    }

    let rate = fiveHourRate(history, at: at(204 * 5), window: reset)
    XCTAssertEqual(rate?.sampleCount, 200)
    // The retained window starts at the sixth sample, 5 * 5 seconds in.
    XCTAssertEqual(rate?.span, 199 * 5)
    let firstRetained: Double = 0.10 + 5 * 0.001
    let lastRecorded: Double = 0.10 + 204 * 0.001
    let expected: Double = (lastRecorded - firstRetained) / (199 * 5)
    XCTAssertEqual(rate?.fractionPerSecond ?? 0, expected, accuracy: 1e-12)
  }

  // MARK: - Persistence

  func testFileRoundTripsAMeasurableHistory() throws {
    let url = try temporaryFile()
    var history = UsageHistory()
    history.record(status(utilization: 0.10), at: start)
    history.record(status(utilization: 0.30), at: at(900))

    let file = UsageHistoryFile(fileURL: url)
    file.save(history)
    let loaded = UsageHistoryFile(fileURL: url).load()

    XCTAssertEqual(fiveHourRate(loaded, at: at(900)), fiveHourRate(history, at: at(900)))
    XCTAssertEqual(loaded.burnRates(at: at(900)).count, 1)
  }

  func testAMissingFileLoadsAsEmptyHistory() throws {
    let url = try temporaryFile()
    XCTAssertTrue(UsageHistoryFile(fileURL: url).load().burnRates(at: start).isEmpty)
  }

  func testCorruptOrUnknownContentLoadsAsEmptyHistory() throws {
    let url = try temporaryFile()
    try Data("{ not json".utf8).write(to: url)
    XCTAssertTrue(UsageHistoryFile(fileURL: url).load().burnRates(at: start).isEmpty)

    try Data(#"{"v":99,"s":[]}"#.utf8).write(to: url)
    XCTAssertTrue(UsageHistoryFile(fileURL: url).load().burnRates(at: start).isEmpty)
  }

  func testSavingIntoAMissingDirectoryCreatesIt() throws {
    let url = try temporaryFile(subdirectory: "nested/deeper")
    var history = UsageHistory()
    history.record(status(utilization: 0.10), at: start)
    history.record(status(utilization: 0.30), at: at(900))

    UsageHistoryFile(fileURL: url).save(history)
    XCTAssertTrue(FileManager.default.fileExists(atPath: url.path))
  }

  // MARK: - Helpers

  private func at(_ offset: TimeInterval) -> Date {
    start.addingTimeInterval(offset)
  }

  private func fiveHourRate(
    _ history: UsageHistory,
    at now: Date,
    window: Date? = nil
  ) -> BurnRate? {
    let key = BurnRateKey(
      account: "af", kind: .fiveHour, window: window ?? at(5 * 60 * 60))
    return history.burnRate(for: key, at: now)
  }

  private func temporaryFile(subdirectory: String? = nil) throws -> URL {
    let root = FileManager.default.temporaryDirectory
      .appendingPathComponent("usage-history-tests-\(UUID().uuidString)", isDirectory: true)
    scratch = root
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    let directory = subdirectory.map { root.appendingPathComponent($0, isDirectory: true) } ?? root
    return directory.appendingPathComponent("usage-history.json", isDirectory: false)
  }

  /// Two live Fable claims, the way the pool reports an overage claim and a
  /// scoped claim. The popover shows whichever carries more pressure.
  private func fableStatus(
    near: (Double, Date),
    far: (Double, Date)
  ) -> PoolRuntimeStatus {
    PoolRuntimeStatus(
      poolID: "default",
      accounts: [
        AccountRuntimeStatus(
          id: "af",
          credentialHealth: "healthy",
          subscriptionType: "max",
          usageSnapshotFresh: true,
          completeGlobalSnapshot: true,
          claims: [
            fableClaim(utilization: near.0, reset: near.1),
            fableClaim(utilization: far.0, reset: far.1),
          ],
          lastError: nil
        )
      ],
      placement: nil
    )
  }

  private func fableClaim(utilization: Double, reset: Date) -> LimitClaim {
    LimitClaim(
      key: "weekly",
      scope: ClaimScope(kind: "model", value: "fable"),
      utilization: utilization,
      status: "allowed",
      resetsAt: String(Int64(reset.timeIntervalSince1970))
    )
  }

  private func status(
    utilization: Double,
    key: String = "five_hour",
    reset: Date? = nil
  ) -> PoolRuntimeStatus {
    let resetsAt = reset ?? at(5 * 60 * 60)
    return PoolRuntimeStatus(
      poolID: "default",
      accounts: [
        AccountRuntimeStatus(
          id: "af",
          credentialHealth: "healthy",
          subscriptionType: "max",
          usageSnapshotFresh: true,
          completeGlobalSnapshot: true,
          claims: [
            LimitClaim(
              key: key,
              scope: ClaimScope(kind: "global", value: nil),
              utilization: utilization,
              status: "allowed",
              resetsAt: String(Int64(resetsAt.timeIntervalSince1970))
            )
          ],
          lastError: nil
        )
      ],
      placement: nil
    )
  }
}
