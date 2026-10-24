import RaylineStatusCore
import SwiftUI

struct WeeklyTimelineView: View {
  let pool: PoolGlance
  let now: Date
  private let labelWidth: CGFloat = 76
  private let rowHeight: CGFloat = 34

  var body: some View {
    let timeline = WeeklyTimeline(pool: pool, now: now)
    VStack(alignment: .leading, spacing: 0) {
      HStack(spacing: 8) {
        Text("Weekly left").font(.caption2).foregroundStyle(.secondary)
          .frame(width: labelWidth, alignment: .leading)
        axis(timeline)
      }
      .frame(height: 24)
      ScrollView {
        LazyVStack(spacing: 0) {
          ForEach(timeline.accounts) { account in
            row(account, timeline: timeline)
          }
        }
      }
      .frame(height: CGFloat(min(timeline.accounts.count, 9)) * rowHeight)
      HStack(spacing: 9) {
        legend("Quota", color: Palette.signalAmber)
        legend("Depleted", color: Palette.exhaustRed.opacity(0.45))
        legend("Reset", color: Palette.capacityMint)
        Text("┄ Renewed").foregroundStyle(.secondary)
      }
      .font(.system(size: 9))
      .padding(.vertical, 7)
      Text(
        timeline.unknownCount > 0
          ? "\(timeline.unknownCount) unknown · gap covers known forecasts only"
          : "Weekly only · current burn · renewal burn unknown"
      )
      .font(.system(size: 9))
      .foregroundStyle(.secondary)
      .lineLimit(1)
      .help(explanation(timeline))
    }
    .padding(.horizontal, 12)
    .padding(.bottom, 8)
  }

  private func axis(_ timeline: WeeklyTimeline) -> some View {
    GeometryReader { geometry in
      ZStack(alignment: .topLeading) {
        Text("Now").foregroundStyle(.primary)
        ForEach(timeline.dayTicks().filter { timeline.fraction($0) > 0.1 }, id: \.self) { date in
          Text(date.formatted(.dateTime.weekday(.abbreviated)))
            .position(x: geometry.size.width * timeline.fraction(date), y: 7)
        }
      }
      .font(.system(size: 10))
      .foregroundStyle(.secondary)
    }
    .frame(height: 14)
  }

  private func row(_ account: WeeklyGlance, timeline: WeeklyTimeline) -> some View {
    HStack(alignment: .top, spacing: 8) {
      VStack(alignment: .leading, spacing: 3) {
        HStack {
          Text(account.accountID).font(.system(size: 12, weight: .semibold)).lineLimit(1)
          Spacer(minLength: 3)
          Text(percentText(account)).font(.system(size: 11, design: .monospaced))
            .foregroundStyle(.secondary)
        }
        HStack(spacing: 3) {
          ForEach(Array(account.flags.prefix(1)), id: \.text) { FlagChip(flag: $0) }
          if account.flags.count > 1 {
            Text("+\(account.flags.count - 1)")
              .font(.system(size: 9))
              .foregroundStyle(.secondary)
              .help(account.flags.map(\.help).joined(separator: "\n"))
          }
        }
      }
      .padding(.top, 3)
      .frame(width: labelWidth, alignment: .leading)
      VStack(alignment: .leading, spacing: 1) {
        TimelineLane(account: account, timeline: timeline)
          .frame(height: 19)
        Text(verdict(account))
          .font(.system(size: 10))
          .foregroundStyle(.secondary)
          .lineLimit(1)
      }
      .frame(maxWidth: .infinity, minHeight: rowHeight, alignment: .topLeading)
      .background {
        GeometryReader { geometry in
          if let gap = timeline.gap {
            Rectangle()
              .fill(Palette.exhaustRed.opacity(0.13))
              .frame(
                width: geometry.size.width
                  * (timeline.fraction(gap.end) - timeline.fraction(gap.start))
              )
              .offset(x: geometry.size.width * timeline.fraction(gap.start))
          }
        }
      }
    }
    .frame(height: rowHeight, alignment: .top)
    .contentShape(Rectangle())
    .help(detail(account))
    .accessibilityElement(children: .ignore)
    .accessibilityLabel(detail(account))
  }

  private func legend(_ title: String, color: Color) -> some View {
    HStack(spacing: 3) {
      RoundedRectangle(cornerRadius: 2).fill(color).frame(width: 8, height: 4)
      Text(title).foregroundStyle(.secondary)
    }
  }

  private func explanation(_ timeline: WeeklyTimeline) -> String {
    var text =
      timeline.gap == nil
      ? "No overlap of known depleted weekly intervals."
      : "Shaded column: all accounts with known forecasts have depleted weekly quota."
    if timeline.unknownCount > 0 {
      text +=
        " \(timeline.unknownCount) account(s) have unknown timing; full-pool coverage is uncertain."
    }
    return text
      + " Current burn assumed; renewed quota and 5-hour/Fable flags do not guarantee availability."
  }

  private func verdict(_ account: WeeklyGlance) -> String {
    if !account.hasTimedForecast {
      switch account.state {
      case .out: return "Depleted at last reading · timing unknown"
      case .idle: return "Unused · reset unknown"
      case .measuring: return "Measuring burn · depletion unknown"
      default: return "Availability unknown · no current forecast"
      }
    }
    switch account.state {
    case .out(let back): return back.map { "Back \(DateText.dayTime($0))" } ?? "Out · reset unknown"
    case .runningFast(let dry): return "Dry \(DateText.dayTime(dry))"
    case .onPace, .idle:
      return account.reset.map { "Reset \(DateText.dayTime($0)) · on pace" }
        ?? "Unused · reset unknown"
    case .measuring: return "Measuring burn · depletion unknown"
    case .unknown:
      return account.remaining == nil
        ? "No data · availability unknown" : "Stale · availability unknown"
    }
  }

  private func detail(_ account: WeeklyGlance) -> String {
    var lines = ["\(account.accountID): \(percentText(account)) weekly left", verdict(account)]
    if let reset = account.reset { lines.append("Scheduled reset: \(DateText.dayTime(reset))") }
    lines.append("Depletion assumes current burn; after renewal, future burn is unknown.")
    lines += account.flags.map(\.help)
    return lines.joined(separator: "\n")
  }
}

private struct TimelineLane: View {
  let account: WeeklyGlance
  let timeline: WeeklyTimeline

  var body: some View {
    Canvas { context, size in
      func x(_ date: Date) -> CGFloat { size.width * timeline.fraction(date) }
      func fill(_ start: Date, _ end: Date, _ color: Color) {
        let rect = CGRect(x: x(start), y: 5, width: max(0, x(end) - x(start)), height: 9)
        context.fill(Path(roundedRect: rect, cornerRadius: 3), with: .color(color))
      }
      for tick in [timeline.now] + timeline.dayTicks() {
        var path = Path()
        path.move(to: CGPoint(x: x(tick), y: 0))
        path.addLine(to: CGPoint(x: x(tick), y: size.height))
        context.stroke(
          path, with: .color(.primary.opacity(0.13)),
          style: StrokeStyle(lineWidth: 1, dash: [2, 3]))
      }
      fill(timeline.now, timeline.end, .primary.opacity(0.06))
      if !account.hasTimedForecast {
        var hatches = Path()
        for offset in stride(from: CGFloat(0), to: size.width, by: 7) {
          hatches.move(to: CGPoint(x: offset, y: 14))
          hatches.addLine(to: CGPoint(x: min(size.width, offset + 7), y: 5))
        }
        context.stroke(hatches, with: .color(Palette.slate.opacity(0.4)), lineWidth: 1)
      } else {
        if let interval = timeline.depletedInterval(for: account) {
          fill(timeline.now, interval.start, Palette.signalAmber)
          fill(interval.start, interval.end, Palette.exhaustRed.opacity(0.35))
        } else if let reset = account.reset, reset > timeline.now {
          fill(timeline.now, reset, Palette.capacityMint.opacity(0.65))
        }
      }
      // Only fresh, timed forecasts can claim a future renewal.
      if account.hasTimedForecast,
        let reset = account.reset, reset > timeline.now, reset <= timeline.end
      {
        let rect = CGRect(x: x(reset), y: 5, width: max(0, size.width - x(reset)), height: 9)
        context.fill(
          Path(roundedRect: rect, cornerRadius: 3), with: .color(Palette.capacityMint.opacity(0.12))
        )
        context.stroke(
          Path(roundedRect: rect, cornerRadius: 3), with: .color(Palette.capacityMint.opacity(0.6)),
          style: StrokeStyle(lineWidth: 1, dash: [3, 3]))
        var tick = Path()
        tick.move(to: CGPoint(x: x(reset), y: 2))
        tick.addLine(to: CGPoint(x: x(reset), y: 17))
        context.stroke(tick, with: .color(Palette.capacityMint), lineWidth: 2)
      }
    }
    .accessibilityHidden(true)
  }
}
