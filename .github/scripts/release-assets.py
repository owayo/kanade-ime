"""Validate the exact release set; never accept unpacked app binaries."""

import argparse
import hashlib
import json
import subprocess
import time
from pathlib import Path

from bundled_model import (
    INSTALLERS,
    PREVIEW_INSTALLERS,
    carriers,
    locked_model,
    releases,
    staging_name,
    verify_model,
)


def checksums(folder: Path, expected: int, *, preview: bool = False) -> None:
    """Reject unexpected files and hash the installers and the model they bundle.

    Raises:
        ValueError: The set, kind or size of the files is invalid.

    """
    model = locked_model()
    entries = sorted(folder.iterdir())
    installers = PREVIEW_INSTALLERS if preview else INSTALLERS
    names = {*installers, model["file"]}
    if expected != len(installers) or {p.name for p in entries} != names:
        raise ValueError(
            "release must contain exactly the installers for its channel "
            "and the model they bundle"
        )
    for p in entries:
        if p.is_symlink() or not p.is_file() or p.stat().st_size == 0:
            raise ValueError("release assets must be non-empty regular files")
    verify_model(folder / model["file"], model)
    lines = []
    for p in entries:
        with p.open("rb") as stream:
            lines.append(
                f"{hashlib.file_digest(stream, 'sha256').hexdigest()}  {p.name}\n"
            )
    (folder / "SHA256SUMS").write_text("".join(lines))


def verify(folder: Path, tag: str, repo: str, *, preview: bool = False) -> None:
    """Match the public release metadata to the locally checked files.

    Raises:
        ValueError: A public asset differs from the local set.

    """
    release = json.loads(
        subprocess.check_output(
            [
                "gh",
                "api",
                f"repos/{repo}/releases/tags/{tag}",
            ]
        )
    )
    if release["name"] != tag or release["tag_name"] != tag:
        raise ValueError("published release title or tag differs")
    if release["prerelease"] != preview:
        raise ValueError("published release channel differs")
    assets = {p["name"]: p for p in release["assets"]}
    installers = PREVIEW_INSTALLERS if preview else INSTALLERS
    expected = {*installers, locked_model()["file"], "SHA256SUMS"}
    if set(assets) != expected:
        raise ValueError("published release asset names differ")
    for name in expected:
        p = folder / name
        remote = assets[name]
        with p.open("rb") as stream:
            digest = hashlib.file_digest(stream, "sha256").hexdigest()
        if remote["state"] != "uploaded" or remote["size"] != p.stat().st_size:
            raise ValueError("published release asset state or size differs")
        if remote.get("digest") != "sha256:" + digest:
            raise ValueError(
                "published release asset checksum differs or is unavailable"
            )


def staged_copies(listing: list, tag: str, model: dict) -> list:
    """Select the copies that builds used before this release carried the model.

    Returns:
        The assets of other releases that are the model under its staging name.

    Raises:
        ValueError: The release itself does not carry the model under its own name.

    """
    found = list(carriers(model, listing))
    own = (model["file"], staging_name(model))
    if not any(r["tag_name"] == tag and a["name"] == own[0] for r, a in found):
        raise ValueError("the release does not carry the model it bundles")
    return [a for r, a in found if r["tag_name"] != tag and a["name"] == own[1]]


def prune(tag: str, repo: str) -> None:
    """Remove the model's staging copies now that its own release carries it.

    Raises:
        ValueError: The release does not list the model, also after waiting.

    """
    model = locked_model()
    # A new asset can take a few seconds to appear in the listing.
    for remaining in (2, 1, 0):
        try:
            copies = staged_copies(list(releases(repo)), tag, model)
            break
        except ValueError:
            if not remaining:
                raise
            time.sleep(10)
    for asset in copies:
        path = f"repos/{repo}/releases/assets/{asset['id']}"
        subprocess.run(["gh", "api", "--method", "DELETE", path], check=True)
        print(f"Removed {asset['name']} ({asset['id']})")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=["checksums", "verify", "prune"])
    parser.add_argument("folder", type=Path, nargs="?")
    parser.add_argument("--expected", type=int, default=2)
    parser.add_argument("--preview", action="store_true")
    parser.add_argument("--tag")
    parser.add_argument("--repo")
    args = parser.parse_args()
    if args.mode != "prune" and args.folder is None:
        parser.error(f"{args.mode} requires a folder")
    if args.mode == "checksums":
        checksums(args.folder, args.expected, preview=args.preview)
    elif not args.tag or not args.repo:
        parser.error(f"{args.mode} requires --tag and --repo")
    elif args.mode == "verify":
        verify(args.folder, args.tag, args.repo, preview=args.preview)
    else:
        prune(args.tag, args.repo)
