<p align="center">
  <img src="docs/images/app.png" width="128" alt="Kanade IME">
</p>

<h1 align="center">Kanade IME</h1>

<p align="center">
  選んだ言葉を覚える日本語入力IME。ローカルAI「Tsumugi」が文脈に合う変換候補を選びます。
</p>

<!-- standard:badges:start -->
<h3 align="center">対応プラットフォーム</h3>

<p align="center">
  <img src="https://img.shields.io/badge/macOS-000000?logo=apple&amp;logoColor=white" alt="macOS">
  <img src="https://img.shields.io/badge/Windows-0078D6" alt="Windows">
</p>

<p align="center">
  <a href="https://github.com/owayo/kanade-ime/actions/workflows/ci.yml"><img src="https://github.com/owayo/kanade-ime/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI"></a>
  <a href="https://github.com/owayo/kanade-ime/releases/latest"><img src="https://img.shields.io/github/v/release/owayo/kanade-ime" alt="Release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/owayo/kanade-ime" alt="License"></a>
</p>
<!-- standard:badges:end -->

---

Kanade は Windows と macOS で使える日本語入力 IME です。「くるまにのります」と「ざっしにのります」のように同じ読みでも表記が変わるとき、同梱の言語モデル **Tsumugi（つむぎ）** が文脈をもとに候補を選びます。自分で選んで確定した言葉や訂正も、次回の変換に反映します。

現在は開発版です。通常変換と学習は実装していますが、配布版としてのネイティブ検証は続いています。[実装状況と残作業](docs/status.md)を確認してください。

## 機能

| 機能 | 動作 |
|---|---|
| かな漢字変換 | Mozcで変換し、OSの候補ウインドウで表示 |
| 選択・訂正の学習 | 明示的に選んで確定した候補や訂正を次の変換へ反映 |
| 学習の除外 | 全アプリを既定の対象とし、設定からアプリを除外 |
| 追加候補 | 絵文字、計算した日付、承認済みプロジェクトの識別子。日付候補が有効なときは「にせんきゅうねんのはる」を「2009年の春」に変換 |
| ローカルAI | Tsumugi が読みと文脈から Mozc の候補を比較し、最初の変換に反映 |
| 設定画面 | ユーザー辞書、学習記録、モデル、プロジェクト辞書を管理 |

入力・キー・文書の抜粋を診断ログに記録しません。パスワード入力は変換エンジンを経由せず、プライベート入力や入力先を判別できない場合は永続学習を無効にします。通常の入力内容をネットワークサービスへ送信しません。

### 同梱の言語モデル Tsumugi

Tsumugi は、かな漢字変換の候補に点数を付けるために学習した言語モデルです。Space を押すと、まず Mozc の候補が表示されます。ローカル AI を有効にすると、Tsumugi が読みとキャレット直前の最大40文字を手掛かりに、端末の CPU で候補を比較します。空の入力欄でも入力中の文の前半を使えるため、「くるまにのります」は「車に乗ります」、「ざっしにのります」は「雑誌に載ります」と一度の変換で書き分けられます。

Windows 用 EXE と Mac 用 DMG は、どちらも **Tsumugi 26.11.102**（約2,806万パラメータ、GGUF ファイル 66.5 MB）を同梱しています。利用するには、Kanade の設定でモデルの利用条件と端末内処理に同意し、同梱モデルを取り込んでローカル AI を有効にします。[モデルの配布と仕様](https://github.com/owayo/kanade-ime/releases/tag/tsumugi-26.11.102)・[採点の仕組み](docs/llm.md)

**Tsumugi の有効・無効で、一発変換はどれだけ変わるか**

Windows 版 Kanade 26.11.116 プレビューで、Tsumugi 26.11.102 の設定を切り替え、同じ6,600件の読みと文脈を入力しました。Space を1回押し、候補の更新が終わった時点で、表示された全文が原文またはあらかじめ認めた表記に一致すれば正解です。

| 通常変換の結果 | Tsumugi 無効 | Tsumugi 有効 |
|---|---:|---:|
| 6,600件のうち、一度で全文が正解になった件数 | 3,542件 | 3,929件 |
| 一度で全文が正解になった割合 | 53.67% | 59.53% |

Tsumugi を有効にすると477件が新たに正解し、無効時に正解だった90件が不正解になりました。差し引き **387件増、正解率は5.86ポイント上昇**しました。6,600件のうち1,320件は開発時の調整にも使っています。[測定したプレビュー版](https://github.com/owayo/kanade-ime/releases/tag/v26.11.116-preview.4)

有効時は、最初の候補が表示されてから更新が終わるまで、測定できた入力の半数が93ミリ秒以内、95%が176ミリ秒以内でした。別に、同音語の候補をあらかじめ与えてモデルの選び分けを調べたテストでは、85問中78問で期待した表記を1位に選びました。[モデル単体のテスト条件](docs/llm.md#tsumugi-model)

## 動作環境

配布対象はApple SiliconのMacとWindows x64です。Windowsの32bitアプリには同梱の入力DLLを使います。Linux、Intel Mac、Windows ARM64の配布物は作りません。

ソースからのビルドには、mise、GitとOSの開発ツールが必要です。MacではXcode、Windowsでは対応するMSVC SDKとCMakeを用意します。Rust、Bazelisk、プロジェクト解析器の版はリポジトリで固定します。

## インストール

<!-- standard:install:start -->
### GitHub Releases から

プレビュー版を試す場合は、タグに `-preview.` が付いたリリースを開きます。Windows x64 の EXE は未署名、Apple Silicon Mac の DMG は一時署名・未公証です。どちらも Tsumugi を1つ同梱しています。インストーラーと同じリリースの `SHA256SUMS` でファイルを照合できます。[プレビュー版の一覧](https://github.com/owayo/kanade-ime/releases?q=preview)・[正式版](https://github.com/owayo/kanade-ime/releases/latest)

| プラットフォーム | ファイル |
|---|---|
| macOS (Apple Silicon) | `kanade-ime-aarch64-apple-darwin.dmg` |
| Windows (x86_64) | `kanade-ime-x86_64-pc-windows-msvc.exe` |

### ソースから

[mise](https://mise.jdx.dev/) が必要です (Rust のツールチェーンは `mise.toml` で固定しています)。

```bash
git clone https://github.com/owayo/kanade-ime.git
cd kanade-ime
make install
```

`make install` は `INSTALL_DIR` にアプリを入れます (既定の場所は `Makefile` にあります)。場所を変えるときは `INSTALL_DIR` を指定します。
<!-- standard:install:end -->

Windows のセットアップ EXE と macOS の DMG は Tsumugi を同梱しています。Windows では「半角／全角」でかな入力と半角英数を切り替えられます。ビルド済みのセットアップがあるチェックアウトで `install.bat` を実行できます。配布EXEをバッチの隣に置いても実行できます。

配布物が公開されるまではソースから開発用のインストールをしてください。`make install`はmacOS向けで、既存のコード署名用証明書が必要です。MacではDMG内のGUIインストーラー、WindowsではEXEセットアップを使います。公開後の導入手順と署名の確認は[配布とインストール](docs/distribution.md)にあります。

## 使い方

入力ソースにKanadeを追加し、本文の入力欄で選択してください。Macではかなキーで日本語入力へ戻り、英数キーでASCII入力に切り替えます。

| 操作 | 動作 |
|---|---|
| 半角／全角（Windows） | キャレットのそばに「A 半角英数」または「あ ひらがな」を約1秒表示 |
| かなを入力してSpace | 変換候補を表示 |
| 候補の数字、上下キー、クリック | 表示中の候補を選択 |
| Enter | 選択した候補を確定 |
| F6〜F10 | ひらがな・カタカナ・英字などの文字種を切り替え |
| 入力メニューの「Kanade の設定…」 | 設定画面を表示 |

候補欄の下部に「AI 評価中…」と出ている間も、通常どおり候補を選んで確定できます。Tsumugi の採点が終わると、候補の順序が同じでも「AI 評価済み」と表示します。「AI 評価できず」や「AI 時間切れ」が出たときは、そのまま通常候補を使えます。

先頭以外の候補を明示的に選んで確定すると、同じ読みの次の変換で優先します。直近の選び直しを先頭にし、そのほかは選択回数で並べます。先頭をEnterでそのまま確定しても選択回数は増やしません。確定が失敗した選択やキャンセルは記録しません。

設定画面の「学習しないアプリ」から除外対象を追加します。一覧を確認して保存すると、そのアプリでの選択記録と個人候補の優先表示を止めます。除外をすべて解除すると、全アプリが再び対象になります。プライベート入力を識別できないアプリは除外してください。

「候補・訂正の記録」では学習記録を確認・削除できます。Mozcの変換学習と予測学習は個別に消去でき、未確定入力や反映待ちの編集がある間は消去を拒否します。操作は取り消せません。

追加候補と補助機能には、次の設定があります。

| 設定 | 対象 |
|---|---|
| 絵文字辞書 | 読みや短縮名に一致する絵文字 |
| 計算日付 | 「こんげつまつ」などから求めた日付 |
| プロジェクト辞書 | 明示的に承認したフォルダの識別子 |
| ローカルAI | 同梱の Tsumugi による候補の並べ替え |

プロジェクト辞書はフォルダと対象アプリを確認してから作成します。承認前には解析しません。更新に失敗した場合も既存の辞書を保持します。

ローカルAIは初期状態では無効です。モデルの配布元と利用条件を確認し、端末内での処理に同意してから取り込みます。ファイルと応答品質の検証を通過した後に有効にできます。使用するモデルはTsumugiに固定しています。

CLIの操作例とログの場所は[操作ガイド](docs/usage.md)を参照してください。計算日付や絵文字、プロジェクト辞書の動作も説明しています。ユーザー辞書の編集は[辞書の管理](docs/dictionary-management.md)、モデルのビルドと検証は[ローカルAI](docs/llm.md)にまとめています。

## 設定

普段の変更は入力メニューから開く設定画面で行います。モデルの同意とプロジェクトの承認は、対象を確認する操作を経て保存します。ライブ変換、自動の候補生成、モデルの自動取得は未提供です。

Macのデータは`~/Library/Application Support/kanade-ime`、Windowsのデータは`%LOCALAPPDATA%\kanade-ime`に保存します。開発や検証では`KANADE_DATA_DIR`でデータディレクトリを指定できます。既存の個人データを使わない検証には、別のディレクトリを指定してください。

署名済みアプリの管理には、同じアプリに同梱したCLIを使います。開発用に別途ビルドしたCLIからの管理要求は、接続先の認証で拒否される場合があります。

## 開発

<!-- standard:dev:start -->
[mise](https://mise.jdx.dev/) が必要です。ツールの版は `mise.toml` で固定しています。

```bash
make setup   # ツールチェーン (mise) と依存を取得する
make ci      # CI と同じ検査 (書き換えない)
```

| コマンド | 説明 |
|---|---|
| `make setup` | ツールチェーン (mise) と依存を取得する |
| `make build` | デバッグ版をビルドする |
| `make release` | リリース版をビルドする |
| `make bundle` | リリース版をアプリにまとめる |
| `make dev` | デバッグ版のアプリを起動する |
| `make run` | リリース版のアプリをインストールして起動する |
| `make test` | テストを実行する |
| `make lint` | clippy を警告ゼロで通す |
| `make fmt` | コードを整形する (書き換える) |
| `make fmt-check` | 整形済みかを確かめる (書き換えない) |
| `make check` | 整形と静的検査 (書き換えない) |
| `make ci` | CI と同じ検査 (書き換えない) |
| `make install` | リリース版のアプリを INSTALL_DIR に入れる |
| `make uninstall` | INSTALL_DIR からアプリを取り除く |
| `make clean` | ビルド成果物を消す |

`make` でターゲットの一覧を表示します。リリースは GitHub Actions で行います (**Actions → Release → Run workflow**)。
<!-- standard:dev:end -->

入力と候補表示はmacOSのInputMethodKitとWindowsのTSFで処理します。変換には別プロセスで動く無改造のMozc、設定画面にはGPUIを使います。

Cargoの通常のビルドではBazelを起動しません。Mozcワーカーは`make worker`で明示的にビルドします。`make contract`は実ワーカーの変換・確定・復旧を、`make project-contract`はプロジェクト辞書の索引と通知を検証します。

`make version`で版の整合性を確認し、`make bump-version`で日本時間の年月とカウンターに基づいて一括更新します。通常のビルドでは版を変更しません。

| 文書 | 内容 |
|---|---|
| [アーキテクチャ](docs/architecture.md) | 入力処理、文書への反映、学習、別プロセスとの境界 |
| [ワーカープロトコル](docs/worker-protocol.md) | Mozcアダプターの契約 |
| [実装状況](docs/status.md) | 測定結果と未検証の項目 |
| [操作ガイド](docs/usage.md) | 個別の操作例と開発用コマンド |
| [配布とインストール](docs/distribution.md) | 対象OS、署名、インストーラーの検証 |

## ライセンス

<!-- standard:license:start -->
[MIT](LICENSE)
<!-- standard:license:end -->

設定アプリのコードにはMITとApache-2.0を組み合わせた条件が適用されます。Mozc、依存コード、辞書の告知は[第三者のライセンス表示](docs/third-party.md)を参照してください。同梱の Tsumugi には別途モデルの利用条件が適用されます。
