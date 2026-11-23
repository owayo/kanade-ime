"""Stage or verify the Tsumugi model that both installers bundle."""

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import tempfile
import tomllib
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
# A verified copy that later builds reuse.
CACHE = ROOT / "artifacts/models"
# Public previews carry Windows only. Stable releases also carry a notarized DMG.
PREVIEW_INSTALLERS = ("kanade-ime-x86_64-pc-windows-msvc.exe",)
INSTALLERS = (
    "kanade-ime-aarch64-apple-darwin.dmg",
    *PREVIEW_INSTALLERS,
)


def locked_model():
    """Read the identity of the single model shared by the two installers.

    Returns:
        The model table of locks/llm.lock.toml.

    """
    with (ROOT / "locks/llm.lock.toml").open("rb") as stream:
        (model,) = tomllib.load(stream)["model"].values()
        return model


def verify_model(path, model):
    """Check that the bundled file matches the locked byte count and SHA-256.

    Raises:
        ValueError: The file is absent, a link, or another file than the locked one.

    """
    if path.is_symlink() or not path.is_file() or path.stat().st_size != model["size"]:
        raise ValueError(f"Missing or invalid Tsumugi model: {path}")
    with path.open("rb") as stream:
        if hashlib.file_digest(stream, "sha256").hexdigest() != model["sha256"]:
            raise ValueError(f"Tsumugi SHA-256 mismatch: {path}")


def check_model_files(root, expected):
    """Require the expected model to be the only GGUF in the installation.

    Raises:
        ValueError: Another GGUF is present, or the expected one is missing.

    """
    files = [p for p in root.rglob("*") if p.suffix.lower() == ".gguf"]
    if files != [expected]:
        raise ValueError(
            f"Expected exactly one bundled model at {expected}, found {files}"
        )


def releases(repository):
    """List the published releases of a repository through the GitHub API.

    Yields:
        Each release that is not a draft, with its assets.

    Raises:
        ValueError: The API refused the request.

    """
    headers = {"Accept": "application/vnd.github+json", "User-Agent": "kanade-build"}
    # Hosted runners share addresses, and requests without a token are few per address.
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"
    page = 1
    while True:
        query = f"releases?per_page=100&page={page}"
        url = f"https://api.github.com/repos/{repository}/{query}"
        request = urllib.request.Request(url, headers=headers)
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                batch = json.load(response)
        except urllib.error.HTTPError as error:
            raise ValueError(
                f"GitHub answered {error.code} for the releases of {repository}. "
                "Set GH_TOKEN when requests without a token are refused."
            ) from error
        if not batch:
            return
        yield from (release for release in batch if not release["draft"])
        page += 1


def carriers(model, listing):
    """Find the release assets that are the locked model, whatever their names.

    Yields:
        A release and its asset whose SHA-256 is the locked one.

    """
    digest = "sha256:" + model["sha256"]
    for release in listing:
        for asset in release["assets"]:
            if asset.get("digest") == digest and asset["state"] == "uploaded":
                yield release, asset


def staging_name(model):
    """Name a copy attached to a release whose installers bundle another model.

    Returns:
        The locked file name with the model version before its suffix.

    """
    file = Path(model["file"])
    return f"{file.stem}-{model['version']}{file.suffix}"


def fetch(model):
    """Keep a verified copy of the locked model in the cache.

    Returns:
        The path of that copy.

    Raises:
        ValueError: No release carries the model, or the download is another file.

    """
    cached = CACHE / model["file"]
    try:
        verify_model(cached, model)
        return cached
    except ValueError:
        pass  # Absent, or left by an earlier lock.
    repository = model["release_repository"]
    found = next(carriers(model, releases(repository)), None)
    if found is None:
        raise ValueError(
            f"No release of {repository} carries Tsumugi {model['version']}. "
            "Pass a copy with --model or KANADE_TSUMUGI_GGUF, or attach it to a "
            "release with --attach."
        )
    CACHE.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=CACHE) as temporary:
        staged = Path(temporary) / model["file"]
        url = found[1]["browser_download_url"]
        with (
            urllib.request.urlopen(url, timeout=90) as response,
            staged.open("wb") as output,
        ):
            shutil.copyfileobj(response, output)
        verify_model(staged, model)
        staged.replace(cached)
    return cached


def attach(source):
    """Attach the locked model to the newest release.

    Builds fetch it from there until a release of its own carries it.

    Raises:
        ValueError: The file is not the locked model, or no release exists.

    """
    model = locked_model()
    verify_model(source, model)
    repository = model["release_repository"]
    listing = list(releases(repository))
    found = next(carriers(model, listing), None)
    if found is not None:
        print(f"{found[0]['tag_name']} already carries it as {found[1]['name']}")
        return
    if not listing:
        raise ValueError(f"{repository} has no release to attach the model to")
    newest = max(listing, key=lambda release: release["published_at"])
    # The release page shows this label in place of the file name.
    label = f"次の版に同梱するモデル（Tsumugi {model['version']}）"
    with tempfile.TemporaryDirectory() as temporary:
        named = Path(temporary) / staging_name(model)
        shutil.copyfile(source, named)
        upload = ["gh", "release", "upload", newest["tag_name"], f"{named}#{label}"]
        subprocess.run([*upload, "--repo", repository], check=True)
    print(f"Attached {named.name} to {newest['tag_name']}")


def stage(directory, source):
    """Verify a temporary copy before replacing the staged model."""
    model = locked_model()
    target = directory / model["file"]
    if source is None:
        source = fetch(model)
    directory.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=directory) as temporary:
        staged = Path(temporary) / model["file"]
        shutil.copyfile(source, staged)
        verify_model(staged, model)
        staged.replace(target)
    check_model_files(directory, target)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path, nargs="?")
    parser.add_argument(
        "--model", type=Path, default=os.environ.get("KANADE_TSUMUGI_GGUF") or None
    )
    parser.add_argument("--verify-windows-install", action="store_true")
    parser.add_argument(
        "--fetch", action="store_true", help="keep a verified copy in artifacts/models"
    )
    parser.add_argument(
        "--attach",
        type=Path,
        metavar="GGUF",
        help="attach the locked model to a release",
    )
    args = parser.parse_args()
    if args.attach:
        attach(args.attach)
    elif args.fetch:
        print(fetch(locked_model()))
    elif args.directory is None:
        parser.error("a directory is required")
    elif args.verify_windows_install:
        model = locked_model()
        expected = args.directory / "models" / model["file"]
        check_model_files(args.directory, expected)
        verify_model(expected, model)
    else:
        stage(args.directory, args.model)
