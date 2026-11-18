# SPDX-License-Identifier: MIT
"""Regenerate independent Tsumugi token IDs from its exported tokenizer.json.

    python tokenizer-parity.py TOKENIZER_JSON RANKING_JSONL OUTPUT_JSON
"""
import hashlib
import json
import sys
from pathlib import Path

from tokenizers import Tokenizer, __version__

source, ranking, output = map(Path, sys.argv[1:4])
tokenizer = Tokenizer.from_file(str(source))

# Synthetic edge cases: scripts, widths, combining marks, control characters,
# literal special-token spellings, prompt tags, astral characters and symbols.
cases = [
    "にほん", "ニホン", "日本", "日本語入力", "東京に行きます。", "ＡＢＣ１２３", "ABC 123",
    "a\nb\tc", "全角　空白", "が", "が", "ｶﾞ", "😀🍣", "<s>", "</s>", "[UNK]", "[PAD]",
    "ニホン", "今日はニホン", "文脈\r\n改行", "記号!?()[]{}",
    "々〆ヶゔヴー", "𠮷野家", "a_b-c/d@example.com", "ー", "ァヺ", "１２３、４５６。",
    "「引用」『二重』（括弧）", "Ａ’ｓ　ｂ", "can't won't it's", "tab\tend", "末尾空白 ",
    " 先頭空白", "二重  空白", "　　", "ｱｲｳｴｵ", "髙﨑", "①②③", "㈱", "ゟ", "〜～", "…‥",
    "", "右文脈", "A", "あ", "龯", " ", "é", "é",
]
seen = set(cases)


def add(text):
    if text and text not in seen:
        seen.add(text)
        cases.append(text)


def katakana(text):
    return "".join(chr(ord(c) + 0x60) if "ぁ" <= c <= "ゖ" else c for c in text)


# Every text the ranking fixture sends, in the exact form the worker tokenizes.
for line in ranking.read_text(encoding="utf-8").splitlines():
    case = json.loads(line)
    reading = katakana(case["reading"])
    add("" + case["context"] + "" + reading + "")
    add(case["prefix"])
    for candidate in case["candidates"]:
        add(candidate + case["suffix"])

digest = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
result = {
    "format": 1,
    "source": {"model":"Tsumugi S", "tokenizer_sha256": digest(source)},
    "reference": {"library":"tokenizers","version":__version__,"add_special_tokens":False},
}
# One case per line keeps the file reviewable.
lines = [
    json.dumps(
        {"text": text, "tokens": tokenizer.encode(text, add_special_tokens=False).ids},
        ensure_ascii=False,
        separators=(",", ":"),
    )
    for text in cases
]
head = json.dumps(result, ensure_ascii=False, indent=1)[:-2]
output.write_text(
    head + ',\n "cases": [\n  ' + ",\n  ".join(lines) + "\n ]\n}\n", encoding="utf-8"
)
print("cases", len(cases), "sha256", digest(output))
