import Combine
import Foundation
import RaylineStatusCore

@MainActor
final class SubscriptionStore: ObservableObject {
  @Published private(set) var status: PoolRuntimeStatus?
  @Published private(set) var lastUpdated: Date?
  @Published private(set) var errorMessage: String?
  @Published private(set) var isRefreshing = false
  /// Display clock. Countdowns keep ticking even when a fetch fails.
  @Published private(set) var now = Date()
  /// Measured burn per allowance, recomputed on every display tick.
  @Published private(set) var burnRates: [BurnRateKey: BurnRate] = [:]

  private let client = RaylineStatusClient()
  private let historyFile = UsageHistoryFile()
  private var history: UsageHistory
  private var refreshLoopRunning = false
  private let poolID = "default"
  private static let refreshInterval: TimeInterval = 60
  private static let tickInterval: UInt64 = 15_000_000_000

  init() {
    history = historyFile.load()
    burnRates = history.burnRates(at: now)
  }

  var presentation: PoolPresentation? {
    status?.presentation(at: now, burnRates: burnRates)
  }

  func refresh() async {
    guard !isRefreshing else { return }
    isRefreshing = true
    defer { isRefreshing = false }
    do {
      let fetched = try await client.fetch(poolID: poolID)
      let fetchedAt = Date()
      status = fetched
      lastUpdated = fetchedAt
      errorMessage = nil
      history.record(fetched, at: fetchedAt)
      historyFile.save(history)
      burnRates = history.burnRates(at: fetchedAt)
    } catch {
      errorMessage = error.localizedDescription
    }
  }

  func runRefreshLoop() async {
    guard !refreshLoopRunning else { return }
    refreshLoopRunning = true
    defer { refreshLoopRunning = false }
    var lastRefresh = Date.distantPast
    while !Task.isCancelled {
      now = Date()
      // A backward clock jump would otherwise park the loop for the length of
      // the jump. Treat it as a reason to fetch, not a reason to wait.
      if now < lastRefresh { lastRefresh = .distantPast }
      if now.timeIntervalSince(lastRefresh) >= Self.refreshInterval {
        lastRefresh = now
        await refresh()
        now = Date()
      }
      // Memory only. The projected run-out keeps moving between fetches.
      burnRates = history.burnRates(at: now)
      do {
        try await Task.sleep(nanoseconds: Self.tickInterval)
      } catch {
        return
      }
    }
  }
}
