"""Create stable MSI file components and refresh hashes after Authenticode signing."""

import hashlib
import json
import tomllib
import uuid
import xml.etree.ElementTree as ET
from pathlib import Path

NS = "http://wixtoolset.org/schemas/v4/wxs"
ET.register_namespace("", NS)
payload = Path("artifacts/windows/payload").resolve()
version = tomllib.loads(Path("Cargo.toml").read_text())["workspace"]["package"][
    "version"
]
major, minor, build = map(int, version.split("."))
if major > 255 or minor > 255 or build > 65535:
    raise ValueError("version exceeds MSI ProductVersion limits")

for metadata, binary in [
    ("manifest.json", "kanade-mozc-worker.exe"),
    ("project-analyzer.json", "kanade-project-analyzer.exe"),
]:
    path = payload / metadata
    data = json.loads(path.read_text())
    with (payload / binary).open("rb") as stream:
        data["sha256"] = hashlib.file_digest(stream, "sha256").hexdigest()
    path.write_text(json.dumps(data, indent=2) + "\n")

root = ET.Element(f"{{{NS}}}Wix")
fragment = ET.SubElement(root, f"{{{NS}}}Fragment")
group = ET.SubElement(
    fragment, f"{{{NS}}}ComponentGroup", Id="Payload", Directory="INSTALLDIR"
)
directories = {Path("."): "INSTALLDIR"}
for path in sorted(payload.rglob("*")):
    if path.is_symlink():
        raise ValueError("MSI payload cannot contain symbolic links")
    relative = path.relative_to(payload)
    identity = uuid.uuid5(
        uuid.NAMESPACE_URL, "https://github.com/owayo/kanade-ime/" + relative.as_posix()
    )
    key = "P" + identity.hex
    if path.is_dir():
        parent = ET.SubElement(
            fragment, f"{{{NS}}}DirectoryRef", Id=directories[relative.parent]
        )
        ET.SubElement(parent, f"{{{NS}}}Directory", Id=key, Name=path.name)
        directories[relative] = key
    elif path.is_file():
        component = ET.SubElement(
            group,
            f"{{{NS}}}Component",
            Id=key,
            Guid=str(identity),
            Directory=directories[relative.parent],
            Bitness="always64",
        )
        ET.SubElement(
            component, f"{{{NS}}}File", Id=key + "File", Source=str(path), KeyPath="yes"
        )
    else:
        raise ValueError("unexpected MSI payload entry")
ET.indent(root)
ET.ElementTree(root).write(
    "artifacts/windows/Payload.wxs", encoding="utf-8", xml_declaration=True
)
Path("artifacts/windows/version.txt").write_text(version)
