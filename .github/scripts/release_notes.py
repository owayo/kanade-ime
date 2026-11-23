"""Write the text of a release: what it bundles and what an update needs."""

import argparse
from pathlib import Path

from bundled_model import INSTALLERS, locked_model, releases

# The engine stops using an imported model that is not the bundled one. The
# candidate window then asks for the settings, and the settings say what to do.
# The steps are right whichever version was installed.
STEPS = (
    "以前の版で Tsumugi を取り込んでいた場合は、更新後に Kanade の設定の"
    "「ローカルAI」を開いてください。「モデルが変更されています」と"
    "表示されていたら、利用条件と端末内処理に同意して"
    "「Tsumugi を取り込む」を押します。"
    "取り込み直すまで、候補の並べ替えは働きません。"
    "並べ替えを有効にしていた場合は、その間、候補欄に"
    "「AI は設定が必要」と出ます。"
)


def compared(listing: list, tag: str, prerelease: bool) -> dict | None:
    """Pick the release that people on this channel most likely update from.

    Returns:
        The latest other release of the channel that carries both installers.

    """
    earlier = [
        release
        for release in listing
        if release["tag_name"] != tag
        and release["prerelease"] == prerelease
        and set(INSTALLERS) <= {asset["name"] for asset in release["assets"]}
    ]
    return max(earlier, key=lambda release: release["published_at"], default=None)


def replaced(model: dict, release: dict | None) -> bool:
    """Tell whether a release is known to bundle another model than the locked one.

    Returns:
        False also when the release does not say which model it bundles.

    """
    if release is None:
        return False
    digests = {asset["name"]: asset.get("digest") for asset in release["assets"]}
    known = digests.get(model["file"])
    return known is not None and known != "sha256:" + model["sha256"]


def update_notice(model: dict, previous: dict | None) -> str:
    """Tell people who update what the bundled model needs from them.

    Returns:
        The steps, led by the change when the compared release shows one.

    """
    if not replaced(model, previous):
        return STEPS
    return (
        f"`{previous['tag_name']}` と比べて、同梱モデルが "
        f"Tsumugi {model['version']} に替わりました。{STEPS}"
    )


def bundled(model: dict) -> str:
    """Name the model that both installers bundle.

    Returns:
        One sentence with the version and the byte count.

    """
    size = f"{model['size']:,}"
    return f"どちらにも Tsumugi {model['version']}（{size} バイト）を同梱しています。"


def preview(version: str, model: dict, previous: dict | None, ci: str) -> str:
    """Compose the text of a preview release.

    Returns:
        Markdown paragraphs.

    """
    opening = (
        f"Kanade {version} のプレビュー版です。Windows x64 の EXE は未署名、"
        "Apple Silicon Mac の DMG は一時署名・未公証です。macOS で警告が出た場合は、"
        "SHA256SUMS で照合してからシステム設定で開くことを許可します。"
        "[macOS プレビュー版を開く手順]"
        "(https://github.com/owayo/kanade-ime/blob/main/docs/distribution.md#macos-プレビュー版を開く)"
    )
    notice = update_notice(model, previous)
    checksum = (
        f"モデルの SHA-256 は `{model['sha256']}` です。"
        "インストーラーは添付の SHA256SUMS で照合できます。"
    )
    attached = (
        f"`{model['file']}` は同梱しているモデルの写しで、"
        "インストーラーのビルドが取得するために添付しています。"
        "利用するときにダウンロードする必要はありません。"
    )
    tests = f"[両 OS のインストーラー検査]({ci})"
    paragraphs = [opening, bundled(model), notice, checksum, attached, tests]
    return "\n\n".join(paragraphs) + "\n"


def stable(model: dict, previous: dict | None) -> str:
    """Compose the text that precedes the generated notes of a stable release.

    Returns:
        Markdown paragraphs.

    """
    guidance = (
        "Windows x64とApple Silicon Mac向けの日本語入力IMEです。\n"
        "SHA256SUMSで照合してから、インストーラーを開いてください。\n"
        "macOSはDMG内の「Kanade インストーラー.app」、WindowsはEXEを起動します。\n"
        "導入手順とネイティブ検証の状況も確認してください。"
    )
    attached = (
        f"`{model['file']}` は同梱しているモデルの写しです。"
        "インストーラーのビルドが取得するために添付しています。"
    )
    paragraphs = [guidance, bundled(model), update_notice(model, previous), attached]
    return "\n\n".join(paragraphs) + "\n"


def previous_release(repository: str, tag: str, prerelease: bool) -> dict | None:
    """Find the release to compare with, without holding up the publication.

    Returns:
        None also when the releases cannot be listed: the steps hold without it.

    """
    try:
        listing = list(releases(repository))
    except (ValueError, OSError) as error:
        print(f"::warning::The notes omit the model comparison: {error}")
        return None
    return compared(listing, tag, prerelease)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("channel", choices=["preview", "stable"])
    parser.add_argument("--tag", required=True)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--version", help="the workspace version of a preview")
    parser.add_argument("--ci", help="the installer tests of a preview")
    args = parser.parse_args()
    model = locked_model()
    if args.channel == "stable":
        text = stable(model, previous_release(args.repo, args.tag, False))
    elif not args.version or not args.ci:
        parser.error("preview requires --version and --ci")
    else:
        previous = previous_release(args.repo, args.tag, True)
        text = preview(args.version, model, previous, args.ci)
    args.output.write_text(text, encoding="utf-8")
