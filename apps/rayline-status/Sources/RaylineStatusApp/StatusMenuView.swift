import AppKit
import RaylineStatusCore
import SwiftUI

/// Status colors carry the read-at-a-glance signal, so each one is tuned per
/// appearance. The dark tints stay vivid; the light tints are darkened enough
/// to keep small type legible on a white popover.
enum Palette {
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

private enum Layout {
  static let width: CGFloat = 420
  static let gutter: CGFloat = 12
}

// MARK: - Menu bar

struct MenuBarStatusLabel: View {
  @ObservedObject var store: SubscriptionStore

  var body: some View {
    if let presentation = store.presentation, !presentation.accounts.isEmpty {
      let pool = PoolGlance(presentation, now: store.now)
      let problems = pool.runningFastCount + pool.outCount
      HStack(spacing: 3) {
        Image(nsImage: MenuBarGlyph.image(for: pool.menuBarAccounts()))
        if problems > 0 {
          Text("\(problems)")
            .monospacedDigit()
        }
      }
      .help(menuBarHelp(pool))
    } else {
      Image(
        systemName: store.errorMessage == nil
          ? "chart.bar" : "exclamationmark.triangle.fill")
    }
  }

  private func menuBarHelp(_ pool: PoolGlance) -> String {
    var parts = pool.menuBarAccounts().map { glance in
      "\(glance.accountID) \(percentText(glance))"
    }
    if pool.accounts.count > parts.count {
      parts.append("+\(pool.accounts.count - parts.count) more")
    }
    return "Weekly limit left: " + parts.joined(separator: " · ")
  }
}

/// Draws one vertical bar per account, filled from the bottom with the weekly
/// allowance left. The image draws lazily so the dynamic system colors resolve
/// against the menu bar's own appearance. It stays a template image while
/// every account is on pace, which lets macOS tint it like any other item.
enum MenuBarGlyph {
  private static let barWidth: CGFloat = 3
  private static let gap: CGFloat = 2
  private static let height: CGFloat = 16
  private static let barHeight: CGFloat = 13

  static func image(for accounts: [WeeklyGlance]) -> NSImage {
    let count = CGFloat(max(accounts.count, 1))
    let size = NSSize(width: count * barWidth + (count - 1) * gap, height: height)
    let needsColor = accounts.contains { glance in
      switch glance.state {
      case .out, .runningFast: true
      default: false
      }
    }
    let image = NSImage(size: size, flipped: false) { _ in
      let base = needsColor ? NSColor.labelColor : NSColor.black
      let bottom = (height - barHeight) / 2
      for (index, glance) in accounts.enumerated() {
        let x = CGFloat(index) * (barWidth + gap)
        let track = NSRect(x: x, y: bottom, width: barWidth, height: barHeight)
        base.withAlphaComponent(0.25).setFill()
        NSBezierPath(roundedRect: track, xRadius: 1, yRadius: 1).fill()

        let fill: NSColor
        let fraction: CGFloat
        switch glance.state {
        case .out:
          fill = .systemRed
          fraction = 0.16
        case .runningFast:
          fill = .systemOrange
          fraction = CGFloat(glance.remaining ?? 0)
        case .unknown:
          continue
        case .onPace, .idle, .measuring:
          fill = base
          fraction = CGFloat(glance.remaining ?? 0)
        }
        let filled = max(1.5, barHeight * min(max(fraction, 0), 1))
        fill.setFill()
        NSBezierPath(
          roundedRect: NSRect(x: x, y: bottom, width: barWidth, height: filled),
          xRadius: 1, yRadius: 1
        ).fill()
      }
      return true
    }
    image.isTemplate = !needsColor
    image.accessibilityDescription = "Weekly limit left per account"
    return image
  }
}

// MARK: - Popover

struct StatusMenuView: View {
  @ObservedObject var store: SubscriptionStore

  var body: some View {
    VStack(spacing: 0) {
      if let presentation = store.presentation, !presentation.accounts.isEmpty {
        let pool = PoolGlance(presentation, now: store.now)
        header(pool)
        Divider()
        if let error = store.errorMessage {
          ErrorBanner(message: error)
          Divider()
        }
        WeeklyTimelineView(pool: pool, now: store.now)
      } else if store.isRefreshing {
        VStack(spacing: 8) {
          ProgressView()
          Text("Reading the subscription pool…")
            .font(.caption)
            .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, minHeight: 130)
      } else {
        unavailable
      }
      Divider()
      footer
    }
    .frame(width: Layout.width)
  }

  private func header(_ pool: PoolGlance) -> some View {
    HStack(alignment: .top, spacing: 8) {
      VStack(alignment: .leading, spacing: 2) {
        headline(pool)
          .font(.system(size: 14, weight: .semibold))
        Text(subheadline(pool))
          .font(.caption)
          .foregroundStyle(.secondary)
          .lineLimit(1)
      }
      Spacer(minLength: 0)
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
    .padding(.horizontal, Layout.gutter)
    .padding(.top, 12)
    .padding(.bottom, 10)
  }

  private func headline(_ pool: PoolGlance) -> Text {
    let timeline = WeeklyTimeline(pool: pool, now: store.now)
    if let gap = timeline.gap {
      return Text(
        "Possible weekly gap · \(ResetCountdown.text(until: gap.end, from: gap.start) ?? "")"
      )
      .foregroundColor(Palette.exhaustRed)
    }
    var parts: [Text] = []
    if pool.runningFastCount > 0 {
      parts.append(
        Text("\(pool.runningFastCount) running too fast").foregroundColor(Palette.signalAmber))
    }
    if pool.outCount > 0 {
      parts.append(Text("\(pool.outCount) out").foregroundColor(Palette.exhaustRed))
    }
    if parts.isEmpty {
      if timeline.unknownCount > 0 {
        return Text("Weekly forecast incomplete").foregroundColor(.secondary)
      }
      return Text("All on pace").foregroundColor(Palette.capacityMint)
    }
    return parts.dropFirst().reduce(parts[0]) { line, part in
      Text("\(line)\(Text(" · ").foregroundColor(.secondary))\(part)")
    }
  }

  private func subheadline(_ pool: PoolGlance) -> String {
    let timeline = WeeklyTimeline(pool: pool, now: store.now)
    if let gap = timeline.gap {
      let unknown = timeline.unknownCount > 0 ? " · \(timeline.unknownCount) unknown" : ""
      return "\(DateText.dayTime(gap.start))–\(DateText.dayTime(gap.end))\(unknown)"
    }
    if let first = pool.firstRunOut {
      return "First to run dry: \(first.account.accountID), \(DateText.dayTime(first.date))"
    }
    let count = pool.accounts.count
    return "\(count) account\(count == 1 ? "" : "s") · weekly limits"
  }

  private var unavailable: some View {
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
        .frame(maxWidth: 300)
      Button("Try Again") {
        Task { await store.refresh() }
      }
    }
    .frame(maxWidth: .infinity, minHeight: 130)
    .padding()
  }

  private var footer: some View {
    HStack(spacing: 8) {
      Text("Weekly · local time")
      Spacer()
      Link("Claude Usage ↗", destination: ResetInventoryPresentation.manualRedemptionURL)
        .help(
          "Saved resets are redeemed by hand. \(ResetInventoryPresentation.manualRedemptionWarning)"
        )
      Text(updatedText)
        .foregroundStyle(.secondary)
      Button("Quit") {
        NSApplication.shared.terminate(nil)
      }
      .buttonStyle(.borderless)
    }
    .font(.caption2)
    .foregroundStyle(.secondary)
    .padding(.horizontal, Layout.gutter)
    .padding(.vertical, 7)
  }

  private var updatedText: String {
    guard let date = store.lastUpdated else { return "waiting" }
    return date.formatted(.relative(presentation: .named))
  }
}

struct FlagChip: View {
  let flag: WeeklyGlance.Flag

  var body: some View {
    Text(flag.text)
      .font(.system(size: 9.5, weight: .semibold))
      .monospacedDigit()
      .foregroundStyle(tint)
      .padding(.horizontal, 4)
      .padding(.vertical, 1)
      .background(RoundedRectangle(cornerRadius: 3).fill(tint.opacity(0.14)))
      .fixedSize()
      .help(flag.help)
  }

  private var tint: Color {
    switch flag.tone {
    case .neutral: .secondary
    case .warning: Palette.signalAmber
    case .critical: Palette.exhaustRed
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

func percentText(_ glance: WeeklyGlance) -> String {
  if case .out = glance.state { return "0%" }
  guard let remaining = glance.remaining else { return "—" }
  return "\(Int((remaining * 100).rounded()))%"
}

/// Local time. Within a day a clock time reads best; further out, the weekday.
@MainActor
enum DateText {
  static func short(_ date: Date, from now: Date) -> String {
    date.timeIntervalSince(now) < 20 * 60 * 60
      ? time.string(from: date) : weekday.string(from: date)
  }

  static func dayTime(_ date: Date) -> String {
    dayTimeFormatter.string(from: date)
  }

  private static let time = formatter("HH:mm")
  private static let weekday = formatter("EEE")
  private static let dayTimeFormatter = formatter("EEE HH:mm")

  private static func formatter(_ template: String) -> DateFormatter {
    let formatter = DateFormatter()
    formatter.setLocalizedDateFormatFromTemplate(template)
    return formatter
  }
}
