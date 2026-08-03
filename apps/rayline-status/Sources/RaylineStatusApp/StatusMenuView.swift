import AppKit
import RaylineStatusCore
import SwiftUI

struct MenuBarStatusLabel: View {
  @ObservedObject var store: SubscriptionStore

  var body: some View {
    HStack(spacing: 4) {
      Image(
        systemName: store.errorMessage == nil ? "chart.bar.fill" : "exclamationmark.triangle.fill")
      Text(summaryText)
        .monospacedDigit()
    }
    .help("Rayline Claude subscription pool")
  }

  private var summaryText: String {
    guard let presentation = store.presentation else { return "—" }
    return "\(presentation.availableAccountCount)/\(presentation.totalAccountCount)"
  }
}

struct StatusMenuView: View {
  @ObservedObject var store: SubscriptionStore

  var body: some View {
    VStack(spacing: 0) {
      header
      Divider()
      content
      Divider()
      footer
    }
    .frame(width: 420)
  }

  private var header: some View {
    HStack(spacing: 12) {
      Image(systemName: "chart.bar.fill")
        .font(.system(size: 22, weight: .semibold))
        .foregroundStyle(.tint)
      VStack(alignment: .leading, spacing: 2) {
        Text("Claude subscriptions")
          .font(.headline)
        Text(headerDetail)
          .font(.caption)
          .foregroundStyle(.secondary)
      }
      Spacer()
      Button {
        Task { await store.refresh() }
      } label: {
        Image(systemName: "arrow.clockwise")
      }
      .buttonStyle(.borderless)
      .disabled(store.isRefreshing)
      .help("Refresh now")
    }
    .padding(14)
  }

  @ViewBuilder
  private var content: some View {
    if let presentation = store.presentation {
      ScrollView {
        LazyVStack(spacing: 10) {
          if let error = store.errorMessage {
            ErrorBanner(message: error)
          }
          ForEach(presentation.accounts) { account in
            AccountCard(account: account)
          }
        }
        .padding(12)
      }
      .frame(maxHeight: 620)
    } else if store.isRefreshing {
      VStack(spacing: 10) {
        ProgressView()
        Text("Reading live pool status…")
          .foregroundStyle(.secondary)
      }
      .frame(maxWidth: .infinity, minHeight: 180)
    } else {
      VStack(spacing: 10) {
        Image(systemName: "exclamationmark.triangle")
          .font(.title)
          .foregroundStyle(.orange)
        Text("Status unavailable")
          .font(.headline)
        Text(store.errorMessage ?? "Rayline has not returned a status snapshot yet.")
          .font(.caption)
          .foregroundStyle(.secondary)
          .multilineTextAlignment(.center)
          .frame(maxWidth: 320)
        Button("Try Again") {
          Task { await store.refresh() }
        }
      }
      .frame(maxWidth: .infinity, minHeight: 180)
      .padding()
    }
  }

  private var footer: some View {
    HStack {
      Text(updatedText)
        .font(.caption)
        .foregroundStyle(.secondary)
      Spacer()
      Button("Quit") {
        NSApplication.shared.terminate(nil)
      }
      .buttonStyle(.borderless)
    }
    .padding(.horizontal, 14)
    .padding(.vertical, 10)
  }

  private var headerDetail: String {
    guard let presentation = store.presentation else { return "Pool default" }
    return
      "Pool \(presentation.poolID) · \(presentation.availableAccountCount) of \(presentation.totalAccountCount) fully available"
  }

  private var updatedText: String {
    guard let date = store.lastUpdated else { return "Refreshes every minute" }
    return "Updated \(date.formatted(.relative(presentation: .named)))"
  }
}

private struct AccountCard: View {
  let account: AccountPresentation

  var body: some View {
    VStack(alignment: .leading, spacing: 10) {
      HStack(spacing: 8) {
        Text(account.id)
          .font(.headline.monospaced())
        Text(account.plan)
          .font(.caption)
          .foregroundStyle(.secondary)
        Spacer()
        if account.activeLaunches > 0 {
          Label("\(account.activeLaunches)", systemImage: "bolt.fill")
            .font(.caption.monospacedDigit())
            .foregroundStyle(.secondary)
        }
        AvailabilityBadge(availability: account.availability)
      }
      ForEach(account.limits) { limit in
        LimitRow(limit: limit)
      }
      if let warning = account.warning {
        Text(warning)
          .font(.caption2)
          .foregroundStyle(.orange)
          .lineLimit(2)
      }
    }
    .padding(12)
    .background(.quaternary.opacity(0.55), in: RoundedRectangle(cornerRadius: 10))
  }
}

private struct LimitRow: View {
  let limit: LimitPresentation

  var body: some View {
    VStack(spacing: 4) {
      HStack {
        Text(limit.kind.title)
          .font(.caption.weight(.medium))
          .frame(width: 48, alignment: .leading)
        if let remaining = limit.remainingFraction {
          ProgressView(value: min(max(remaining, 0), 1))
            .tint(limitColor)
        } else {
          Capsule()
            .fill(.quaternary)
            .frame(height: 4)
        }
        Text(remainingText)
          .font(.caption.monospacedDigit())
          .foregroundStyle(limitColor)
          .frame(width: 66, alignment: .trailing)
      }
      HStack {
        Spacer().frame(width: 56)
        Text(forecastText)
          .foregroundStyle(forecastColor)
        Spacer()
        Text(resetText)
          .foregroundStyle(.secondary)
      }
      .font(.caption2.monospacedDigit())
    }
  }

  private var remainingText: String {
    if limit.exhausted { return "exhausted" }
    guard let remaining = limit.remainingFraction else { return "—" }
    return String(format: "%.0f%%", remaining * 100)
  }

  private var resetText: String {
    guard let reset = limit.reset else { return "reset —" }
    return "reset \(UTCDateText.string(from: reset))"
  }

  private var forecastText: String {
    switch limit.forecast {
    case .exhausted: "exhausted"
    case .noBurn: "no burn"
    case .resetFirst: "reset first"
    case .runsOut(let date): "runs out \(UTCDateText.string(from: date))"
    case .learning: "learning rate"
    case .stale: "stale"
    case .unavailable: "forecast —"
    }
  }

  private var limitColor: Color {
    if limit.exhausted { return .red }
    guard let remaining = limit.remainingFraction else { return .secondary }
    if remaining <= 0.1 { return .red }
    if remaining <= 0.3 { return .orange }
    return .green
  }

  private var forecastColor: Color {
    switch limit.forecast {
    case .exhausted: .red
    case .runsOut: .orange
    default: .secondary
    }
  }
}

private struct AvailabilityBadge: View {
  let availability: AccountAvailability

  var body: some View {
    Text(availability.label)
      .font(.caption2.weight(.semibold))
      .padding(.horizontal, 7)
      .padding(.vertical, 3)
      .foregroundStyle(color)
      .background(color.opacity(0.12), in: Capsule())
  }

  private var color: Color {
    switch availability {
    case .all: .green
    case .nonFable: .orange
    case .none: .red
    case .unknown: .secondary
    }
  }
}

private struct ErrorBanner: View {
  let message: String

  var body: some View {
    Label(message, systemImage: "exclamationmark.triangle.fill")
      .font(.caption)
      .foregroundStyle(.orange)
      .frame(maxWidth: .infinity, alignment: .leading)
      .padding(9)
      .background(.orange.opacity(0.1), in: RoundedRectangle(cornerRadius: 8))
  }
}

@MainActor
private enum UTCDateText {
  private static let formatter: DateFormatter = {
    let formatter = DateFormatter()
    formatter.locale = Locale(identifier: "en_US_POSIX")
    formatter.timeZone = TimeZone(secondsFromGMT: 0)
    formatter.dateFormat = "MMM d HH:mm'Z'"
    return formatter
  }()

  static func string(from date: Date) -> String {
    formatter.string(from: date)
  }
}
