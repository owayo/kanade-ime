"""Validate the exact native installer set; never accept unpacked app binaries."""

import argparse
import hashlib
import json
import subprocess
from pathlib import Path

ASSETS = (
    "kanade-ime-aarch64-apple-darwin.dmg",
    "kanade-ime-x86_64-pc-windows-msvc.exe",
)


def checksums(folder: Path, expected: int) -> None:
    """Reject unexpected files and hash exactly the supported installers.

    Raises:
        ValueError: The set, kind or size of the installers is invalid.

    """
    entries = sorted(folder.iterdir())
    if expected != len(ASSETS) or {p.name for p in entries} != set(ASSETS):
        raise ValueError(
            "release must contain exactly the two supported native installers"
        )
    for p in entries:
        if p.is_symlink() or not p.is_file() or p.stat().st_size == 0:
            raise ValueError("release assets must be non-empty regular files")
    lines = []
    for p in entries:
        with p.open("rb") as stream:
            lines.append(
                f"{hashlib.file_digest(stream, 'sha256').hexdigest()}  {p.name}\n"
            )
    (folder / "SHA256SUMS").write_text("".join(lines))


def verify(folder: Path, tag: str, repo: str) -> None:
    """Match the public release metadata to the locally checked installers.

    Raises:
        ValueError: A public asset differs from the local installer set.

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
    assets = {p["name"]: p for p in release["assets"]}
    expected = {*ASSETS, "SHA256SUMS"}
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


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=["checksums", "verify"])
    parser.add_argument("folder", type=Path)
    parser.add_argument("--expected", type=int, default=2)
    parser.add_argument("--tag")
    parser.add_argument("--repo")
    args = parser.parse_args()
    if args.mode == "checksums":
        checksums(args.folder, args.expected)
    else:
        if not args.tag or not args.repo:
            parser.error("verify requires --tag and --repo")
        verify(args.folder, args.tag, args.repo)
