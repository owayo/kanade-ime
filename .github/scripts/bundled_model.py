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
    """Read the identity of the single model shared by the two installers."""
    lock = Path(__file__).resolve().parents[2] / "locks/llm.lock.toml"
    with lock.open("rb") as stream:
        (model,) = tomllib.load(stream)["model"].values()
        return model


def verify_model(path, model):
    """Check that the bundled file matches the locked byte count and SHA-256."""
    if path.is_symlink() or not path.is_file() or path.stat().st_size != model["size"]:
        raise ValueError(f"Missing or invalid Tsumugi model: {path}")
    with path.open("rb") as stream:
        if hashlib.file_digest(stream, "sha256").hexdigest() != model["sha256"]:
            raise ValueError(f"Tsumugi SHA-256 mismatch: {path}")


def check_model_files(root, expected):
    """Require the expected model to be the only GGUF in the installation."""
    files = [p for p in root.rglob("*") if p.suffix.lower() == ".gguf"]
    if files != [expected]:
        raise ValueError(f"Expected exactly one bundled model at {expected}, found {files}")


def stage(directory, source):
    """Verify a temporary copy before replacing the staged model."""
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
    check_model_files(directory, target)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--model", type=Path, default=os.environ.get("KANADE_TSUMUGI_GGUF"))
    parser.add_argument("--verify-windows-install", action="store_true")
    args = parser.parse_args()
    if args.verify_windows_install:
        model = locked_model()
        expected = args.directory / "models" / model["file"]
        check_model_files(args.directory, expected)
        verify_model(expected, model)
    else:
        stage(args.directory, args.model)
