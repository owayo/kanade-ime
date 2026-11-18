import AppKit
import Foundation

enum InstallerFailure: Error { case missingPayload }

/// The payload travels inside the signed installer, including when macOS
/// translocates a downloaded app. Never resolve an executable through PATH.
struct InstallerPayload: Sendable {
  let application: URL
  let executable: URL

  init(installer: URL) throws {
    application = installer.appendingPathComponent("Contents/Helpers/Kanade.app")
    executable = application.appendingPathComponent("Contents/MacOS/kanade-installer")
    let data = try Data(contentsOf: application.appendingPathComponent("Contents/Info.plist"))
    let plist =
      try PropertyListSerialization.propertyList(from: data, format: nil) as? [String: Any]
    guard let identifier = plist?["CFBundleIdentifier"] as? String,
      ["dev.kanade.inputmethod", "dev.kanade.inputmethod.Kanade"].contains(identifier),
      FileManager.default.isExecutableFile(atPath: executable.path)
    else {
      throw InstallerFailure.missingPayload
    }
  }

  func arguments(uninstall: Bool, dryRun: Bool) -> [String] {
    var arguments = ["install-macos", "--bundle", application.path]
    if uninstall { arguments.append("--uninstall") }
    if dryRun { arguments.append("--dry-run") }
    return arguments
  }

  func run(uninstall: Bool, dryRun: Bool) -> Int32 {
    let signature = execute(
      URL(fileURLWithPath: "/usr/bin/codesign"),
      [
        "--verify", "--deep", "--strict", application.path,
      ])
    guard signature == 0 else { return signature }
    return execute(executable, arguments(uninstall: uninstall, dryRun: dryRun))
  }

  private func execute(_ program: URL, _ arguments: [String]) -> Int32 {
    let process = Process()
    process.executableURL = program
    process.arguments = arguments
    process.standardOutput = FileHandle.nullDevice
    process.standardError = FileHandle.nullDevice
    do {
      try process.run()
      process.waitUntilExit()
      return process.terminationStatus
    } catch {
      return 125
    }
  }
}

@MainActor
final class InstallerDelegate: NSObject, NSApplicationDelegate {
  private let payload: InstallerPayload
  private var installing = false
  private var progress: NSWindow?

  init(payload: InstallerPayload) { self.payload = payload }

  func applicationDidFinishLaunching(_ notification: Notification) {
    NSApp.activate(ignoringOtherApps: true)
    let prompt = NSAlert()
    prompt.messageText = "Kanadeをインストール"
    prompt.informativeText =
      "このMacの現在のユーザーに、日本語入力IMEのKanadeをインストールします。\n\n入力中の文章を確定してから進めてください。更新やアンインストールでも、個人設定と学習データは保持します。"
    prompt.addButton(withTitle: "インストール")
    prompt.addButton(withTitle: "キャンセル")
    prompt.addButton(withTitle: "アンインストール")
    let answer = prompt.runModal()
    guard answer == .alertFirstButtonReturn || answer == .alertThirdButtonReturn else {
      NSApp.terminate(nil)
      return
    }
    let uninstall = answer == .alertThirdButtonReturn
    installing = true
    let window = NSWindow(
      contentRect: NSRect(x: 0, y: 0, width: 380, height: 110),
      styleMask: [.titled], backing: .buffered, defer: false
    )
    window.title = uninstall ? "Kanadeをアンインストール中" : "Kanadeをインストール中"
    window.isReleasedWhenClosed = false
    let spinner = NSProgressIndicator(frame: NSRect(x: 28, y: 39, width: 32, height: 32))
    spinner.style = .spinning
    spinner.startAnimation(nil)
    let label = NSTextField(labelWithString: "処理が終わるまでお待ちください。")
    label.frame = NSRect(x: 78, y: 39, width: 280, height: 28)
    window.contentView?.addSubview(spinner)
    window.contentView?.addSubview(label)
    window.center()
    window.makeKeyAndOrderFront(nil)
    progress = window
    let payload = payload
    Task {
      let status = await Task.detached(priority: .userInitiated) {
        payload.run(uninstall: uninstall, dryRun: false)
      }.value
      finish(status: status, uninstall: uninstall)
    }
  }

  private func finish(status: Int32, uninstall: Bool) {
    installing = false
    progress?.close()
    let result = NSAlert()
    if status == 0 {
      result.messageText = uninstall ? "アンインストールしました" : "インストールしました"
      result.informativeText =
        uninstall
        ? "個人設定と学習データは保持しました。"
        : "システム設定の「キーボード」で、入力ソースにKanadeを追加してください。表示されない場合はログインし直してください。かなキー、または入力メニューからKanadeを選べます。"
    } else {
      result.alertStyle = .warning
      result.messageText = "処理を完了できませんでした"
      result.informativeText = "アプリの署名とインストール先を確認してから、やり直してください。終了コード: \(status)"
    }
    result.addButton(withTitle: "閉じる")
    NSApp.activate(ignoringOtherApps: true)
    result.runModal()
    NSApp.terminate(nil)
  }

  func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
    installing ? .terminateCancel : .terminateNow
  }
}

@main
struct InstallerMain {
  @MainActor static func main() {
    do {
      let payload = try InstallerPayload(installer: Bundle.main.bundleURL)
      let arguments = Array(CommandLine.arguments.dropFirst())
      if arguments == ["--check"] {
        print("インストーラーのペイロード構成を確認しました")
        return
      }
      if arguments == ["--dry-run"] || arguments == ["--uninstall-dry-run"] {
        exit(payload.run(uninstall: arguments == ["--uninstall-dry-run"], dryRun: true))
      }
      guard arguments.isEmpty else { exit(2) }
      let application = NSApplication.shared
      let delegate = InstallerDelegate(payload: payload)
      application.setActivationPolicy(.regular)
      application.delegate = delegate
      withExtendedLifetime(delegate) { application.run() }
    } catch {
      let message = "インストーラーに必要なKanade.appが見つかりません。DMGを開き直してください。"
      if CommandLine.arguments.count == 1 {
        NSApplication.shared.setActivationPolicy(.regular)
        NSApplication.shared.activate(ignoringOtherApps: true)
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = "インストーラーを起動できませんでした"
        alert.informativeText = message
        alert.runModal()
      } else {
        FileHandle.standardError.write(Data((message + "\n").utf8))
      }
      exit(1)
    }
  }
}
