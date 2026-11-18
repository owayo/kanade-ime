import Foundation
import Testing

@testable import KanadeInstaller

struct PayloadFixture {
  let installer: URL
  init(identifier: String) throws {
    installer = FileManager.default.temporaryDirectory
      .appendingPathComponent("Kanade installer \(UUID().uuidString).app")
    let payload = installer.appendingPathComponent("Contents/Helpers/Kanade.app/Contents")
    try FileManager.default.createDirectory(
      at: payload.appendingPathComponent("MacOS"), withIntermediateDirectories: true)
    try PropertyListSerialization.data(
      fromPropertyList: ["CFBundleIdentifier": identifier], format: .xml, options: 0
    ).write(to: payload.appendingPathComponent("Info.plist"))
    let executable = payload.appendingPathComponent("MacOS/kanade-installer")
    try Data("#!/bin/sh\nexit 99\n".utf8).write(to: executable)
    try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: executable.path)
  }
  func remove() { try? FileManager.default.removeItem(at: installer) }
}

@Test func payloadTravelsInsideTheInstaller() throws {
  let fixture = try PayloadFixture(identifier: "dev.kanade.inputmethod.Kanade")
  defer { fixture.remove() }
  let payload = try InstallerPayload(installer: fixture.installer)
  #expect(payload.application.path.hasPrefix(fixture.installer.path + "/Contents/Helpers/"))
  #expect(
    payload.arguments(uninstall: false, dryRun: true) == [
      "install-macos", "--bundle", payload.application.path, "--dry-run",
    ])
  #expect(payload.arguments(uninstall: true, dryRun: false).last == "--uninstall")
  // A valid-looking identity cannot execute an unsigned payload.
  #expect(payload.run(uninstall: false, dryRun: true) != 99)
  #expect(payload.run(uninstall: false, dryRun: true) != 0)
}

@Test func anotherApplicationCannotBecomeThePayload() throws {
  let fixture = try PayloadFixture(identifier: "com.example.Other")
  defer { fixture.remove() }
  #expect(throws: InstallerFailure.self) { try InstallerPayload(installer: fixture.installer) }
}
