"""Verify the mounted DMG without installing an app or changing input sources."""

import argparse
import hashlib
import json
import os
import plistlib
import subprocess
import tempfile
from pathlib import Path


def verify_dmg(image: Path, version: str) -> None:
    """Check the signed, self-contained installer in a mounted disk image.

    Raises:
        ValueError: Payload metadata, hashes or preflight effects differ.

    """
    with tempfile.TemporaryDirectory(
        prefix="kanade dmg ", suffix=".noindex"
    ) as temporary:
        folder = Path(temporary)
        mount = folder / "image"
        mount.mkdir()
        try:
            subprocess.run(
                [
                    "hdiutil",
                    "attach",
                    "-readonly",
                    "-nobrowse",
                    "-mountpoint",
                    str(mount),
                    str(image.resolve()),
                ],
                check=True,
                timeout=120,
                stdout=subprocess.DEVNULL,
            )
            installer = mount / "Kanade インストーラー.app"
            payload = installer / "Contents/Helpers/Kanade.app"
            settings = payload / "Contents/Helpers/Settings.app"
            subprocess.run(
                ["codesign", "--verify", "--deep", "--strict", str(installer)],
                check=True,
                timeout=120,
            )
            for app in [installer, payload, settings]:
                metadata = plistlib.loads((app / "Contents/Info.plist").read_bytes())
                if any(
                    metadata.get(key) != version
                    for key in ["CFBundleShortVersionString", "CFBundleVersion"]
                ):
                    raise ValueError("installer and app versions differ")
            for manifest, binary in [
                ("manifest.json", "kanade-mozc-worker"),
                ("project-analyzer.json", "kanade-project-analyzer"),
            ]:
                expected = json.loads(
                    (payload / "Contents/Resources" / manifest).read_text()
                )["sha256"]
                with (payload / "Contents/MacOS" / binary).open("rb") as stream:
                    digest = hashlib.file_digest(stream, "sha256").hexdigest()
                if expected != digest:
                    raise ValueError("packaged executable hash differs")
            executable = installer / "Contents/MacOS/KanadeInstaller"
            environment = os.environ.copy()
            environment["KANADE_INSTALL_DIR"] = str(folder / "Input Methods")
            environment["KANADE_DATA_DIR"] = str(folder / "Personal Data")
            for argument in ["--check", "--dry-run", "--uninstall-dry-run"]:
                subprocess.run(
                    [str(executable), argument],
                    env=environment,
                    check=True,
                    timeout=120,
                )
            if (folder / "Input Methods/Kanade.app").exists() or (
                folder / "Personal Data"
            ).exists():
                raise ValueError("preflight changed installation or personal data")
        finally:
            if mount.is_mount():
                subprocess.run(
                    ["hdiutil", "detach", str(mount)],
                    check=True,
                    timeout=120,
                    stdout=subprocess.DEVNULL,
                )
    print("DMG payload, versions, signatures, hashes and installer preflight passed")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("image", type=Path)
    parser.add_argument("--version", required=True)
    arguments = parser.parse_args()
    verify_dmg(arguments.image, arguments.version)
