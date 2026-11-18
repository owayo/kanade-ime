# 配布とインストール

配布先はGitHub Releasesです。Apple Silicon Mac向けにはDMG、Windows x64向けにはEXEインストーラーを作ります。WindowsのEXEは、32bitアプリ向けの入力DLLも含むMSIを内包します。Linux、Intel Mac、Windows ARM64のインストーラーは作りません。

検証状況は[実装状況](https://github.com/owayo/kanade-ime/blob/main/docs/status.md)を確認してください。署名・公証とネイティブ入力の検証は、コンパイルとは別に行います。

## ダウンロード後の確認

インストーラーと同じReleaseの`SHA256SUMS`を取得し、SHA-256を照合してから開いてください。ファイル名は版を含みません。ダウンロードするReleaseのタグで版を選びます。

| 対象 | ファイル | 内容 |
|---|---|---|
| Apple Silicon Mac | `kanade-ime-aarch64-apple-darwin.dmg` | 日本語のGUIインストーラー、Kanade本体、Tsumugi |
| Windows x64 | `kanade-ime-x86_64-pc-windows-msvc.exe` | 日本語のセットアップ画面、Tsumugi を含むMSI |

Windowsでは `install.bat` が `artifacts/release/` のEXEを起動します。EXEをバッチの隣に置いても実行できます。Windows では Tsumugi をインストール先の `models/` に、macOS では Kanade 内の設定アプリに配置します。利用条件に同意して有効にする手順は[利用方法](usage.md)にあります。

## macOS

1. 入力中の文章を確定し、DMGを開きます。
2. 「Kanade インストーラー」をダブルクリックし、「インストール」を選びます。
3. 「システム設定」の「キーボード」からKanadeを入力ソースに追加します。表示されない場合はログインし直してください。
4. かなキー、または入力メニューからKanadeを選びます。

管理者権限は不要です。DMG内のインストーラーは、現在のユーザーの`~/Library/Input Methods/Kanade.app`へ本体を配置し、ユーザー用のLaunchAgentを登録します。

更新時は既存のASCII入力ソースへ切り替え、既存のフロントエンドを終了してからアプリを置き換えます。正常終了後もASCII入力を維持します。設定、学習記録、モデルの同意、プロジェクト承認は更新しません。

削除する場合は、同じDMGのインストーラーで「アンインストール」を選びます。個人設定と学習データは保持します。

配布ではDeveloper IDによる署名と公証を要求します。本体とインストーラーに公証チケットを付けてからDMGを作り、DMGも署名・公証してチケットを付けます。隔離属性を削除する操作は不要です。

ソースからの開発用インストールは[開発・操作の詳細](https://github.com/owayo/kanade-ime/blob/main/docs/usage.md)を参照してください。ローカル開発の証明書と配布用のDeveloper IDは用途が異なります。

## Windows

Microsoft Visual C++ Redistributableのx64版とx86版が必要です。[Microsoftの配布ページ](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist)から最新の対応版を導入してください。ランタイムの登録がなければ、MSIは入力DLLを登録せずに停止します。Microsoftのランタイムは再配布しません。

1. 入力を確定してKanadeを終了し、ダウンロードしたEXEをダブルクリックします。
2. ライセンスを確認し、セットアップ画面の「インストール」を選びます。管理者権限が必要です。
3. 完了後にサインアウトし、再びサインインします。通常権限のユーザーセッションでエンジンが起動します。
4. Windowsの言語設定でKanadeを有効にし、入力方式を選びます。完了画面の「設定方法を開く」から、この手順を開けます。

EXEはWiX Burnのセットアップです。MSIはProgram Files配下にエンジンと設定アプリを隣接して配置し、64bit・32bitそれぞれの入力DLLを登録します。失敗時には登録を取り消します。更新はWindows Installerのトランザクション内で旧版を置き換えます。更新前から開いていたアプリは、読み込み済みの入力 DLL を使い続ける場合があります。入力中の文章を保存し、常駐アプリは通知領域からも終了して開き直してください。

削除はWindowsの「インストールされているアプリ」、またはEXEの「アンインストール」から行います。入力DLLの登録と自動起動設定を解除し、個人設定と学習データは保持します。

## メンテナー向けの流れ

インストーラーはKanade本体を内部に持ち、macOSによるアプリの一時移動にも対応します。入力ソースの切り替えとアプリの置き換えは、既存の共通ヘルパーへ委譲します。ペイロードは実行前に署名を検証する構成です。

`Release`は手動実行のworkflowです。`dry_run`をオンにすると次の版を計算するだけの実行になります。ファイルの変更、コミット、タグ、ビルド、署名、公開は行いません。

本番実行には、既定ブランチの対象コミットでCIが成功していることと、配布用の署名・公証情報が必要です。公開前に[ネイティブ検証項目](https://github.com/owayo/kanade-ime/blob/main/docs/status.md#native-release-gates)も確認してください。署名情報がなければ採番やタグ作成より前に止まります。情報の名前はworkflowの事前検査と署名ステップで確認できます。

採番は`cargo xtask bump-version`を使います。日本時間の`yy.m.counter`で、ワークスペースと両方のmacOSアプリを一括更新します。インストーラーの版は本体から生成します。同じタグがある場合は止めます。ブランチとタグは`git push --atomic`で一度に送信します。

ビルドではキャッシュを使いません。macOSではGUIインストーラーをテストします。DMG本体に加え、マウント後の署名・チェックサム・事前検査も検証対象です。インストーラー内部の`KanadeInstaller --dry-run`は導入前の検査だけを実行し、`--uninstall-dry-run`は削除前の検査だけを実行します。

WindowsはMSIの登録失敗の取り消しと、EXE経由の新規インストール・削除を使い捨てのCIランナーで確認します。Burnの内部エンジン、MSIと各実行ファイルを署名した後、EXE全体を署名します。実際のアプリでのキー入力とAppContainer対応には、別途ネイティブ検証が必要です。

DMGとEXEは変形せずジョブ間で受け渡します。公開前に対象名・件数・通常ファイルであることを確認し、`SHA256SUMS`を作成します。公開後の照合対象は、名前、サイズ、状態とGitHubが返すSHA-256です。既存のRelease添付を差し替える運用はしません。

Homebrewとwingetの更新ジョブはありません。
