<p align="center">
  <img src="docs/images/app.png" width="128" alt="Kanade IME">
</p>

<h1 align="center">Kanade IME</h1>

<p align="center">
  選んだ言葉を覚える、日本語入力IME
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

Kanadeは、選んで確定した言葉や訂正を覚え、次の変換候補に反映する日本語入力IMEです。ユーザー辞書、学習記録、ローカルAIを設定画面で管理できます。

現在は開発版です。通常変換と学習は実装していますが、配布版としてのネイティブ検証は続いています。[実装状況と残作業](docs/status.md)を確認してください。

## 機能

| 機能 | 動作 |
|---|---|
| かな漢字変換 | Mozcで変換し、OSの候補ウインドウで表示 |
| 選択・訂正の学習 | 明示的に選んで確定した候補や訂正を次の変換へ反映 |
| 学習の除外 | 全アプリを既定の対象とし、設定からアプリを除外 |
| 追加候補 | 絵文字、計算した日付、承認済みプロジェクトの識別子 |
| ローカルAI | Tsumugi S で文脈に応じて既存候補を並べ替え |
| 設定画面 | ユーザー辞書、学習記録、モデル、プロジェクト辞書を管理 |

入力・キー・文書の抜粋を診断ログに記録しません。パスワード入力は変換エンジンを経由せず、プライベート入力や入力先を判別できない場合は永続学習を無効にします。通常の入力内容をネットワークサービスへ送信しません。

## 動作環境

配布対象はApple SiliconのMacとWindows x64です。Windowsの32bitアプリには同梱の入力DLLを使います。Linux、Intel Mac、Windows ARM64の配布物は作りません。

ソースからのビルドには、mise、GitとOSの開発ツールが必要です。MacではXcode、Windowsでは対応するMSVC SDKとCMakeを用意します。Rust、Bazelisk、プロジェクト解析器の版はリポジトリで固定します。

## インストール

<!-- standard:install:start -->
### GitHub Releases から

[Releases](https://github.com/owayo/kanade-ime/releases/latest)から自分の環境の配布ファイルを取得し、同梱の案内に従ってインストールします。各リリースには、取得したファイルを確かめるための `SHA256SUMS` も添付しています。

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

Windows のセットアップ EXE は Tsumugi S を同梱しています。「半角／全角」でかな入力と半角英数を切り替えられます。ビルド済みのセットアップがあるチェックアウトで `install.bat` を実行できます。配布EXEをバッチの隣に置いても実行できます。

配布物が公開されるまではソースから開発用のインストールをしてください。`make install`はmacOS向けで、既存のコード署名用証明書が必要です。MacではDMG内のGUIインストーラー、WindowsではEXEセットアップを使います。公開後の導入手順と署名の確認は[配布とインストール](docs/distribution.md)にあります。

## 使い方

入力ソースにKanadeを追加し、本文の入力欄で選択してください。Macではかなキーで日本語入力へ戻り、英数キーでASCII入力に切り替えます。

| 操作 | 動作 |
|---|---|
| かなを入力してSpace | 変換候補を表示 |
| 候補の数字、上下キー、クリック | 表示中の候補を選択 |
| Enter | 選択した候補を確定 |
| F6〜F10 | ひらがな・カタカナ・英字などの文字種を切り替え |
| 入力メニューの「Kanade の設定…」 | 設定画面を表示 |

先頭以外の候補を明示的に選んで確定すると、同じ読みの次の変換で優先します。直近の選び直しを先頭にし、そのほかは選択回数で並べます。先頭をEnterでそのまま確定しても選択回数は増やしません。確定が失敗した選択やキャンセルは記録しません。

設定画面の「学習しないアプリ」から除外対象を追加します。一覧を確認して保存すると、そのアプリでの選択記録と個人候補の優先表示を止めます。除外をすべて解除すると、全アプリが再び対象になります。プライベート入力を識別できないアプリは除外してください。

「候補・訂正の記録」では学習記録を確認・削除できます。Mozcの変換学習と予測学習は個別に消去でき、未確定入力や反映待ちの編集がある間は消去を拒否します。操作は取り消せません。

追加候補と補助機能には、次の設定があります。

| 設定 | 対象 |
|---|---|
| 絵文字辞書 | 読みや短縮名に一致する絵文字 |
| 計算日付 | 「こんげつまつ」などから求めた日付 |
| プロジェクト辞書 | 明示的に承認したフォルダの識別子 |
| ローカルAI | 手動導入した対応モデルによる候補の並べ替え |

プロジェクト辞書はフォルダと対象アプリを確認してから作成します。承認前には解析しません。更新に失敗した場合も既存の辞書を保持します。

ローカルAIは初期状態では無効です。モデルの配布元と利用条件を確認し、端末内での処理に同意してから取り込みます。ファイルと応答品質の検証を通過した後に有効にできます。使用するモデルはTsumugi Sに固定しています。

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

設定アプリのコードにはMITとApache-2.0を組み合わせた条件が適用されます。Mozc、依存コード、辞書の告知は[第三者のライセンス表示](docs/third-party.md)を参照してください。モデルの重みは同梱しません。
