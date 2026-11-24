"""Exercise install.bat without registering an IME or running an MSI."""

import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
EXE = "kanade-ime-x86_64-pc-windows-msvc.exe"


@unittest.skipUnless(sys.platform == "win32", "Windows batch entry point")
class HeadInstallerTest(unittest.TestCase):
    """Check how the batch file selects and builds its package."""

    def test_build_reuse_and_failed_rebuild(self):
        """Install exactly the committed source and propagate packaging failures."""
        with tempfile.TemporaryDirectory(prefix="kanade install ") as temporary:
            work = Path(temporary) / "checkout"
            work.mkdir()
            record = Path(temporary) / "builds.txt"
            stub = Path(temporary) / "fixture.exe"
            env = dict(
                os.environ,
                GIT_CONFIG_GLOBAL=os.devnull,
                GIT_CONFIG_NOSYSTEM="1",
                GIT_CONFIG_COUNT="0",
                BUILD_RECORD=str(record),
                FIXTURE_EXE=str(stub),
            )

            def git(*args):
                return subprocess.check_output(
                    ["git", *args], cwd=work, env=env, text=True
                ).strip()

            for script in ("install.bat", "packaging/windows/install-head.ps1"):
                destination = work / script
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(ROOT / script, destination)
            packager = work / ".github/scripts/package-windows.ps1"
            packager.parent.mkdir(parents=True)
            packager.write_text(
                "param([switch]$Unsigned)\n"
                "$revision = Get-Content revision.txt\n"
                "Add-Content -LiteralPath $env:BUILD_RECORD -Value $revision\n"
                "if ($revision -eq 'failed') { exit 17 }\n"
                "New-Item -Force -ItemType Directory artifacts/release | Out-Null\n"
                f"Copy-Item $env:FIXTURE_EXE artifacts/release/{EXE} -Force\n"
            )
            (work / ".gitignore").write_text("artifacts/\n")
            if shutil.which("mise"):
                (work / "mise.toml").write_text("[tools]\n")
            revision = work / "revision.txt"
            revision.write_text("first")
            git("init", "-q")
            git("config", "user.name", "Fixture")
            git("config", "user.email", "test@example.com")
            git("add", ".")

            def commit():
                git("commit", "-qam", "fixture")
                return git("rev-parse", "HEAD")

            # Exit before install.bat can run any real profile or engine setup.
            subprocess.run(
                [
                    "powershell.exe",
                    "-NoProfile",
                    "-Command",
                    (
                        "Add-Type -TypeDefinition "
                        "'class Fixture { static int Main() { return 42; } }' "
                        "-OutputType WindowsApplication "
                        "-OutputAssembly $env:FIXTURE_EXE"
                    ),
                ],
                env=env,
                check=True,
                capture_output=True,
            )

            def install(expected):
                result = subprocess.run(
                    [os.environ["COMSPEC"], "/d", "/c", "install.bat"],
                    cwd=work,
                    env=env,
                    check=False,
                    capture_output=True,
                    timeout=90,
                )
                self.assertEqual(
                    result.returncode, expected, result.stdout + result.stderr
                )

            first = commit()
            install(42)
            install(42)
            cached = work / "artifacts/windows/install-head" / first / EXE
            cached.write_bytes(b"damaged")
            install(42)
            revision.write_text("second")
            commit()
            revision.write_text("uncommitted")
            install(42)
            self.assertEqual(revision.read_text(), "uncommitted")
            revision.write_text("failed")
            commit()
            install(1)
            self.assertEqual(
                record.read_text().splitlines(), ["first", "first", "second", "failed"]
            )


if __name__ == "__main__":
    unittest.main()
