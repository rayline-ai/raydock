import Foundation

public enum RaylineStatusClientError: LocalizedError, Sendable {
  case executableNotFound
  case commandFailed(String)
  case invalidResponse(String)

  public var errorDescription: String? {
    switch self {
    case .executableNotFound:
      "Rayline CLI not found. Install it at ~/.rayline/bin/rayline or set RAYLINE_BIN."
    case .commandFailed(let message):
      message
    case .invalidResponse(let message):
      "Could not read Rayline status: \(message)"
    }
  }
}

public struct RaylineStatusClient: Sendable {
  public init() {}

  static func arguments(poolID: String) -> [String] {
    [
      "subscriptions", "status",
      "--pool", poolID,
      "--json",
      "--live-only",
    ]
  }

  public func fetch(poolID: String = "default") async throws -> PoolRuntimeStatus {
    let executable = try RaylineExecutableLocator.resolve()
    return try await Task.detached(priority: .utility) {
      let process = Process()
      let standardOutput = Pipe()
      let standardError = Pipe()
      process.executableURL = executable
      process.arguments = Self.arguments(poolID: poolID)
      process.standardOutput = standardOutput
      process.standardError = standardError

      do {
        try process.run()
      } catch {
        throw RaylineStatusClientError.commandFailed(
          "Could not start Rayline: \(error.localizedDescription)")
      }

      let output = standardOutput.fileHandleForReading.readDataToEndOfFile()
      let errorOutput = standardError.fileHandleForReading.readDataToEndOfFile()
      process.waitUntilExit()

      guard process.terminationStatus == 0 else {
        let boundedError = Data(errorOutput.prefix(1_024))
        let message = String(data: boundedError, encoding: .utf8)?
          .trimmingCharacters(in: .whitespacesAndNewlines)
        let fallback = "Rayline status exited with code \(process.terminationStatus)."
        let detail = message.flatMap { $0.isEmpty ? nil : $0 } ?? fallback
        throw RaylineStatusClientError.commandFailed(detail)
      }
      guard output.count <= 2 * 1_024 * 1_024 else {
        throw RaylineStatusClientError.invalidResponse("response exceeded 2 MiB")
      }
      do {
        return try JSONDecoder().decode(PoolRuntimeStatus.self, from: output)
      } catch {
        throw RaylineStatusClientError.invalidResponse(error.localizedDescription)
      }
    }.value
  }
}

private enum RaylineExecutableLocator {
  static func resolve() throws -> URL {
    let environment = ProcessInfo.processInfo.environment
    let home = FileManager.default.homeDirectoryForCurrentUser.path
    var candidates: [String] = []
    if let override = environment["RAYLINE_BIN"], !override.isEmpty {
      candidates.append(override)
    }
    if let stored = UserDefaults.standard.string(forKey: "RaylineExecutablePath"), !stored.isEmpty {
      candidates.append(stored)
    }
    candidates.append(contentsOf: [
      "\(home)/.rayline/bin/rayline",
      "/opt/homebrew/bin/rayline",
      "/usr/local/bin/rayline",
      "\(FileManager.default.currentDirectoryPath)/target/release/rayline",
    ])
    if let path = environment["PATH"] {
      candidates.append(contentsOf: path.split(separator: ":").map { "\($0)/rayline" })
    }

    for candidate in candidates where FileManager.default.isExecutableFile(atPath: candidate) {
      return URL(fileURLWithPath: candidate)
    }
    throw RaylineStatusClientError.executableNotFound
  }
}
