"""Build Kanade's offline spelling index from SudachiDict raw v1 (20261015)."""

import argparse
import csv
import hashlib
import io
import re
import zipfile
from collections import defaultdict
from pathlib import Path

ARCHIVES = {
    "small_lex": "981b78cdc532ce058b34c76cd9acd3c9a6c771cc0ce528d7de92424b21172125",
    "core_lex": "e9ac06222f0ce401a0fef9b129e61e93d59e3ff395a1b037f498962ee5b8ad8b",
}
DATA = Path(__file__).resolve().parents[1] / "crates/kanade-engine/data"
SPELLING = re.compile(r"[A-Za-z]+(?:[ '-][A-Za-z]+)*")
READING = re.compile(r"[ァ-ヶー]{2,48}")


def collect(folder):
    """Read only spelled-out nouns with a kana pronunciation from the pinned CSVs."""
    # Original casing is significant for names and abbreviations. For ordinary
    # nouns, prefer natural lowercase to a headline-capitalized corpus spelling.
    entries = defaultdict(dict)
    for name, digest in ARCHIVES.items():
        path = folder / f"{name}.zip"
        with path.open("rb") as src:
            if hashlib.file_digest(src, "sha256").hexdigest() != digest:
                raise ValueError(f"unexpected SHA-256 for {path.name}")
        with zipfile.ZipFile(path) as archive, archive.open(f"{name}.csv") as raw:
            for row in csv.reader(io.TextIOWrapper(raw, encoding="utf-8")):
                word = row[4] or row[0]
                reading = row[11]
                if not (
                    2 <= len(word) <= 64
                    and SPELLING.fullmatch(word)
                    and READING.fullmatch(reading)
                    and row[5] == "名詞"
                ):
                    continue
                if row[6] == "普通名詞" and (
                    word.istitle() or (len(word) > 4 and word[1:].isupper())
                ):
                    word = word.lower()
                cost = int(row[3])
                forms = entries[reading]
                key = word.lower()
                # Use the lowest conversion cost for homophone order. Choose
                # letter case separately: an explicit proper name or acronym
                # takes precedence over an automatically folded index form.
                case_rank = (
                    not bool(row[4]),
                    row[6] != "固有名詞",
                    word.isupper() and len(word) > 3,
                    cost,
                    word,
                )
                old_cost, old_case = forms.get(key, (cost, case_rank))
                forms[key] = (min(cost, old_cost), min(case_rank, old_case))
    return entries


def main():
    """Write a sorted row per reading with up to six distinct spellings."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive_directory", type=Path)
    entries = collect(parser.parse_args().archive_directory)
    # Preferred spellings and common kana variants missing from the raw data.
    for row in (
        (DATA / "calendar-spellings.tsv").read_text(encoding="utf-8").splitlines()
    ):
        word, *readings = row.split("\t")
        for reading in readings:
            entries[reading][word.lower()] = (-1, (False, False, False, -1, word))
    with (DATA / "english-spellings.tsv").open(
        "w", encoding="utf-8", newline="\n"
    ) as out:
        for reading, alternatives in sorted(entries.items()):
            words = [case[-1] for _, case in sorted(alternatives.values())[:6]]
            print(reading, *words, sep="\t", file=out)


if __name__ == "__main__":
    main()
