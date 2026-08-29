import XCTest

@testable import RaylineStatusCore

final class ResetCountdownTests: XCTestCase {
  private let now = Date(timeIntervalSince1970: 1_893_499_200)

  func testFormatsMinutesHoursAndDays() {
    XCTAssertEqual(text(afterMinutes: 20), "20m")
    XCTAssertEqual(text(afterMinutes: 59), "59m")
    XCTAssertEqual(text(afterMinutes: 80), "1h 20m")
    XCTAssertEqual(text(afterMinutes: 5 * 60), "5h")
    XCTAssertEqual(text(afterMinutes: 23 * 60 + 59), "23h 59m")
    XCTAssertEqual(text(afterMinutes: 24 * 60), "1d")
    XCTAssertEqual(text(afterMinutes: 3 * 24 * 60 + 5 * 60), "3d 5h")
    XCTAssertEqual(text(afterMinutes: 6 * 24 * 60 + 23 * 60 + 30), "6d 23h")
  }

  func testRoundsPartialMinutesUpSoTheWindowIsNeverUnderstated() {
    XCTAssertEqual(ResetCountdown.text(until: now.addingTimeInterval(1), from: now), "1m")
    XCTAssertEqual(ResetCountdown.text(until: now.addingTimeInterval(61), from: now), "2m")
  }

  func testReportsMissingAndElapsedResets() {
    XCTAssertNil(ResetCountdown.text(until: nil, from: now))
    XCTAssertEqual(ResetCountdown.text(until: now, from: now), "due")
    XCTAssertEqual(ResetCountdown.text(until: now.addingTimeInterval(-60), from: now), "due")
  }

  private func text(afterMinutes minutes: Int) -> String? {
    ResetCountdown.text(until: now.addingTimeInterval(TimeInterval(minutes * 60)), from: now)
  }
}
