import SwiftUI

@main
struct RaylineStatusApp: App {
  @StateObject private var store = SubscriptionStore()

  var body: some Scene {
    MenuBarExtra {
      StatusMenuView(store: store)
    } label: {
      MenuBarStatusLabel(store: store)
        .task {
          await store.runRefreshLoop()
        }
    }
    .menuBarExtraStyle(.window)
  }
}
