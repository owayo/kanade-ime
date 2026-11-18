"""Stage or verify the Tsumugi release asset pinned for both installers."""

import argparse
import hashlib
import os
import shutil
import tempfile
import tomllib
import urllib.request
from pathlib import Path


def locked_model():
    lock = Path(__file__).resolve().parents[2] / "locks/llm.lock.toml"
    with lock.open("rb") as stream:
        return tomllib.load(stream)["model"]["tsumugi-s-f16"]


def verify_model(path, model):
    if path.is_symlink() or not path.is_file() or path.stat().st_size != model["size"]:
        raise ValueError(f"Missing or invalid Tsumugi model: {path}")
    with path.open("rb") as stream:
        if hashlib.file_digest(stream, "sha256").hexdigest() != model["sha256"]:
            raise ValueError(f"Tsumugi SHA-256 mismatch: {path}")


def stage(directory, source):
    model = locked_model()
    target = directory / model["file"]
    cached = Path("artifacts/models") / model["file"]
    if source is None and cached.is_file():
        source = cached
    directory.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=directory) as temporary:
        staged = Path(temporary) / model["file"]
        if source is None:
            with urllib.request.urlopen(model["download_url"], timeout=90) as response:
                with staged.open("wb") as output:
                    shutil.copyfileobj(response, output)
        else:
            shutil.copyfile(source, staged)
        verify_model(staged, model)
        staged.replace(target)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--model", type=Path, default=os.environ.get("KANADE_TSUMUGI_GGUF"))
    args = parser.parse_args()
    stage(args.directory, args.model)
