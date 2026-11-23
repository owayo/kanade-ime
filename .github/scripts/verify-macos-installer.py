"""Verify a mounted DMG or a preview package's installed bundle."""

import argparse
import hashlib
import json
import os
import plistlib
import subprocess
import tempfile
from pathlib import Path

from bundled_model import check_model_files, locked_model, verify_model


def verify_payload(payload: Path, container: Path, version: str) -> None:
    """Verify bundle versions and the locked model and auxiliary executables.

    Raises:
        ValueError: Bundle versions or payload hashes differ.

    """
    settings = payload / "Contents/Helpers/Settings.app"
    bundles = (
        (payload, settings) if container == payload else (container, payload, settings)
    )
    for app in bundles:
        metadata = plistlib.loads((app / "Contents/Info.plist").read_bytes())
        if any(
            metadata.get(key) != version
            for key in ["CFBundleShortVersionString", "CFBundleVersion"]
        ):
            raise ValueError("installer and app versions differ")
    model = locked_model()
    gguf = settings / "Contents/Resources/models" / model["file"]
    check_model_files(container, gguf)
    verify_model(gguf, model)
    for manifest, binary in [
        ("manifest.json", "kanade-mozc-worker"),
        ("project-analyzer.json", "kanade-project-analyzer"),
    ]:
        manifest_data = (payload / "Contents/Resources" / manifest).read_text()
        expected = json.loads(manifest_data)["sha256"]
        with (payload / "Contents/MacOS" / binary).open("rb") as stream:
            digest = hashlib.file_digest(stream, "sha256").hexdigest()
        if expected != digest:
            raise ValueError("packaged executable hash differs")


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
            subprocess.run(
                ["codesign", "--verify", "--deep", "--strict", str(installer)],
                check=True,
                timeout=120,
            )
            verify_payload(payload, installer, version)
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
    parser.add_argument("--installed-preview", action="store_true")
    arguments = parser.parse_args()
    if arguments.installed_preview:
        subprocess.run(
            ["codesign", "--verify", "--deep", "--strict", str(arguments.image)],
            check=True,
            timeout=120,
        )
        verify_payload(arguments.image, arguments.image, arguments.version)
        print("Installed PKG payload, versions, signature and hashes passed")
    else:
        verify_dmg(arguments.image, arguments.version)
