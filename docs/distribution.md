# 配布とインストール

配布先はGitHub Releasesです。Apple Silicon Mac向けには正式版の公証済みDMGとプレビュー版の未公証PKGを配布します。Windows x64向けはどちらもEXEで、32bitアプリ向け入力DLLを含むMSIを内包します。Linux、Intel Mac、Windows ARM64のインストーラーは作りません。

検証状況は[実装状況](https://github.com/owayo/kanade-ime/blob/main/docs/status.md)を確認してください。署名・公証とネイティブ入力の検証は、コンパイルとは別に行います。

## ダウンロード後の確認

インストーラーと同じReleaseの`SHA256SUMS`を取得し、SHA-256を照合してから開いてください。ファイル名は版を含みません。ダウンロードするReleaseのタグで版を選びます。

| 対象 | ファイル | 内容 |
|---|---|---|
| Apple Silicon Mac（正式版） | `kanade-ime-aarch64-apple-darwin.dmg` | 日本語のGUIインストーラー、Kanade本体、Tsumugi |
| Apple Silicon Mac（プレビュー版） | `kanade-ime-aarch64-apple-darwin.pkg` | macOS標準インストーラーで導入するKanade本体、Tsumugi |
| Windows x64 | `kanade-ime-x86_64-pc-windows-msvc.exe` | 日本語のセットアップ画面、Tsumugi を含むMSI |

同じ Release にある `tsumugi.gguf` は、インストーラーが同梱しているモデルの写しです。インストーラーのビルドが取得するために添付しています。利用するときにダウンロードする必要はありません。

Windows の開発用チェックアウトでは、`install.bat` を実行すると、現在の HEAD のインストーラーを作って起動します。同じ HEAD で再実行すると、`artifacts/windows/install-head/<コミットID>/` に保存した EXE の SHA-256 を照合して再利用します。未コミットの変更がある場合は、一時フォルダーに HEAD をチェックアウトしてビルドします。ビルドにはプロジェクトの開発ツール、Windows SDK、WiX を実行するための .NET SDK が必要です。[Windows のパッケージ作成](../packaging/windows/README.md)

配布 EXE を使う場合は、チェックアウトの外で `install.bat` と EXE を同じフォルダーに置いて実行できます。Windows では Tsumugi をインストール先の `models/` に、macOS では Kanade 内の設定アプリに配置します。利用条件に同意して有効にする手順は[利用方法](usage.md)にあります。同梱モデルが替わらない版に更新したときは、並べ替えを有効にしていれば、Kanade がモデルを検証し直します。`install.bat` はその検証が終わるのを待ちます。同梱モデルが替わった版に更新したときは、モデルを取り込み直すまで候補の並べ替えが働きません。その手順も同じページにあります。

## macOS

### 正式版のDMG

1. 入力中の文章を確定し、DMGを開きます。
2. 「Kanade インストーラー」をダブルクリックし、「インストール」を選びます。
3. 「システム設定」の「キーボード」からKanadeを入力ソースに追加します。表示されない場合はログインし直してください。
4. かなキー、または入力メニューからKanadeを選びます。

管理者権限は不要です。DMG内のインストーラーは、現在のユーザーの`~/Library/Input Methods/Kanade.app`へ本体を配置し、ユーザー用のLaunchAgentを登録します。

更新時は既存のASCII入力ソースへ切り替え、既存のフロントエンドを終了してからアプリを置き換えます。正常終了後もASCII入力を維持します。設定、学習データ、モデルの同意、プロジェクト承認は更新しません。

削除する場合は、同じDMGのインストーラーで「アンインストール」を選びます。個人設定と学習データは保持します。

正式版はDeveloper IDで署名し、Appleの公証を受けます。本体とインストーラーに公証チケットを付けてからDMGを作り、DMGも署名・公証してチケットを付けます。

ソースからの開発用インストールは[開発・操作の詳細](https://github.com/owayo/kanade-ime/blob/main/docs/usage.md)を参照してください。ローカル開発の証明書と配布用のDeveloper IDは用途が異なります。

### macOS プレビュー版のPKG

PKGは未署名・未公証です。Apple SiliconのmacOS 14以降で、現在のユーザーにインストールします。

1. ダウンロード先で `shasum -a 256 kanade-ime-aarch64-apple-darwin.pkg` を実行し、同じリリースの `SHA256SUMS` にあるPKGの行と照合します。一致したら入力中の文章を確定し、PKGを開きます。
2. macOSがPKGを止めた場合は、システム設定の「プライバシーとセキュリティ」で、そのPKGの「このまま開く」を選びます。[Appleの案内](https://support.apple.com/ja-jp/102445)
3. macOS標準のインストーラーで「インストール」を選びます。入力ソースにKanadeが表示されないときは、ログアウト・再ログインしてから「キーボード」の設定で追加します。

削除するには、入力中の文章を確定してから、ターミナルで次を実行します。個人設定と学習データは保持します。

```sh
"$HOME/Library/Input Methods/Kanade.app/Contents/MacOS/kanade-installer" install-macos --uninstall
```

## Windows

Windows にサインインすると、タスク スケジューラの「Kanade IME」が、サインインしたユーザーの通常権限で変換エンジンの準備を始めます。ログオン トリガーに遅延は設定していません。バッテリー駆動中も起動し、複数のユーザーがサインインした場合はセッションごとに起動します。

タスクは `kanade-engine.exe --supervise` を直接実行します。使用中にエンジンが終了すると監視プロセスが再起動します。短時間に終了が続いた場合は、次の起動までの間隔を最大30秒まで延ばします。終了コードと再起動の回数は `%LOCALAPPDATA%\kanade-ime\logs\engine-events.log` に残ります。読み方は[利用方法](usage.md)にあります。

Microsoft Visual C++ Redistributableのx64版とx86版が必要です。[Microsoftの配布ページ](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist)から最新の対応版を導入してください。ランタイムの登録がなければ、MSIは入力DLLを登録せずに停止します。Microsoftのランタイムは再配布しません。

1. 入力を確定してKanadeを終了し、ダウンロードしたEXEをダブルクリックします。
2. ライセンスを確認し、セットアップ画面の「インストール」を選びます。管理者権限が必要です。
3. 完了後にサインアウトし、再びサインインします。通常権限のユーザーセッションでエンジンが起動します。
4. Windowsの言語設定でKanadeを有効にし、入力方式を選びます。完了画面の「設定方法を開く」から、この手順を開けます。

設定画面は、スタートメニューの「Kanade の設定」から開けます。Kanadeを選んでいる間は、タスクバーの「あ」または「A」を右クリックしても開けます。

EXEはWiX Burnのセットアップです。MSIはProgram Files配下にエンジンと設定アプリを隣接して配置し、64bit・32bitそれぞれの入力DLLを登録します。スタートメニューには、設定アプリのショートカットを全ユーザー向けに置きます。更新では、新版が引き継ぐファイルを先にインストールし、同じトランザクションで旧版を削除してから入力DLLを登録します。失敗時には登録を取り消します。更新時に開いていたアプリでは、次に開くまで以前の入力DLLが使われることがあります。サイレント更新が終了コード3010を返した場合は、作業を保存し、都合のよいときにWindowsを再起動してください。

削除はWindowsの「インストールされているアプリ」、またはEXEの「アンインストール」から行います。入力DLLの登録、自動起動設定、スタートメニューのショートカットを取り除き、個人設定と学習データは保持します。

## メンテナー向けの流れ

インストーラーはKanade本体を内部に持ち、macOSによるアプリの一時移動にも対応します。入力ソースの切り替えとアプリの置き換えは、既存の共通ヘルパーへ委譲します。ペイロードは実行前に署名を検証する構成です。

`Release` の手動実行で `channel` を選びます。既定の `stable` は署名・公証した正式版を配布します。`dry_run` では正式版の次の版を計算します。

`preview` の公開対象は、既定ブランチの同じコミットでCIに合格したWindows x64向け未署名EXEと、Apple Silicon Mac向け未署名・未公証PKGです。どちらもTsumugiを1個同梱します。CIのmacOSランナーでは、CI用証明書への信頼を削除してからPKGをインストールし、アプリの署名、同梱モデル、エンジン接続を確認します。`dry_run` は、両インストーラーとモデルのチェックサム照合までです。

Release 本文には、モデルの版、バイト数、SHA-256と、更新時にモデルを取り込み直す手順が載ります。直前の同じチャネルの版からモデルが替わった場合は、その版のタグも記載します。プレビューの本文はインストーラーを検査したCIにリンクします。本文は `.github/scripts/release_notes.py` が組み立てます。

Tsumugi はインストーラーに同梱する形で配布します。モデルだけの Release は作りません。各 Release には、インストーラーが同梱しているモデルの写しを `tsumugi.gguf` として添付します。`locks/llm.lock.toml` が記録するのはモデルの版・バイト数・SHA-256 で、URL は持ちません。インストーラーを作るときは、`.github/scripts/bundled_model.py` が Release の添付ファイルから SHA-256 の一致するものを探して取得し、ロックと照合してから同梱します。

モデルを更新するときは、`crates/kanade-llm/src/profile.rs` の登録と `locks/llm.lock.toml` を同じコミットで書き換えます。そのモデルを同梱した Release はまだないので、次のコマンドで GGUF を最新の Release に添付します。添付できるのはロックと一致するファイルだけで、ファイル名は `tsumugi-<モデルの版>.gguf` になります。

```sh
python3 .github/scripts/bundled_model.py --attach <GGUF>
```

モデルを添付すると、重みが公開されます。取り消せないので、添付するのは同梱すると決めたモデルだけにします。Release のページでは、この添付は「次の版に同梱するモデル」と表示されます。添付した直後は、API の一覧に出るまで数秒かかることがあります。次の Release を公開すると、モデルは `tsumugi.gguf` として新しい Release に付きます。その本文には、直前の版と比べて同梱モデルが替わったことが入ります。前の Release に添付した写しは、公開のジョブが削除します。手で削除するときは次のコマンドを使います。

```sh
gh release delete-asset <前の Release のタグ> tsumugi-<モデルの版>.gguf
```

本番実行には、既定ブランチの対象コミットでCIが成功していることと、配布用の署名・公証情報が必要です。公開前に[ネイティブ検証項目](https://github.com/owayo/kanade-ime/blob/main/docs/status.md#native-release-gates)も確認してください。署名情報がなければ採番やタグ作成より前に止まります。情報の名前はworkflowの事前検査と署名ステップで確認できます。

採番は`cargo xtask bump-version`を使います。日本時間の`yy.m.counter`で、ワークスペースと両方のmacOSアプリを一括更新します。インストーラーの版は本体から生成します。同じタグがある場合は止めます。ブランチとタグは`git push --atomic`で一度に送信します。

ビルドではキャッシュを使いません。macOSではGUIインストーラーをテストします。DMG本体に加え、マウント後の署名・チェックサム・事前検査も検証対象です。インストーラー内部の`KanadeInstaller --dry-run`は導入前の検査だけを実行し、`--uninstall-dry-run`は削除前の検査だけを実行します。

WindowsはMSIの登録失敗の取り消しと、EXE経由の新規インストール・削除を使い捨てのCIランナーで確認します。Burnの内部エンジン、MSIと各実行ファイルを署名した後、EXE全体を署名します。実際のアプリでのキー入力とAppContainer対応には、別途ネイティブ検証が必要です。

DMGとEXEは変形せずジョブ間で受け渡します。公開前に、インストーラーと同梱モデルの対象名・件数・通常ファイルであることを確認し、`SHA256SUMS`を作成します。公開後の照合対象は、名前、サイズ、状態とGitHubが返すSHA-256です。公開したインストーラーと`SHA256SUMS`は差し替えません。Release に後から足すのは、次の版で同梱するモデルの写しだけです。

Homebrewとwingetの更新ジョブはありません。
