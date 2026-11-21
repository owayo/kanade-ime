# Third-party notices

Kanade's original source is MIT licensed. Components retain their own licenses; MIT does not relicense bundled code or data.

| Component | Pin | License and notice |
|---|---|---|
| Mozc | 3.35.6354 | BSD 3-Clause; `third_party/mozc/LICENSE` and generated `credits_en.html` |
| hasami | [v26.11.106](https://github.com/owayo/hasami/releases/tag/v26.11.106), pinned in Cargo.lock | MIT code; embedded exception data also uses NAIST-2003, Apache-2.0 and BSD-3-Clause. Include its LICENSE, THIRD_PARTY_LICENSES.md, Apache license and builtin_exceptions.NOTICE; IME APIs are included upstream |
| Project analyzer | astro-sight `429f894188842b0ba183db766793d4aafca19e12` | MIT source and separately licensed dependencies. The macOS bundle includes `ANALYZER_LICENSES.md` with the pinned root, vendored packages and dependency notices; `project-analyzer.json` records its revision, version and signed-binary digest |
| noslop | v26.11.101 | MIT; its repository LICENSE |
| GPUI preview crates | =0.3.12 | Apache-2.0; crate license files |
| Settings text entry | GPUI =0.3.12 input example, adapted locally | Apache-2.0; `crates/kanade-ui/LICENSE-APACHE`. The native text field retains the upstream notice and adds bounded drafts, horizontal scrolling and dictionary-field presentation. The UI crate combines MIT and Apache-2.0 source; the collected GPUI notice includes the full Apache license |
| llama.cpp / Rust FFI | `locks/llm.lock.toml` and Cargo.lock | llama.cpp: MIT; llama-cpp-sys-2: MIT OR Apache-2.0. The optional worker's bundled runtime and available vendor notices are collected into CARGO_LICENSES.md |
| Rust dependencies | Cargo.lock | Individual crate license files |
| Kana to English spellings | SudachiDict raw v1, 20261015 | Copyright (c) 2017–2023 Works Applications Co., Ltd. Kanade extracts readings and Latin spellings, ranks homophones and adds calendar variants. Both installers include `SudachiDict-LEGAL.txt` and the Apache License 2.0 in `LICENSE-APACHE-2.0.hasami`. [Upstream notices](https://github.com/WorksApplications/SudachiDict/blob/v20261015/LEGAL) |
| Emoji readings | `locks/emoji.lock.json` and generated manifest | emojibase-data, emoji-ja and emojilib: MIT; Unicode emoji sequences and CLDR data: Unicode License V3. Exact source notices and the pinned IPAdic reading-data notice are in `crates/kanade-emoji/data/notices` and copied into the app bundle |

The build copies Mozc's license/credits, hasami's code/data notices and collected Cargo dependency licenses into the macOS bundle. hasami's sentence exception table is embedded even without a separate dictionary file, so its data notices always accompany the binary. Before distribution, audit the collected notices for the exact release. A developer bundle is not a distribution package.

The optional IPAdic `.hsd` dictionary is acquired separately from hasami release v26.11.103. Its artifact pin in `locks/dictionary.lock.toml` is independent of the library commit; both use dictionary format v6. Preserve its IPAdic copyright and license notices with the dictionary.

The Windows and macOS installers include Tsumugi for local use under LicenseRef-Kanade-Model-1.0. The import dialog displays the file identity, model license identifier and local-processing terms. [Tsumugi model](llm.md#tsumugi-model) · [Training data](#tsumugi-training-data)

To regenerate the spelling index, save both pinned archives in the same directory and run `python tools/english_spellings.py <archive-directory>` with Python 3.11 or later. The generator checks both archive digests before writing the index. [Small lexicon](https://sudachi.s3.ap-northeast-1.amazonaws.com/sudachidict-raw/v1/20261015/small_lex.zip) · [Core lexicon](https://sudachi.s3.ap-northeast-1.amazonaws.com/sudachidict-raw/v1/20261015/core_lex.zip)

## Windows installer

The Windows EXE includes WiX Burn from WiX 4.0.6, licensed under the Microsoft Reciprocal License (MS-RL). Its Japanese bootstrapper localization is adapted from the same version under that license. [WIX-LICENSE.txt](../packaging/windows/WIX-LICENSE.txt) contains the notice and terms and is installed with the Windows payload. WiX source: https://github.com/wixtoolset/wix4/tree/v4.0.6.

## Tsumugi training data

The installers contain the Tsumugi weights and none of the text the weights were trained on. The training text came from these sources. Readings were added to it, and passages were cut into conversion examples.

| Source | Credit and terms |
|---|---|
| Japanese Wikipedia | Text by the contributors of Japanese Wikipedia, licensed under [CC BY-SA 4.0](https://creativecommons.org/licenses/by-sa/4.0/). Tsumugi 26.11.105 took its examples from 1,260,029 articles of the `20231101.ja` snapshot in the [wikimedia/wikipedia dataset](https://huggingface.co/datasets/wikimedia/wikipedia). The articles and sentences used for measurement were left out. An article and its authors are found at `https://ja.wikipedia.org/?curid=<page ID>` |
| Aozora Bunko | Works whose term of copyright has ended, entered and proofread by the volunteers of [Aozora Bunko](https://www.aozora.gr.jp/) |
| e-Gov 法令検索 | Statutes published by [e-Gov 法令検索](https://laws.e-gov.go.jp/) |
| Generated sentences | Sentences written for training with language models |

hasami and its dictionaries supplied the readings, and the Mozc dictionary supplied the candidate spellings that the weights learned to compare. Both are listed above with their notices.
