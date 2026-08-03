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

private enum TableLayout {
  static let width: CGFloat = 370
  static let fiveHour: CGFloat = 48
  static let sevenDay: CGFloat = 48
  static let fable: CGFloat = 58
}

private struct HoverDetail {
  let account: AccountPresentation
  let limit: LimitPresentation?
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
  @State private var hoverDetail: HoverDetail?

  var body: some View {
    VStack(spacing: 0) {
      header
      Divider()
      content
      Divider()
      footer
    }
    .frame(width: TableLayout.width)
    .overlay(alignment: .bottom) {
      if let hoverDetail {
        HoverDetailCard(detail: hoverDetail)
          .padding(.horizontal, 8)
          .padding(.bottom, 27)
          .allowsHitTesting(false)
          .transition(.opacity)
      }
    }
    .onDisappear {
      hoverDetail = nil
    }
  }

  private var header: some View {
    HStack(spacing: 8) {
      ZStack {
        RoundedRectangle(cornerRadius: 6)
          .fill(Palette.railBlue.opacity(0.14))
        Image(systemName: "arrow.triangle.branch")
          .font(.system(size: 13, weight: .semibold))
          .foregroundStyle(Palette.railBlue)
      }
      .frame(width: 25, height: 25)
      Text("Claude pool")
        .font(.system(.subheadline, design: .rounded, weight: .semibold))
      Text("\(store.presentation?.poolID ?? "default") · UTC")
        .font(.caption2.monospaced())
        .foregroundStyle(.secondary)
      Spacer()
      if let presentation = store.presentation {
        HStack(spacing: 4) {
          Circle()
            .fill(readinessColor(presentation))
            .frame(width: 5, height: 5)
          Text("\(presentation.availableAccountCount)/\(presentation.totalAccountCount) ready")
            .font(.caption2.monospacedDigit().weight(.medium))
        }
        .foregroundStyle(readinessColor(presentation))
      }
      Button {
        Task { await store.refresh() }
      } label: {
        Image(systemName: "arrow.clockwise")
          .font(.caption)
      }
      .buttonStyle(.borderless)
      .disabled(store.isRefreshing)
      .help("Refresh now")
    }
    .padding(.horizontal, 10)
    .padding(.vertical, 7)
  }

  @ViewBuilder
  private var content: some View {
    if let presentation = store.presentation {
      VStack(spacing: 0) {
        if let error = store.errorMessage {
          ErrorBanner(message: error)
          Divider()
        }
        ColumnHeader()
        Divider()
        ForEach(Array(presentation.accounts.enumerated()), id: \.element.id) { index, account in
          SubscriptionRow(
            account: account,
            alternate: index.isMultiple(of: 2) == false,
            onHoverDetail: { detail in
              if let detail {
                hoverDetail = detail
              } else if hoverDetail?.account.id == account.id {
                hoverDetail = nil
              }
            })
          if index < presentation.accounts.count - 1 {
            Divider()
              .opacity(0.55)
          }
        }
      }
    } else if store.isRefreshing {
      VStack(spacing: 8) {
        ProgressView()
        Text("Reading live pool status…")
          .font(.caption)
          .foregroundStyle(.secondary)
      }
      .frame(maxWidth: .infinity, minHeight: 130)
    } else {
      VStack(spacing: 8) {
        Image(systemName: "exclamationmark.triangle")
          .font(.title2)
          .foregroundStyle(Palette.signalAmber)
        Text("Status unavailable")
          .font(.subheadline.weight(.semibold))
        Text(store.errorMessage ?? "Start Claude through Rayline, then refresh.")
          .font(.caption)
          .foregroundStyle(.secondary)
          .multilineTextAlignment(.center)
          .frame(maxWidth: 360)
        Button("Try Again") {
          Task { await store.refresh() }
        }
      }
      .frame(maxWidth: .infinity, minHeight: 130)
      .padding()
    }
  }

  private var footer: some View {
    HStack(spacing: 8) {
      Circle()
        .fill(store.errorMessage == nil ? Palette.capacityMint : Palette.signalAmber)
        .frame(width: 4, height: 4)
      Text(updatedText)
        .font(.system(size: 9, design: .monospaced))
        .foregroundStyle(.secondary)
      Spacer()
      Text("hover limits")
        .font(.system(size: 9, design: .monospaced))
        .foregroundStyle(.tertiary)
      Text("refresh 1m")
        .font(.system(size: 9, design: .monospaced))
        .foregroundStyle(.tertiary)
      Button("Quit") {
        NSApplication.shared.terminate(nil)
      }
      .buttonStyle(.borderless)
      .font(.caption2)
    }
    .padding(.horizontal, 10)
    .padding(.vertical, 5)
  }

  private func readinessColor(_ presentation: PoolPresentation) -> Color {
    presentation.availableAccountCount == presentation.totalAccountCount
      ? Palette.capacityMint : Palette.signalAmber
  }

  private var updatedText: String {
    guard let date = store.lastUpdated else { return "waiting for refresh" }
    return "updated \(date.formatted(.relative(presentation: .named)))"
  }
}

private struct ColumnHeader: View {
  var body: some View {
    HStack(spacing: 8) {
      Text("SUBSCRIPTION")
        .frame(maxWidth: .infinity, alignment: .leading)
      Text("5H")
        .frame(width: TableLayout.fiveHour, alignment: .trailing)
      Text("7D")
        .frame(width: TableLayout.sevenDay, alignment: .trailing)
      Text("FABLE")
        .frame(width: TableLayout.fable, alignment: .trailing)
    }
    .font(.system(size: 8, weight: .semibold, design: .monospaced))
    .foregroundStyle(.tertiary)
    .padding(.horizontal, 10)
    .padding(.vertical, 4)
  }
}

private struct SubscriptionRow: View {
  let account: AccountPresentation
  let alternate: Bool
  let onHoverDetail: (HoverDetail?) -> Void

  @State private var isHovered = false
  @State private var hoveredLimit: LimitKind?

  var body: some View {
    HStack(spacing: 8) {
      AccountCell(account: account)
        .frame(maxWidth: .infinity, alignment: .leading)
      QuotaCell(limit: limit(.fiveHour)) { hovering in
        updateLimitHover(.fiveHour, hovering: hovering)
      }
      .frame(width: TableLayout.fiveHour, alignment: .trailing)
      QuotaCell(limit: limit(.sevenDay)) { hovering in
        updateLimitHover(.sevenDay, hovering: hovering)
      }
      .frame(width: TableLayout.sevenDay, alignment: .trailing)
      QuotaCell(limit: limit(.fable)) { hovering in
        updateLimitHover(.fable, hovering: hovering)
      }
      .frame(width: TableLayout.fable, alignment: .trailing)
    }
    .padding(.horizontal, 10)
    .frame(height: 34)
    .background(alternate ? Color.primary.opacity(0.025) : .clear)
    .help(accountHelp)
    .onHover { hovering in
      isHovered = hovering
      if hovering {
        publishHoverDetail()
      } else {
        hoveredLimit = nil
        onHoverDetail(nil)
      }
    }
  }

  private func limit(_ kind: LimitKind) -> LimitPresentation? {
    account.limits.first { $0.kind == kind }
  }

  private var accountHelp: String {
    let active =
      account.activeLaunches == 1
      ? "1 active session" : "\(account.activeLaunches) active sessions"
    let warning = account.warning.map { "\n\($0)" } ?? ""
    return
      "\(account.id) · \(account.availability.label) · \(active)\n\(nextEventDescription(for: account))\(warning)"
  }

  private func updateLimitHover(_ kind: LimitKind, hovering: Bool) {
    hoveredLimit = hovering ? kind : nil
    if hovering {
      onHoverDetail(HoverDetail(account: account, limit: limit(kind)))
    } else if isHovered {
      onHoverDetail(HoverDetail(account: account, limit: nil))
    } else {
      onHoverDetail(nil)
    }
  }

  private func publishHoverDetail() {
    let focusedLimit = hoveredLimit.flatMap(limit)
    onHoverDetail(HoverDetail(account: account, limit: focusedLimit))
  }
}

private struct AccountCell: View {
  let account: AccountPresentation

  var body: some View {
    HStack(spacing: 5) {
      Image(systemName: stateSymbol)
        .font(.system(size: 11, weight: .semibold))
        .foregroundStyle(color(for: account.availability))
        .help(account.availability.label)
        .accessibilityLabel(account.availability.label)
      Text(account.id)
        .font(.system(.subheadline, design: .rounded, weight: .bold))
        .lineLimit(1)
      Text(account.plan.uppercased())
        .font(.system(size: 8, weight: .medium, design: .monospaced))
        .foregroundStyle(.secondary)
      if account.activeLaunches > 0 {
        HStack(spacing: 2) {
          Image(systemName: "bolt.fill")
          Text("\(account.activeLaunches)")
        }
        .font(.system(size: 8, weight: .medium, design: .monospaced))
        .foregroundStyle(Palette.railBlue)
      }
    }
  }

  private var stateSymbol: String {
    switch account.availability {
    case .all: "checkmark.circle.fill"
    case .nonFable: "exclamationmark.circle.fill"
    case .none: "xmark.circle.fill"
    case .unknown: "questionmark.circle.fill"
    }
  }
}

private struct QuotaCell: View {
  let limit: LimitPresentation?
  let onHoverChange: (Bool) -> Void

  var body: some View {
    HStack(spacing: 3) {
      if isRisk {
        Image(systemName: "exclamationmark.triangle.fill")
          .font(.system(size: 7))
          .foregroundStyle(Palette.signalAmber)
      }
      Text(value)
        .font(.system(size: 11, weight: .semibold, design: .monospaced))
        .foregroundStyle(valueColor)
    }
    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .trailing)
    .contentShape(Rectangle())
    .help(helpText)
    .accessibilityLabel(helpText)
    .onHover(perform: onHoverChange)
  }

  private var value: String {
    guard let limit else { return "—" }
    if limit.exhausted { return "OUT" }
    guard let remaining = limit.remainingFraction else { return "—" }
    return String(format: "%.0f%%", remaining * 100)
  }

  private var isRisk: Bool {
    guard let limit else { return false }
    if case .runsOut = limit.forecast { return true }
    return false
  }

  private var valueColor: Color {
    guard let limit else { return Palette.slate }
    if limit.exhausted { return Palette.exhaustRed }
    guard let remaining = limit.remainingFraction else { return Palette.slate }
    if remaining <= 0.1 { return Palette.exhaustRed }
    if remaining <= 0.3 { return Palette.signalAmber }
    return Palette.capacityMint
  }

  private var helpText: String {
    guard let limit else { return "Allowance unavailable" }
    let allowance = limit.exhausted ? "exhausted" : "\(value) left"
    return
      "\(limit.kind.title): \(allowance)\n\(forecastDescription(limit.forecast))\nReset: \(fullDate(limit.reset))"
  }
}

private struct HoverDetailCard: View {
  let detail: HoverDetail

  var body: some View {
    VStack(alignment: .leading, spacing: 5) {
      HStack(spacing: 5) {
        Image(systemName: accountStateSymbol)
          .font(.system(size: 10, weight: .semibold))
          .foregroundStyle(color(for: detail.account.availability))
        Text(detail.account.id)
          .font(.system(.caption, design: .rounded, weight: .bold))
        Text(detail.account.availability.label)
          .font(.caption2)
          .foregroundStyle(.secondary)
        Spacer()
        if let limit = detail.limit {
          Text("\(limit.kind.title.uppercased()) · \(remainingText(limit))")
            .font(.system(size: 9, weight: .semibold, design: .monospaced))
            .foregroundStyle(eventColor(limit.forecast))
        }
      }

      if let limit = detail.limit {
        limitTiming(limit)
      } else {
        accountTiming
      }
    }
    .padding(.horizontal, 9)
    .padding(.vertical, 8)
    .frame(maxWidth: .infinity, alignment: .leading)
    .background(.ultraThickMaterial, in: RoundedRectangle(cornerRadius: 8))
    .overlay {
      RoundedRectangle(cornerRadius: 8)
        .stroke(cardAccent.opacity(0.5), lineWidth: 1)
    }
    .shadow(color: .black.opacity(0.28), radius: 10, y: 4)
  }

  private func limitTiming(_ limit: LimitPresentation) -> some View {
    VStack(alignment: .leading, spacing: 3) {
      Label(limitEvent(limit), systemImage: eventSymbol(limit.forecast))
        .font(.system(size: 10, weight: .semibold, design: .rounded))
        .foregroundStyle(eventColor(limit.forecast))
      Label("Resets \(fullDate(limit.reset))", systemImage: "arrow.clockwise")
        .font(.system(size: 9, design: .monospaced))
        .foregroundStyle(.secondary)
    }
  }

  private var accountTiming: some View {
    VStack(alignment: .leading, spacing: 3) {
      Label(nextEventDescription(for: detail.account), systemImage: accountEventSymbol)
        .font(.system(size: 10, weight: .semibold, design: .rounded))
        .foregroundStyle(cardAccent)
        .lineLimit(2)
      Text("Hover 5H, 7D, or Fable for full timing")
        .font(.system(size: 9, design: .monospaced))
        .foregroundStyle(.secondary)
    }
  }

  private var cardAccent: Color {
    guard let limit = detail.limit else { return color(for: detail.account.availability) }
    return eventColor(limit.forecast)
  }

  private var accountEventSymbol: String {
    if detail.account.limits.contains(where: \.exhausted) { return "arrow.clockwise" }
    if detail.account.limits.contains(where: { limit in
      if case .runsOut = limit.forecast { return true }
      return false
    }) {
      return "exclamationmark.triangle.fill"
    }
    return "checkmark"
  }

  private var accountStateSymbol: String {
    switch detail.account.availability {
    case .all: "checkmark.circle.fill"
    case .nonFable: "exclamationmark.circle.fill"
    case .none: "xmark.circle.fill"
    case .unknown: "questionmark.circle.fill"
    }
  }

  private func remainingText(_ limit: LimitPresentation) -> String {
    if limit.exhausted { return "OUT" }
    guard let remaining = limit.remainingFraction else { return "—" }
    return String(format: "%.0f%% LEFT", remaining * 100)
  }

  private func limitEvent(_ limit: LimitPresentation) -> String {
    switch limit.forecast {
    case .exhausted: "Limit reached; available again after reset"
    case .noBurn: "No current consumption"
    case .resetFirst: "On pace to last until reset"
    case .runsOut(let date): "Projected to hit limit \(fullDate(date))"
    case .learning: "Learning the current consumption rate"
    case .stale: "Usage data is stale"
    case .unavailable: "Run-out prediction unavailable"
    }
  }

  private func eventSymbol(_ forecast: DepletionForecast) -> String {
    switch forecast {
    case .exhausted: "xmark.circle.fill"
    case .runsOut: "exclamationmark.triangle.fill"
    case .noBurn, .resetFirst: "checkmark.circle.fill"
    case .learning: "ellipsis.circle.fill"
    case .stale, .unavailable: "questionmark.circle.fill"
    }
  }

  private func eventColor(_ forecast: DepletionForecast) -> Color {
    switch forecast {
    case .exhausted: Palette.exhaustRed
    case .runsOut: Palette.signalAmber
    case .noBurn, .resetFirst: Palette.capacityMint
    case .learning, .stale, .unavailable: Palette.slate
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
      .padding(.horizontal, 10)
      .padding(.vertical, 6)
      .background(Palette.signalAmber.opacity(0.08))
  }
}

private func color(for availability: AccountAvailability) -> Color {
  switch availability {
  case .all: Palette.capacityMint
  case .nonFable: Palette.signalAmber
  case .none: Palette.exhaustRed
  case .unknown: Palette.slate
  }
}

@MainActor
private func forecastDescription(_ forecast: DepletionForecast) -> String {
  switch forecast {
  case .exhausted: "Exhausted until reset"
  case .noBurn: "No current consumption"
  case .resetFirst: "Expected to reset before depletion"
  case .runsOut(let date): "Risk: projected to run out \(fullDate(date))"
  case .learning: "Forecast is learning the current rate"
  case .stale: "Forecast unavailable because usage is stale"
  case .unavailable: "Forecast unavailable"
  }
}

@MainActor
private func nextEventDescription(for account: AccountPresentation) -> String {
  let exhausted = account.limits
    .filter(\.exhausted)
    .compactMap { limit in limit.reset.map { (limit.kind, $0) } }
    .min { $0.1 < $1.1 }
  if let exhausted {
    return "Next: \(exhausted.0.title) allowance resets \(fullDate(exhausted.1))"
  }

  let risk = account.limits
    .compactMap { limit -> (LimitKind, Date)? in
      guard case .runsOut(let date) = limit.forecast else { return nil }
      return (limit.kind, date)
    }
    .min { $0.1 < $1.1 }
  if let risk {
    return "Next risk: \(risk.0.title) allowance may run out \(fullDate(risk.1))"
  }

  let nextReset = account.limits
    .compactMap { limit in limit.reset.map { (limit.kind, $0) } }
    .min { $0.1 < $1.1 }
  if let nextReset {
    return
      "Next: \(nextReset.0.title) allowance resets \(fullDate(nextReset.1)); no earlier depletion projected"
  }

  return "Next event unavailable"
}

@MainActor
private func fullDate(_ date: Date?) -> String {
  guard let date else { return "unknown" }
  return DateText.full.string(from: date)
}

@MainActor
private enum DateText {
  static let full: DateFormatter = {
    let formatter = DateFormatter()
    formatter.locale = Locale(identifier: "en_US_POSIX")
    formatter.timeZone = TimeZone(secondsFromGMT: 0)
    formatter.dateFormat = "MMM d, HH:mm 'UTC'"
    return formatter
  }()
}
