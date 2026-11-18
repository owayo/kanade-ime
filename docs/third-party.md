# Third-party notices

Kanade's original source is MIT licensed. Components retain their own licenses; MIT does not relicense bundled code or data.

| Component | Pin | License and notice |
|---|---|---|
| Mozc | 3.35.6354 | BSD 3-Clause; `third_party/mozc/LICENSE` and generated `credits_en.html` |
| hasami | `47c7dec1870d812b60cbddc67be18bcc077947a2` ([PR #145](https://github.com/owayo/hasami/pull/145)) | MIT code; embedded exception data also uses NAIST-2003, Apache-2.0 and BSD-3-Clause. Include its LICENSE, THIRD_PARTY_LICENSES.md, Apache license and builtin_exceptions.NOTICE; IME APIs are included upstream |
| Project analyzer | astro-sight `429f894188842b0ba183db766793d4aafca19e12` | MIT source and separately licensed dependencies. The macOS bundle includes `ANALYZER_LICENSES.md` with the pinned root, vendored packages and dependency notices; `project-analyzer.json` records its revision, version and signed-binary digest |
| noslop | v26.11.101 | MIT; its repository LICENSE |
| GPUI preview crates | =0.3.12 | Apache-2.0; crate license files |
| Settings text entry | GPUI =0.3.12 input example, adapted locally | Apache-2.0; `crates/kanade-ui/LICENSE-APACHE`. The native text field retains the upstream notice and adds bounded drafts, horizontal scrolling and dictionary-field presentation. The UI crate combines MIT and Apache-2.0 source; the collected GPUI notice includes the full Apache license |
| llama.cpp / Rust FFI | `locks/llm.lock.toml` and Cargo.lock | llama.cpp: MIT; llama-cpp-sys-2: MIT OR Apache-2.0. The optional worker's bundled runtime and available vendor notices are collected into CARGO_LICENSES.md |
| Rust dependencies | Cargo.lock | Individual crate license files |
| Emoji readings | `locks/emoji.lock.json` and generated manifest | emojibase-data, emoji-ja and emojilib: MIT; Unicode emoji sequences and CLDR data: Unicode License V3. Exact source notices and the pinned IPAdic reading-data notice are in `crates/kanade-emoji/data/notices` and copied into the app bundle |

The build copies Mozc's license/credits, hasami's code/data notices and collected Cargo dependency licenses into the macOS bundle. hasami's sentence exception table is embedded even without a separate dictionary file, so its data notices always accompany the binary. Before distribution, audit the collected notices for the exact release. A developer bundle is not a distribution package.

The optional IPAdic `.hsd` dictionary is acquired separately from hasami release v26.11.103. Its artifact pin in `locks/dictionary.lock.toml` is independent of the library commit; both use dictionary format v6. Preserve its IPAdic copyright and license notices with the dictionary.

The Windows installer includes Tsumugi S for local use under LicenseRef-Kanade-Model-1.0. The import dialog displays the file identity, model license identifier and local-processing terms. [Tsumugi model](llm.md#tsumugi-model)

## Windows installer

The Windows EXE includes WiX Burn from WiX 4.0.6, licensed under the Microsoft Reciprocal License (MS-RL). Its Japanese bootstrapper localization is adapted from the same version under that license. [WIX-LICENSE.txt](../packaging/windows/WIX-LICENSE.txt) contains the notice and terms and is installed with the Windows payload. WiX source: https://github.com/wixtoolset/wix4/tree/v4.0.6.
