import AppKit
import RaylineStatusCore
import SwiftUI

/// Status colors carry the read-at-a-glance signal, so each one is tuned per
/// appearance. The dark tints stay vivid; the light tints are darkened enough
/// to keep small monospaced type legible on a white popover.
private enum Palette {
  static let railBlue = adaptive(
    light: (0.10, 0.40, 0.92), dark: (0.36, 0.60, 1.00))
  static let capacityMint = adaptive(
    light: (0.02, 0.52, 0.34), dark: (0.30, 0.82, 0.58))
  static let signalAmber = adaptive(
    light: (0.68, 0.43, 0.02), dark: (0.98, 0.72, 0.28))
  static let exhaustRed = adaptive(
    light: (0.80, 0.16, 0.18), dark: (0.98, 0.44, 0.44))
  static let slate = adaptive(
    light: (0.42, 0.45, 0.52), dark: (0.58, 0.61, 0.69))

  private static func adaptive(
    light: (Double, Double, Double), dark: (Double, Double, Double)
  ) -> Color {
    Color(
      nsColor: NSColor(name: nil) { appearance in
        let components = appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua ? dark : light
        return NSColor(
          srgbRed: components.0, green: components.1, blue: components.2, alpha: 1)
      })
  }
}

private enum TableLayout {
  static let width: CGFloat = 384
  static let gutter: CGFloat = 12
  static let columnSpacing: CGFloat = 10
  static let rowHeight: CGFloat = 46
  static let fiveHour: CGFloat = 62
  static let sevenDay: CGFloat = 62
  static let fable: CGFloat = 66
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
    .onDisappear {
      hoverDetail = nil
    }
  }

  private var header: some View {
    HStack(spacing: 8) {
      if let hoverDetail {
        HoverInspectionHeader(detail: hoverDetail)
      } else {
        poolHeader
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
    .frame(height: 25)
    .padding(.horizontal, TableLayout.gutter)
    .padding(.vertical, 8)
  }

  private var poolHeader: some View {
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
    }
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
            onHoverDetail: { detail in
              if let detail {
                hoverDetail = detail
              } else if hoverDetail?.account.id == account.id {
                hoverDetail = nil
              }
            })
          if index < presentation.accounts.count - 1 {
            RowSeparator()
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
      Text("% left · resets in · ⚠ runs out")
        .font(.system(size: 9, design: .monospaced))
        .foregroundStyle(.tertiary)
      Button("Quit") {
        NSApplication.shared.terminate(nil)
      }
      .buttonStyle(.borderless)
      .font(.caption2)
    }
    .padding(.horizontal, TableLayout.gutter)
    .padding(.vertical, 6)
    .help(
      "Each column shows allowance left and the time until that window resets. A cell at risk swaps the reset countdown for the projected time to empty, marked with ⚠. Hover a cell for the forecast. Refreshes every minute."
    )
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

private struct RowSeparator: View {
  var body: some View {
    Rectangle()
      .fill(Color.primary.opacity(0.07))
      .frame(height: 0.5)
      .padding(.horizontal, TableLayout.gutter)
  }
}

private struct ColumnHeader: View {
  var body: some View {
    HStack(spacing: TableLayout.columnSpacing) {
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
    .tracking(0.6)
    .foregroundStyle(.tertiary)
    .padding(.horizontal, TableLayout.gutter)
    .padding(.vertical, 5)
  }
}

private struct SubscriptionRow: View {
  let account: AccountPresentation
  let onHoverDetail: (HoverDetail?) -> Void

  @State private var isHovered = false
  @State private var hoveredLimit: LimitKind?

  var body: some View {
    HStack(spacing: TableLayout.columnSpacing) {
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
    .padding(.horizontal, TableLayout.gutter)
    .frame(height: TableLayout.rowHeight)
    .background(
      RoundedRectangle(cornerRadius: 7, style: .continuous)
        .fill(Color.primary.opacity(isHovered ? 0.055 : 0))
        .padding(.horizontal, 5)
    )
    .animation(.easeOut(duration: 0.12), value: isHovered)
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
    let event = StatusText.nextEvent(for: account)
    let next = StatusText.nextEventDescription(event, dateText: fullDate(event.date))
    return
      "\(account.id) · \(account.availability.label) · \(active)\n\(next)\(warning)"
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
    HStack(spacing: 6) {
      Image(systemName: stateSymbol)
        .font(.system(size: 12, weight: .semibold))
        .foregroundStyle(color(for: account.availability))
        .help(account.availability.label)
        .accessibilityLabel(account.availability.label)
      Text(account.id)
        .font(.system(.subheadline, design: .rounded, weight: .bold))
        .lineLimit(1)
      Chip(text: account.plan.uppercased(), tint: nil)
      if account.activeLaunches > 0 {
        Chip(
          text: "\(account.activeLaunches)", symbol: "bolt.fill", tint: Palette.railBlue
        )
        .help(
          account.activeLaunches == 1
            ? "1 active session" : "\(account.activeLaunches) active sessions"
        )
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

private struct Chip: View {
  let text: String
  var symbol: String?
  var tint: Color?

  var body: some View {
    HStack(spacing: 2) {
      if let symbol {
        Image(systemName: symbol)
      }
      Text(text)
    }
    .font(.system(size: 8, weight: .semibold, design: .rounded))
    .monospacedDigit()
    .foregroundStyle(tint ?? Color.secondary)
    .padding(.horizontal, 4)
    .padding(.vertical, 1.5)
    .background(
      Capsule().fill((tint ?? Color.primary).opacity(tint == nil ? 0.07 : 0.14))
    )
  }
}

/// One allowance: how much is left, and how long until that window resets.
private struct QuotaCell: View {
  let limit: LimitPresentation?
  let onHoverChange: (Bool) -> Void

  var body: some View {
    VStack(alignment: .trailing, spacing: 2) {
      HStack(alignment: .firstTextBaseline, spacing: 1) {
        Text(value)
          .font(.system(size: 14, weight: .semibold, design: .rounded))
          .monospacedDigit()
          .foregroundStyle(valueColor)
        if let unit {
          Text(unit)
            .font(.system(size: 9, weight: .semibold, design: .rounded))
            .foregroundStyle(valueColor.opacity(0.55))
        }
      }
      HStack(spacing: 2) {
        if isRisk {
          Image(systemName: "exclamationmark.triangle.fill")
            .font(.system(size: 7))
        }
        Text(countdown)
          .font(.system(size: 9, weight: .medium, design: .monospaced))
      }
      .foregroundStyle(countdownColor)
      .lineLimit(1)
      .minimumScaleFactor(0.8)
    }
    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .trailing)
    .contentShape(Rectangle())
    .help(helpText)
    .accessibilityElement(children: .ignore)
    .accessibilityLabel(helpText)
    .onHover(perform: onHoverChange)
  }

  private var value: String { StatusText.cellValue(limit) }

  private var unit: String? { StatusText.cellUnit(limit) }

  private var countdown: String { StatusText.cellCountdown(limit) }

  private var isRisk: Bool {
    limit?.isAtRisk ?? false
  }

  private var valueColor: Color {
    guard let limit else { return Palette.slate }
    if limit.exhausted { return Palette.exhaustRed }
    guard let remaining = limit.remainingFraction else { return Palette.slate }
    if remaining <= 0.1 { return Palette.exhaustRed }
    if remaining <= 0.3 { return Palette.signalAmber }
    return Palette.capacityMint
  }

  private var countdownColor: Color {
    isRisk ? Palette.signalAmber : Color.secondary
  }

  private var helpText: String {
    StatusText.tooltip(
      limit,
      resetDateText: fullDate(limit?.reset),
      runOutDateText: fullDate(limit?.runOutDate)
    )
  }
}

private struct HoverInspectionHeader: View {
  let detail: HoverDetail

  var body: some View {
    HStack(spacing: 7) {
      Image(systemName: eventSymbol)
        .font(.system(size: 12, weight: .semibold))
        .foregroundStyle(eventColor)
        .frame(width: 18)

      VStack(alignment: .leading, spacing: 1) {
        HStack(spacing: 4) {
          Text(detail.account.id)
            .font(.system(.caption, design: .rounded, weight: .bold))
          Text("·")
            .foregroundStyle(.tertiary)
          Text(contextLabel)
            .font(.system(size: 9, weight: .semibold, design: .monospaced))
            .foregroundStyle(.secondary)
        }

        Text(timingText)
          .font(.system(size: 9, weight: .medium, design: .monospaced))
          .foregroundStyle(eventColor)
          .lineLimit(1)
          .minimumScaleFactor(0.72)
      }
      .frame(maxWidth: .infinity, alignment: .leading)
    }
    .accessibilityElement(children: .combine)
    .accessibilityLabel("\(detail.account.id), \(contextLabel), \(timingText)")
  }

  private var contextLabel: String {
    guard let limit = detail.limit else {
      let active =
        detail.account.activeLaunches == 1 ? "1 ACTIVE" : "\(detail.account.activeLaunches) ACTIVE"
      return "\(detail.account.availability.label.uppercased()) · \(active)"
    }
    return "\(limit.kind.title.uppercased()) · \(StatusText.remainingSummary(limit))"
  }

  private var timingText: String {
    guard let limit = detail.limit else {
      let event = StatusText.nextEvent(for: detail.account)
      return StatusText.accountTiming(event, dateText: compactDate(event.date))
    }
    return StatusText.limitTiming(limit, resetDateText: compactDate(limit.reset))
  }

  private var eventSymbol: String {
    guard let limit = detail.limit else {
      if detail.account.limits.contains(where: \.exhausted) { return "arrow.clockwise" }
      if detail.account.limits.contains(where: { limit in
        if case .runsOut = limit.forecast { return true }
        return false
      }) {
        return "exclamationmark.triangle.fill"
      }
      return "checkmark.circle.fill"
    }
    switch limit.forecast {
    case .exhausted: return "xmark.circle.fill"
    case .runsOut: return "exclamationmark.triangle.fill"
    case .noBurn, .resetFirst: return "checkmark.circle.fill"
    case .learning: return "ellipsis.circle.fill"
    case .stale, .unavailable: return "questionmark.circle.fill"
    }
  }

  private var eventColor: Color {
    guard let limit = detail.limit else {
      if detail.account.limits.contains(where: \.exhausted) { return Palette.exhaustRed }
      if detail.account.limits.contains(where: { limit in
        if case .runsOut = limit.forecast { return true }
        return false
      }) {
        return Palette.signalAmber
      }
      return color(for: detail.account.availability)
    }
    switch limit.forecast {
    case .exhausted: return Palette.exhaustRed
    case .runsOut: return Palette.signalAmber
    case .noBurn, .resetFirst: return Palette.capacityMint
    case .learning, .stale, .unavailable: return Palette.slate
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
private func fullDate(_ date: Date?) -> String {
  guard let date else { return "unknown" }
  return DateText.full.string(from: date)
}

@MainActor
private func compactDate(_ date: Date?) -> String {
  guard let date else { return "unknown" }
  return DateText.compact.string(from: date)
}

@MainActor
private enum DateText {
  static let compact: DateFormatter = {
    let formatter = DateFormatter()
    formatter.locale = Locale(identifier: "en_US_POSIX")
    formatter.timeZone = TimeZone(secondsFromGMT: 0)
    formatter.dateFormat = "MMM d HH:mm"
    return formatter
  }()

  static let full: DateFormatter = {
    let formatter = DateFormatter()
    formatter.locale = Locale(identifier: "en_US_POSIX")
    formatter.timeZone = TimeZone(secondsFromGMT: 0)
    formatter.dateFormat = "MMM d, HH:mm 'UTC'"
    return formatter
  }()
}
