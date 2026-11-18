# Windows packaging

The release target is x64 Windows, including its x86 application hosts. The release
EXE contains the signed `Kanade.msi`; native ARM64 is not a release target.
`.github/scripts/package-windows.ps1` explicitly builds the pinned worker and both
TSF architectures, stages the engine, settings, optional CPU model worker, pinned
project analyzer and notices, signs the binaries, then builds and signs the MSI and EXE.
`Package.wxs` installs adjacent executables and pairs registration with rollback.
Current Microsoft Visual C++ Redistributables for x64 and x86 are prerequisites.

The disposable-runner rehearsal forces an error after registration, checks its
rollback, installs normally, verifies both COM views and engine management, and
uninstalls. The first Windows CI run linked both frontends but stopped during the worker build.
The complete installer rehearsal still needs a successful CI result.
Physical TSF input, AppContainer hosts, upgrades with loaded DLLs, interruption and
repair remain native release gates. See [distribution details](../../docs/distribution.md).

Build `kanade-tip` for `x86_64-pc-windows-msvc`, `i686-pc-windows-msvc`, and `aarch64-pc-windows-msvc` on Windows with the corresponding MSVC tools. Do not run registration from `DllMain`.

For ARM64 Windows, place the x64 and ARM64 implementations beside the forwarder as `kanade_tip_x64.dll` and `kanade_tip_arm64.dll`. Run `build-forwarder.cmd` from the repository root in an ARM64 Developer Command Prompt. The script follows Microsoft's [pure forwarder procedure](https://learn.microsoft.com/en-us/windows/arm/arm64x-build#building-an-arm64x-pure-forwarder-dll). It has not been executed on Windows yet.

`DllRegisterServer` writes HKLM and needs elevation. An installer must install the architecture-specific files, register each required view, create the per-user engine startup entry, and roll back registrations if installation fails. Keep the engine and settings executables beside each other for named-pipe server path validation. Rename loaded DLLs before replacing them during updates.

Signed MSI generation and installer custom-action validation remain release gates. Do not distribute a folder of DLLs as an installed IME. Run the application and AppContainer test matrix in `docs/status.md` before distribution.

## EXE setup

The released EXE uses WiX Burn to embed the MSI. `Bundle.wxs` selects the Japanese standard bootstrapper. `package-windows.ps1` signs the MSI, detaches and signs the Burn engine, reattaches it and signs the whole EXE. CI tests the MSI rollback path, installs through the EXE, runs the installed engine with a temporary profile and uninstalls through the EXE. The bootstrapper license is included in the MSI payload.

The Windows MSI includes the locked Tsumugi F16 GGUF. The packaging script accepts `-ModelPath` or `KANADE_TSUMUGI_GGUF` for a local build. It checks the staged bytes against `locks/llm.lock.toml` before creating the embedded cabinet. The settings window imports the bundled copy with the user’s consent.

For GitHub Actions, provide the locked file in `models/` or set the repository variable `KANADE_TSUMUGI_MODEL_URL` to its HTTPS distribution URL. CI builds the distributable EXE when those bytes are available. Release packaging requires them.
