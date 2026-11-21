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

WEIGHTS = b"test model"
MODEL = {
    "version": "26.11.900",
    "file": "tsumugi.gguf",
    "size": len(WEIGHTS),
    "sha256": hashlib.sha256(WEIGHTS).hexdigest(),
    "release_repository": "example/ime",
}
STAGED = "tsumugi-26.11.900.gguf"


def asset(identifier, name, content=WEIGHTS):
    return {
        "id": identifier,
        "name": name,
        "state": "uploaded",
        "digest": "sha256:" + hashlib.sha256(content).hexdigest(),
    }


class InstallerIntegrityTests(unittest.TestCase):
    def setUp(self):
        locked = patch.object(assets, "locked_model", return_value=MODEL)
        locked.start()
        self.addCleanup(locked.stop)

    def populate(self, folder):
        for name in assets.INSTALLERS:
            (folder / name).write_bytes(b"test installer " + name.encode())
        (folder / MODEL["file"]).write_bytes(WEIGHTS)

    def test_exact_installers_and_published_checksums(self):
        with tempfile.TemporaryDirectory() as directory:
            folder = Path(directory)
            self.populate(folder)
            assets.checksums(folder, 2)
            listed = (folder / "SHA256SUMS").read_text().splitlines()
            self.assertEqual(
                [line.split("  ")[1] for line in listed],
                sorted([*assets.INSTALLERS, MODEL["file"]]),
            )
            self.assertIn(f"{MODEL['sha256']}  {MODEL['file']}", listed)
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
                # A release without the model its installers bundle is refused.
                metadata["assets"] = [
                    a for a in metadata["assets"] if a["name"] != MODEL["file"]
                ]
                with self.assertRaises(ValueError):
                    assets.verify(folder, "v26.11.101", "example/ime")

    def test_unpacked_executable_missing_installer_and_extra_platform_are_refused(self):
        for name in [
            "kanade-ui.exe",
            "kanade-ime-x86_64-unknown-linux-gnu.tar.gz",
            "kanade-ime-x86_64-pc-windows-msvc.zip",
            "kanade-ime-aarch64-apple-darwin.tar.gz",
            ".hidden",
            STAGED,
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
            (folder / assets.INSTALLERS[0]).unlink()
            with self.assertRaises(ValueError):
                assets.checksums(folder, 2)

    def test_a_missing_model_and_another_model_are_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            folder = Path(directory)
            self.populate(folder)
            (folder / MODEL["file"]).unlink()
            with self.assertRaises(ValueError):
                assets.checksums(folder, 2)
            # Same length, other bytes: only the digest tells them apart.
            (folder / MODEL["file"]).write_bytes(WEIGHTS[::-1])
            with self.assertRaises(ValueError):
                assets.checksums(folder, 2)
            self.assertFalse((folder / "SHA256SUMS").exists())

    def test_empty_file_directory_and_symlink_are_refused(self):
        for kind in ["empty", "directory", "symlink"]:
            with self.subTest(kind=kind), tempfile.TemporaryDirectory() as directory:
                folder = Path(directory)
                self.populate(folder)
                path = folder / assets.INSTALLERS[0]
                path.unlink()
                if kind == "empty":
                    path.touch()
                elif kind == "directory":
                    path.mkdir()
                else:
                    path.symlink_to(folder / assets.INSTALLERS[1])
                with self.assertRaises(ValueError):
                    assets.checksums(folder, 2)


class StagedCopyTests(unittest.TestCase):
    def setUp(self):
        locked = patch.object(assets, "locked_model", return_value=MODEL)
        locked.start()
        self.addCleanup(locked.stop)
        self.listing = [
            {"tag_name": "v2", "assets": [asset(1, MODEL["file"])]},
            {
                "tag_name": "v1",
                "assets": [asset(2, STAGED), asset(3, MODEL["file"], b"older model")],
            },
            # A release that only ever held the model keeps it.
            {"tag_name": "model-only", "assets": [asset(4, MODEL["file"])]},
            # Another file under the staging name is left alone.
            {"tag_name": "v0", "assets": [asset(5, STAGED, b"other bytes")]},
        ]

    def test_only_staged_copies_of_the_released_model_are_selected(self):
        chosen = assets.staged_copies(self.listing, "v2", MODEL)
        self.assertEqual([a["id"] for a in chosen], [2])

    def test_nothing_is_selected_unless_the_release_carries_the_model(self):
        # v1 holds the model under its staging name only; the last tag is absent.
        for tag in ["v1", "v0", "absent"]:
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                assets.staged_copies(self.listing, tag, MODEL)

    def prune(self, tag, listings):
        with (
            patch.object(assets, "releases", side_effect=listings) as api,
            patch.object(assets.subprocess, "run") as run,
            patch.object(assets.time, "sleep") as sleep,
        ):
            try:
                assets.prune(tag, "example/ime")
            finally:
                self.calls = (api.call_count, sleep.call_count)
        return [call.args[0] for call in run.call_args_list]

    def test_prune_deletes_exactly_the_selected_assets(self):
        delete = ["gh", "api", "--method", "DELETE"]
        commands = self.prune("v2", lambda repo: iter(self.listing))
        self.assertEqual(commands, [[*delete, "repos/example/ime/releases/assets/2"]])
        self.assertEqual(self.calls, (1, 0))

    def test_prune_waits_for_a_listing_that_shows_the_new_asset(self):
        late = [self.listing[1:], self.listing]
        commands = self.prune("v2", lambda repo: iter(late.pop(0)))
        self.assertEqual(len(commands), 1)
        self.assertEqual(self.calls, (2, 1))

    def test_prune_deletes_nothing_for_a_release_without_the_model(self):
        with self.assertRaises(ValueError):
            self.prune("v1", lambda repo: iter(self.listing))
        self.assertEqual(self.calls, (3, 2))
        with (
            patch.object(assets, "releases", side_effect=lambda r: iter(self.listing)),
            patch.object(assets.subprocess, "run") as run,
            patch.object(assets.time, "sleep"),
            self.assertRaises(ValueError),
        ):
            assets.prune("v1", "example/ime")
        run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
