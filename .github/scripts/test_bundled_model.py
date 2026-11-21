import hashlib
import io
import json
import tempfile
import unittest
import urllib.error
from pathlib import Path
from unittest.mock import patch

import bundled_model

WEIGHTS = b"test model"
OTHER = b"other bytes"
MODEL = {
    "version": "26.11.900",
    "file": "tsumugi.gguf",
    "size": len(WEIGHTS),
    "sha256": hashlib.sha256(WEIGHTS).hexdigest(),
    "release_repository": "example/ime",
}
STAGED = "tsumugi-26.11.900.gguf"


def release(tag, published, files, draft=False):
    return {
        "tag_name": tag,
        "draft": draft,
        "published_at": published,
        "assets": [
            {
                "name": name,
                "state": "uploaded",
                "digest": "sha256:" + hashlib.sha256(content).hexdigest(),
                "browser_download_url": f"https://example.invalid/{tag}/{name}",
            }
            for name, content in files.items()
        ],
    }


class FetchTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.cache = Path(directory.name) / "models"
        for target, value in [("CACHE", self.cache), ("locked_model", lambda: MODEL)]:
            patcher = patch.object(bundled_model, target, value)
            patcher.start()
            self.addCleanup(patcher.stop)

    def served(self, content):
        return patch.object(
            bundled_model.urllib.request,
            "urlopen",
            side_effect=lambda *a, **kw: io.BytesIO(content),
        )

    def test_the_model_is_found_by_digest_whatever_the_asset_is_called(self):
        listing = [
            release("v2", "2026-11-02T00:00:00Z", {"tsumugi.gguf": OTHER}),
            release(
                "v1", "2026-11-01T00:00:00Z", {"installer.exe": OTHER, STAGED: WEIGHTS}
            ),
        ]
        with (
            patch.object(bundled_model, "releases", return_value=iter(listing)) as api,
            self.served(WEIGHTS) as download,
        ):
            path = bundled_model.fetch(MODEL)
            self.assertEqual(path.read_bytes(), WEIGHTS)
            self.assertEqual(
                download.call_args.args[0], f"https://example.invalid/v1/{STAGED}"
            )
            # The verified copy answers the next request without the network.
            self.assertEqual(bundled_model.fetch(MODEL), path)
            self.assertEqual((api.call_count, download.call_count), (1, 1))

    def test_a_copy_left_by_another_lock_is_replaced(self):
        self.cache.mkdir()
        (self.cache / MODEL["file"]).write_bytes(b"model of an earlier lock")
        listing = [release("v1", "2026-11-01T00:00:00Z", {MODEL["file"]: WEIGHTS})]
        with (
            patch.object(bundled_model, "releases", return_value=iter(listing)),
            self.served(WEIGHTS),
        ):
            self.assertEqual(bundled_model.fetch(MODEL).read_bytes(), WEIGHTS)

    def test_no_carrier_and_a_wrong_download_are_refused(self):
        other = [release("v1", "2026-11-01T00:00:00Z", {MODEL["file"]: OTHER})]
        with (
            patch.object(bundled_model, "releases", return_value=iter(other)),
            self.served(WEIGHTS) as download,
            self.assertRaises(ValueError),
        ):
            bundled_model.fetch(MODEL)
        download.assert_not_called()
        listing = [release("v1", "2026-11-01T00:00:00Z", {MODEL["file"]: WEIGHTS})]
        with (
            patch.object(bundled_model, "releases", return_value=iter(listing)),
            self.served(WEIGHTS[::-1]),
            self.assertRaises(ValueError),
        ):
            bundled_model.fetch(MODEL)
        self.assertFalse((self.cache / MODEL["file"]).exists())

    def test_staging_verifies_the_copy_it_places(self):
        self.cache.mkdir()
        (self.cache / MODEL["file"]).write_bytes(WEIGHTS)
        payload = self.cache.parent / "payload" / "models"
        bundled_model.stage(payload, None)
        self.assertEqual((payload / MODEL["file"]).read_bytes(), WEIGHTS)
        source = self.cache.parent / "other.gguf"
        source.write_bytes(WEIGHTS[::-1])
        with self.assertRaises(ValueError):
            bundled_model.stage(self.cache.parent / "refused" / "models", source)


class AttachTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.source = Path(directory.name) / "exported.gguf"
        self.source.write_bytes(WEIGHTS)
        patcher = patch.object(bundled_model, "locked_model", return_value=MODEL)
        patcher.start()
        self.addCleanup(patcher.stop)

    def attach(self, listing, source=None):
        with (
            patch.object(bundled_model, "releases", return_value=iter(listing)),
            patch.object(bundled_model.subprocess, "run") as run,
        ):
            bundled_model.attach(source or self.source)
        return run

    def test_the_locked_model_goes_to_the_newest_release_under_its_staging_name(self):
        listing = [
            release("v1", "2026-11-01T00:00:00Z", {"installer.exe": OTHER}),
            release("v2", "2026-11-03T00:00:00Z", {"installer.exe": OTHER}),
            release("other", "2026-11-02T00:00:00Z", {"tsumugi.gguf": OTHER}),
        ]
        (call,) = self.attach(listing).call_args_list
        command = call.args[0]
        self.assertEqual(command[:4], ["gh", "release", "upload", "v2"])
        self.assertEqual(command[-2:], ["--repo", "example/ime"])
        path, label = command[4].split("#")
        self.assertEqual(Path(path).name, STAGED)
        self.assertIn("26.11.900", label)

    def test_nothing_is_uploaded_when_a_release_already_carries_it(self):
        listing = [release("v1", "2026-11-01T00:00:00Z", {STAGED: WEIGHTS})]
        self.attach(listing).assert_not_called()

    def test_another_file_and_a_repository_without_releases_are_refused(self):
        other = self.source.with_name("another.gguf")
        other.write_bytes(WEIGHTS[::-1])
        listing = [release("v1", "2026-11-01T00:00:00Z", {"installer.exe": OTHER})]
        with self.assertRaises(ValueError):
            self.attach(listing, other)
        with self.assertRaises(ValueError):
            self.attach([])


class ReleaseListingTests(unittest.TestCase):
    def test_pages_are_followed_drafts_skipped_and_the_token_sent(self):
        pages = [
            [
                release("v2", "2026-11-02T00:00:00Z", {}),
                release("draft", None, {}, draft=True),
            ],
            [release("v1", "2026-11-01T00:00:00Z", {})],
            [],
        ]
        requests = []

        def answer(request, timeout):
            requests.append(request)
            return io.BytesIO(json.dumps(pages[len(requests) - 1]).encode())

        environment = {"GH_TOKEN": "token-for-the-test"}
        with (
            patch.object(bundled_model.urllib.request, "urlopen", side_effect=answer),
            patch.dict(bundled_model.os.environ, environment),
        ):
            tags = [r["tag_name"] for r in bundled_model.releases("example/ime")]
        self.assertEqual(tags, ["v2", "v1"])
        self.assertEqual(len(requests), 3)
        self.assertIn("/repos/example/ime/releases?", requests[0].full_url)
        self.assertEqual(
            requests[0].get_header("Authorization"), "Bearer token-for-the-test"
        )

    def test_a_refused_request_is_reported_with_its_status(self):
        refusal = urllib.error.HTTPError("https://example.invalid", 403, "", {}, None)
        with (
            patch.object(bundled_model.urllib.request, "urlopen", side_effect=refusal),
            self.assertRaisesRegex(ValueError, "403"),
        ):
            list(bundled_model.releases("example/ime"))


if __name__ == "__main__":
    unittest.main()
