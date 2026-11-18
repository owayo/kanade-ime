"""Stage the pinned Tsumugi weights and per-user setup alongside the Windows IME."""
import argparse
import hashlib
import shutil
import tomllib
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("--model", type=Path)
args = parser.parse_args()
model = tomllib.loads(Path("locks/llm.lock.toml").read_text(encoding="utf-8"))["model"]["tsumugi-s-f16"]
name = model["file"]
source = args.model or Path("models") / name
if args.model is None and not source.is_file():
    source = Path("artifacts/models") / name
if not source.is_file():
    parser.error(f"Supply {name} with -ModelPath when building the installer")
payload = Path("artifacts/windows/payload")
target = payload / "models" / name
target.parent.mkdir(parents=True, exist_ok=True)
shutil.copyfile(source, target)
with target.open("rb") as stream:
    if target.stat().st_size != model["size"] or hashlib.file_digest(stream, "sha256").hexdigest() != model["sha256"]:
        target.unlink()
        raise ValueError("The supplied model does not match the locked Tsumugi size and SHA-256")
for script in ("start-engine.ps1", "setup-user.ps1", "enable-profile.ps1", "enable-tsumugi.ps1"):
    shutil.copyfile(Path("packaging/windows") / script, payload / script)
