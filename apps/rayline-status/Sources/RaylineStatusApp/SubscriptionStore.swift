import Combine
import Foundation
import RaylineStatusCore

@MainActor
final class SubscriptionStore: ObservableObject {
  @Published private(set) var status: PoolRuntimeStatus?
  @Published private(set) var lastUpdated: Date?
  @Published private(set) var errorMessage: String?
  @Published private(set) var isRefreshing = false

  private let client = RaylineStatusClient()
  private var refreshLoopRunning = false
  private let poolID = "default"

  var presentation: PoolPresentation? {
    status?.presentation()
  }

  func refresh() async {
    guard !isRefreshing else { return }
    isRefreshing = true
    defer { isRefreshing = false }
    do {
      status = try await client.fetch(poolID: poolID)
      lastUpdated = Date()
      errorMessage = nil
    } catch {
      errorMessage = error.localizedDescription
    }
  }

  func runRefreshLoop() async {
    guard !refreshLoopRunning else { return }
    refreshLoopRunning = true
    defer { refreshLoopRunning = false }
    while !Task.isCancelled {
      await refresh()
      do {
        try await Task.sleep(nanoseconds: 60_000_000_000)
      } catch {
        return
      }
    }
  }
}
