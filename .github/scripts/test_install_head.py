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


def build_fixture(path):
    """Build a stub that records its path and exits before profile or engine setup."""
    source = """
using System;
using System.IO;
class Fixture {
    static int Main() {
        File.WriteAllText(Environment.GetEnvironmentVariable("LAUNCH_RECORD"),
                          typeof(Fixture).Assembly.Location);
        return 42;
    }
}
"""
    subprocess.run(
        [
            "powershell.exe",
            "-NoProfile",
            "-Command",
            (
                "Add-Type -TypeDefinition $env:FIXTURE_SOURCE "
                "-OutputType WindowsApplication -OutputAssembly $env:FIXTURE_EXE"
            ),
        ],
        env=dict(os.environ, FIXTURE_EXE=str(path), FIXTURE_SOURCE=source),
        check=True,
        capture_output=True,
    )


@unittest.skipUnless(sys.platform == "win32", "Windows batch entry point")
class HeadInstallerTest(unittest.TestCase):
    """Check how the batch file selects and builds its package."""

    def install(self, work, env, expected):
        """Run the batch entry point and check its exit code."""
        result = subprocess.run(
            [os.environ["COMSPEC"], "/d", "/c", "install.bat"],
            cwd=work,
            env=env,
            check=False,
            capture_output=True,
            timeout=90,
        )
        self.assertEqual(result.returncode, expected, result.stdout + result.stderr)

    def test_build_reuse_and_failed_rebuild(self):
        """Install exactly the committed source and propagate packaging failures."""
        with tempfile.TemporaryDirectory(prefix="kanade install ") as temporary:
            work = Path(temporary) / "checkout"
            work.mkdir()
            record = Path(temporary) / "builds.txt"
            launched = Path(temporary) / "launched.txt"
            stub = Path(temporary) / "fixture.exe"
            env = dict(
                os.environ,
                GIT_CONFIG_GLOBAL=os.devnull,
                GIT_CONFIG_NOSYSTEM="1",
                GIT_CONFIG_COUNT="0",
                BUILD_RECORD=str(record),
                FIXTURE_EXE=str(stub),
                LAUNCH_RECORD=str(launched),
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

            build_fixture(stub)
            release = work / "artifacts/release" / EXE
            release.parent.mkdir(parents=True)
            shutil.copyfile(stub, release)
            first = commit()
            self.install(work, env, 42)
            cached = work / "artifacts/windows/install-head" / first / EXE
            self.assertTrue(cached.samefile(launched.read_text(encoding="utf-8")))
            self.install(work, env, 42)
            cached.write_bytes(b"damaged")
            self.install(work, env, 42)
            revision.write_text("second")
            second = commit()
            revision.write_text("uncommitted")
            self.install(work, env, 42)
            cached = work / "artifacts/windows/install-head" / second / EXE
            self.assertTrue(cached.samefile(launched.read_text(encoding="utf-8")))
            self.assertEqual(revision.read_text(), "uncommitted")
            revision.write_text("failed")
            commit()
            launched.unlink()
            self.install(work, env, 1)
            self.assertFalse(launched.exists())
            self.assertEqual(
                record.read_text().splitlines(), ["first", "first", "second", "failed"]
            )

    def test_distribution_prefers_adjacent_setup(self):
        """Prefer the distributed EXE and retain the build-directory fallback."""
        with tempfile.TemporaryDirectory(prefix="kanade release ") as temporary:
            work = Path(temporary)
            shutil.copyfile(ROOT / "install.bat", work / "install.bat")
            adjacent = work / EXE
            release = work / "artifacts/release" / EXE
            release.parent.mkdir(parents=True)
            build_fixture(adjacent)
            shutil.copyfile(adjacent, release)
            launched = work / "launched.txt"
            env = dict(os.environ, LAUNCH_RECORD=str(launched))
            self.install(work, env, 42)
            self.assertTrue(adjacent.samefile(launched.read_text(encoding="utf-8")))
            adjacent.unlink()
            self.install(work, env, 42)
            self.assertTrue(release.samefile(launched.read_text(encoding="utf-8")))


if __name__ == "__main__":
    unittest.main()
