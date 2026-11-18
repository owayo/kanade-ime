import hashlib
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "release_assets", Path(__file__).with_name("release-assets.py")
)
assets = importlib.util.module_from_spec(spec)
spec.loader.exec_module(assets)


class InstallerIntegrityTests(unittest.TestCase):
    def populate(self, folder):
        for name in assets.ASSETS:
            (folder / name).write_bytes(b"test installer " + name.encode())

    def test_exact_installers_and_published_checksums(self):
        with tempfile.TemporaryDirectory() as directory:
            folder = Path(directory)
            self.populate(folder)
            assets.checksums(folder, 2)
            metadata = {"name": "v26.11.101", "tag_name": "v26.11.101", "assets": []}
            for path in folder.iterdir():
                metadata["assets"].append(
                    {
                        "name": path.name,
                        "size": path.stat().st_size,
                        "state": "uploaded",
                        "digest": "sha256:"
                        + hashlib.sha256(path.read_bytes()).hexdigest(),
                    }
                )
            with patch.object(
                assets.subprocess,
                "check_output",
                side_effect=lambda *a, **kw: json.dumps(metadata).encode(),
            ):
                assets.verify(folder, "v26.11.101", "example/ime")
                metadata["assets"][0]["digest"] = "sha256:incorrect"
                with self.assertRaises(ValueError):
                    assets.verify(folder, "v26.11.101", "example/ime")

    def test_unpacked_executable_missing_installer_and_extra_platform_are_refused(self):
        for name in [
            "kanade-ui.exe",
            "kanade-ime-x86_64-unknown-linux-gnu.tar.gz",
            "kanade-ime-x86_64-pc-windows-msvc.zip",
            "kanade-ime-aarch64-apple-darwin.tar.gz",
            ".hidden",
        ]:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                folder = Path(directory)
                self.populate(folder)
                (folder / name).write_bytes(b"unexpected")
                with self.assertRaises(ValueError):
                    assets.checksums(folder, 2)
        with tempfile.TemporaryDirectory() as directory:
            folder = Path(directory)
            self.populate(folder)
            (folder / assets.ASSETS[0]).unlink()
            with self.assertRaises(ValueError):
                assets.checksums(folder, 2)

    def test_empty_file_directory_and_symlink_are_refused(self):
        for kind in ["empty", "directory", "symlink"]:
            with self.subTest(kind=kind), tempfile.TemporaryDirectory() as directory:
                folder = Path(directory)
                self.populate(folder)
                path = folder / assets.ASSETS[0]
                path.unlink()
                if kind == "empty":
                    path.touch()
                elif kind == "directory":
                    path.mkdir()
                else:
                    path.symlink_to(folder / assets.ASSETS[1])
                with self.assertRaises(ValueError):
                    assets.checksums(folder, 2)


if __name__ == "__main__":
    unittest.main()
