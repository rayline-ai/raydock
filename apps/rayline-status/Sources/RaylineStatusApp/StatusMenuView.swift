import AppKit
import RaylineStatusCore
import SwiftUI

private enum Palette {
  static let railBlue = Color(red: 0.30, green: 0.55, blue: 1.00)
  static let capacityMint = Color(red: 0.28, green: 0.78, blue: 0.56)
  static let signalAmber = Color(red: 0.95, green: 0.66, blue: 0.23)
  static let exhaustRed = Color(red: 0.94, green: 0.36, blue: 0.37)
  static let slate = Color(red: 0.55, green: 0.58, blue: 0.66)
}

struct MenuBarStatusLabel: View {
  @ObservedObject var store: SubscriptionStore

  var body: some View {
    HStack(spacing: 4) {
      Image(
        systemName: store.errorMessage == nil
          ? "arrow.triangle.branch" : "exclamationmark.triangle.fill")
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
    .frame(width: 620)
  }

  private var header: some View {
    HStack(spacing: 10) {
      ZStack {
        RoundedRectangle(cornerRadius: 7)
          .fill(Palette.railBlue.opacity(0.14))
        Image(systemName: "arrow.triangle.branch")
          .font(.system(size: 15, weight: .semibold))
          .foregroundStyle(Palette.railBlue)
      }
      .frame(width: 30, height: 30)

      VStack(alignment: .leading, spacing: 1) {
        Text("Claude pool")
          .font(.system(.headline, design: .rounded, weight: .semibold))
        Text("\(store.presentation?.poolID ?? "default") · UTC")
          .font(.caption.monospaced())
          .foregroundStyle(.secondary)
      }

      Spacer()
      if let presentation = store.presentation {
        HStack(spacing: 4) {
          Text("\(presentation.availableAccountCount) / \(presentation.totalAccountCount)")
            .font(.caption.monospacedDigit().weight(.semibold))
          Text("ready")
            .font(.caption)
        }
        .foregroundStyle(readinessColor(presentation))
        .padding(.horizontal, 8)
        .padding(.vertical, 4)
        .background(readinessColor(presentation).opacity(0.11), in: Capsule())
      }
      Button {
        Task { await store.refresh() }
      } label: {
        Image(systemName: "arrow.clockwise")
      }
      .buttonStyle(.borderless)
      .disabled(store.isRefreshing)
      .help("Refresh now")
    }
    .padding(.horizontal, 12)
    .padding(.vertical, 9)
  }

  @ViewBuilder
  private var content: some View {
    if let presentation = store.presentation {
      VStack(spacing: 0) {
        if let error = store.errorMessage {
          ErrorBanner(message: error)
          Divider()
        }
        HStack(alignment: .top, spacing: 0) {
          ForEach(Array(presentation.accounts.enumerated()), id: \.element.id) { index, account in
            if index > 0 {
              Divider()
            }
            AccountLane(account: account)
              .frame(maxWidth: .infinity)
          }
        }
      }
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
          .foregroundStyle(Palette.signalAmber)
        Text("Status unavailable")
          .font(.headline)
        Text(store.errorMessage ?? "Start Claude through Rayline, then refresh.")
          .font(.caption)
          .foregroundStyle(.secondary)
          .multilineTextAlignment(.center)
          .frame(maxWidth: 360)
        Button("Try Again") {
          Task { await store.refresh() }
        }
      }
      .frame(maxWidth: .infinity, minHeight: 180)
      .padding()
    }
  }

  private var footer: some View {
    HStack(spacing: 10) {
      Circle()
        .fill(store.errorMessage == nil ? Palette.capacityMint : Palette.signalAmber)
        .frame(width: 5, height: 5)
      Text(updatedText)
        .font(.caption2.monospaced())
        .foregroundStyle(.secondary)
      Spacer()
      Text("Refreshes every minute")
        .font(.caption2)
        .foregroundStyle(.tertiary)
      Button("Quit") {
        NSApplication.shared.terminate(nil)
      }
      .buttonStyle(.borderless)
      .font(.caption)
    }
    .padding(.horizontal, 12)
    .padding(.vertical, 7)
  }

  private func readinessColor(_ presentation: PoolPresentation) -> Color {
    presentation.availableAccountCount == presentation.totalAccountCount
      ? Palette.capacityMint : Palette.signalAmber
  }

  private var updatedText: String {
    guard let date = store.lastUpdated else { return "Waiting for first refresh" }
    return "Updated \(date.formatted(.relative(presentation: .named)))"
  }
}

private struct AccountLane: View {
  let account: AccountPresentation

  var body: some View {
    VStack(alignment: .leading, spacing: 10) {
      accountHeader
      ZStack(alignment: .leading) {
        Rectangle()
          .fill(availabilityColor.opacity(0.23))
          .frame(width: 1)
          .padding(.vertical, 9)
          .offset(x: 3)
        VStack(spacing: 10) {
          ForEach(account.limits) { limit in
            CompactLimitRow(limit: limit)
          }
        }
      }
      if let warning = account.warning {
        Text(warning)
          .font(.caption2)
          .foregroundStyle(Palette.signalAmber)
          .lineLimit(1)
      }
    }
    .padding(.horizontal, 12)
    .padding(.vertical, 10)
  }

  private var accountHeader: some View {
    HStack(spacing: 6) {
      Text(account.id)
        .font(.system(.headline, design: .rounded, weight: .bold))
      Text(account.plan.uppercased())
        .font(.system(size: 9, weight: .medium, design: .monospaced))
        .foregroundStyle(.secondary)
      if account.activeLaunches > 0 {
        Label("\(account.activeLaunches)", systemImage: "bolt.fill")
          .labelStyle(.titleAndIcon)
          .font(.caption2.monospacedDigit())
          .foregroundStyle(Palette.railBlue)
      }
      Spacer(minLength: 4)
      HStack(spacing: 4) {
        Circle()
          .fill(availabilityColor)
          .frame(width: 5, height: 5)
        Text(availabilityText)
          .font(.caption2.weight(.medium))
      }
      .foregroundStyle(availabilityColor)
    }
  }

  private var availabilityText: String {
    switch account.availability {
    case .all: "ready"
    case .nonFable: "no Fable"
    case .none: "blocked"
    case .unknown: "unknown"
    }
  }

  private var availabilityColor: Color {
    switch account.availability {
    case .all: Palette.capacityMint
    case .nonFable: Palette.signalAmber
    case .none: Palette.exhaustRed
    case .unknown: Palette.slate
    }
  }
}

private struct CompactLimitRow: View {
  let limit: LimitPresentation

  var body: some View {
    HStack(alignment: .top, spacing: 7) {
      Circle()
        .fill(limitColor)
        .frame(width: 7, height: 7)
        .overlay(Circle().stroke(.background, lineWidth: 1.5))
        .padding(.top, 4)
      VStack(spacing: 3) {
        HStack(alignment: .firstTextBaseline) {
          Text(limitLabel)
            .font(.system(size: 10, weight: .semibold, design: .monospaced))
            .foregroundStyle(.secondary)
          Spacer()
          Text(remainingText)
            .font(.system(size: 13, weight: .semibold, design: .monospaced))
            .foregroundStyle(limitColor)
        }
        ProgressView(value: progressValue)
          .progressViewStyle(.linear)
          .tint(limitColor)
          .scaleEffect(x: 1, y: 0.65, anchor: .center)
        Text(detailText)
          .font(.system(size: 9, weight: .regular, design: .monospaced))
          .foregroundStyle(detailColor)
          .lineLimit(1)
          .minimumScaleFactor(0.72)
          .frame(maxWidth: .infinity, alignment: .leading)
      }
    }
  }

  private var limitLabel: String {
    switch limit.kind {
    case .fiveHour: "5H"
    case .sevenDay: "7D"
    case .fable: "FABLE"
    }
  }

  private var progressValue: Double {
    limit.exhausted ? 0 : min(max(limit.remainingFraction ?? 0, 0), 1)
  }

  private var remainingText: String {
    if limit.exhausted { return "OUT" }
    guard let remaining = limit.remainingFraction else { return "—" }
    return String(format: "%.0f%%", remaining * 100)
  }

  private var detailText: String {
    "\(forecastText) · ↻ \(resetText)"
  }

  private var resetText: String {
    guard let reset = limit.reset else { return "—" }
    return UTCDateText.compactString(from: reset)
  }

  private var forecastText: String {
    switch limit.forecast {
    case .exhausted: "exhausted"
    case .noBurn: "idle"
    case .resetFirst: "renews first"
    case .runsOut(let date): "risk \(UTCDateText.compactString(from: date))"
    case .learning: "learning"
    case .stale: "stale"
    case .unavailable: "forecast —"
    }
  }

  private var limitColor: Color {
    if limit.exhausted { return Palette.exhaustRed }
    guard let remaining = limit.remainingFraction else { return Palette.slate }
    if remaining <= 0.1 { return Palette.exhaustRed }
    if remaining <= 0.3 { return Palette.signalAmber }
    return Palette.capacityMint
  }

  private var detailColor: Color {
    switch limit.forecast {
    case .exhausted: Palette.exhaustRed
    case .runsOut: Palette.signalAmber
    default: .secondary
    }
  }
}

private struct ErrorBanner: View {
  let message: String

  var body: some View {
    Label(message, systemImage: "exclamationmark.triangle.fill")
      .font(.caption)
      .foregroundStyle(Palette.signalAmber)
      .frame(maxWidth: .infinity, alignment: .leading)
      .padding(.horizontal, 12)
      .padding(.vertical, 7)
      .background(Palette.signalAmber.opacity(0.08))
  }
}

@MainActor
private enum UTCDateText {
  private static let formatter: DateFormatter = {
    let formatter = DateFormatter()
    formatter.locale = Locale(identifier: "en_US_POSIX")
    formatter.timeZone = TimeZone(secondsFromGMT: 0)
    formatter.dateFormat = "M/d HH:mm"
    return formatter
  }()

  static func compactString(from date: Date) -> String {
    formatter.string(from: date)
  }
}
