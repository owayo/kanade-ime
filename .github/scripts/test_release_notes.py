import hashlib
import unittest
import urllib.error
from unittest.mock import patch

import release_notes
from bundled_model import INSTALLERS

WEIGHTS = b"test model"
OTHER = b"model of an earlier lock"
MODEL = {
    "version": "26.11.900",
    "file": "tsumugi.gguf",
    "size": 66507360,
    "sha256": hashlib.sha256(WEIGHTS).hexdigest(),
}
APP = dict.fromkeys(INSTALLERS, b"installer")


def release(tag, published, files, prerelease=True):
    return {
        "tag_name": tag,
        "prerelease": prerelease,
        "published_at": published,
        "assets": [
            {"name": name, "digest": "sha256:" + hashlib.sha256(content).hexdigest()}
            for name, content in files.items()
        ],
    }


class ComparedReleaseTests(unittest.TestCase):
    def test_the_latest_other_release_of_the_channel_with_installers_is_compared(self):
        listing = [
            release("v3-preview.9", "2026-11-09T00:00:00Z", APP),
            release("v3", "2026-11-08T00:00:00Z", APP, prerelease=False),
            # Not a release of the app: it only carries a model.
            release("model-only", "2026-11-07T00:00:00Z", {MODEL["file"]: OTHER}),
            release("v2-preview.6", "2026-11-06T00:00:00Z", APP),
            release("v2-preview.4", "2026-11-04T00:00:00Z", APP),
            release("v1", "2026-11-01T00:00:00Z", APP, prerelease=False),
        ]
        preview = release_notes.compared(listing, "v3-preview.9", True)
        self.assertEqual(preview["tag_name"], "v2-preview.6")
        stable = release_notes.compared(listing, "v3", False)
        self.assertEqual(stable["tag_name"], "v1")
        self.assertIsNone(release_notes.compared(listing[:1], "v3-preview.9", True))


class UpdateNoticeTests(unittest.TestCase):
    def test_a_change_is_stated_only_when_the_compared_release_shows_it(self):
        changed = release("v1", "2026-11-01T00:00:00Z", {MODEL["file"]: OTHER})
        notice = release_notes.update_notice(MODEL, changed)
        lead = "`v1` と比べて、同梱モデルが Tsumugi 26.11.900 に替わりました。"
        self.assertEqual(notice, lead + release_notes.STEPS)
        same = release("v1", "2026-11-01T00:00:00Z", {MODEL["file"]: WEIGHTS})
        without_model = release("v1", "2026-11-01T00:00:00Z", APP)
        without_digest = release("v1", "2026-11-01T00:00:00Z", {MODEL["file"]: OTHER})
        del without_digest["assets"][0]["digest"]
        for unknown in (same, without_model, without_digest, None):
            self.assertEqual(
                release_notes.update_notice(MODEL, unknown), release_notes.STEPS
            )

    def test_the_steps_name_the_place_the_action_and_the_effect(self):
        for part in ("「ローカルAI」", "「Tsumugi を取り込む」", "働きません"):
            self.assertIn(part, release_notes.STEPS)


class TextTests(unittest.TestCase):
    def test_a_preview_names_the_model_the_notice_and_the_installer_tests(self):
        previous = release(
            "v1-preview.4", "2026-11-01T00:00:00Z", {"tsumugi.gguf": OTHER}
        )
        text = release_notes.preview(
            "26.11.901", MODEL, previous, "https://ci.invalid/1"
        )
        paragraphs = text.removesuffix("\n").split("\n\n")
        self.assertEqual(len(paragraphs), 6)
        self.assertIn("Kanade 26.11.901 のプレビュー版です。", paragraphs[0])
        self.assertIn("SHA256SUMS で照合してから", paragraphs[0])
        self.assertIn("docs/distribution.md#macos-プレビュー版を開く", paragraphs[0])
        self.assertEqual(
            paragraphs[1],
            "どちらにも Tsumugi 26.11.900（66,507,360 バイト）を同梱しています。",
        )
        self.assertTrue(paragraphs[2].startswith("`v1-preview.4` と比べて、"))
        self.assertIn(MODEL["sha256"], paragraphs[3])
        self.assertIn("`tsumugi.gguf`", paragraphs[4])
        self.assertEqual(
            paragraphs[5], "[両 OS のインストーラー検査](https://ci.invalid/1)"
        )
        self.assertTrue(text.endswith("検査](https://ci.invalid/1)\n"))

    def test_a_stable_release_keeps_its_guidance_and_adds_the_notice(self):
        text = release_notes.stable(MODEL, None)
        paragraphs = text.removesuffix("\n").split("\n\n")
        self.assertEqual(len(paragraphs), 4)
        self.assertIn("SHA256SUMSで照合してから", paragraphs[0])
        self.assertNotIn("macOS プレビュー版を開く手順", text)
        self.assertIn("Tsumugi 26.11.900", paragraphs[1])
        self.assertEqual(paragraphs[2], release_notes.STEPS)
        self.assertIn("`tsumugi.gguf`", paragraphs[3])


class ListingTests(unittest.TestCase):
    def test_a_listing_that_fails_leaves_the_steps_without_a_comparison(self):
        refusals = (
            ValueError("GitHub answered 403"),
            urllib.error.URLError("no route"),
        )
        for refusal in refusals:
            with patch.object(release_notes, "releases", side_effect=refusal):
                found = release_notes.previous_release("example/ime", "v2", True)
            self.assertIsNone(found)

    def test_the_listing_is_read_from_the_repository_being_released(self):
        listing = [release("v1-preview.4", "2026-11-01T00:00:00Z", APP)]
        with patch.object(release_notes, "releases", return_value=iter(listing)) as api:
            found = release_notes.previous_release("example/ime", "v2-preview.6", True)
        api.assert_called_once_with("example/ime")
        self.assertEqual(found["tag_name"], "v1-preview.4")


if __name__ == "__main__":
    unittest.main()
