# ユーザー辞書の管理

設定画面の「ユーザー辞書」から、Mozc のユーザー辞書を登録・編集・削除できます。

「単語を登録…」で読み・単語・品詞・保存先を指定し、「保存」を押します。辞書を指定しなければ「Kanade 手動登録」を使います。辞書の作成、名前の変更、削除も同じ画面で行えます。削除前には対象を確認し、保存前の編集を閉じるときは破棄するか選べます。

「候補・訂正記録」の「辞書へ登録…」を押すと、読みと単語が登録フォームに入ります。内容を確認し、「保存」を押して登録してください。ユーザー辞書の単語一覧は50件ずつ表示します。

入力中の内容を確定し、操作が落ち着くと保存とエンジンへの反映が進みます。処理中は同じ変更を再送せず、「一覧・処理状況を更新」で確認できます。別の操作で辞書が更新された場合は、最新の一覧から編集を開き直してください。

同梱の署名済みCLIでも管理できます。

まず一覧を取得します。`revision` はファイル全体の版、`dictionary_index` と `entry_index` はその版における位置です。JSON内の `REVISION` を取得した値に置き換えてください。

```bash
KANADE_UI="$HOME/Library/Input Methods/Kanade.app/Contents/Helpers/Settings.app/Contents/MacOS/kanade-ui"
"$KANADE_UI" dictionary list
"$KANADE_UI" dictionary list 50
```

単語を追加する場合は、次の内容を `edit.json` に保存します。辞書を指定しなければ「Kanade 手動登録」を使います。`pos` は省略時に名詞です。指定する場合はMozcの品詞名を小文字で記述します。たとえば `noun`、`proper_noun`、`personal_name` です。

```json
{
  "operation": "upsert",
  "revision": "REVISION",
  "entry": { "reading": "かなで", "value": "奏", "pos": "noun" }
}
```

```bash
"$KANADE_UI" dictionary edit edit.json
"$KANADE_UI" dictionary status
"$KANADE_UI" dictionary status JOB_ID
"$KANADE_UI" dictionary cancel JOB_ID
```

編集はジョブとして受け付けます。未確定入力も文書への反映待ちもなく、最後の入力から2秒以上経つと保存します。待機は最大10分で、その間は取り消せます。保存が始まった後の取り消しは拒否します。同時に実行できる編集は1件です。

`ready` は保存と再読込の完了、`failed` は失敗です。`saved_revision` があれば、ファイルの保存は済んでいます。再読込が失敗した場合も保存済みの変更を勝手に元へ戻しません。エンジン再起動時はそのファイルを読み込みます。ジョブの状態は直近16件だけをメモリに持ち、再起動すると消えます。再起動後は一覧の `revision` で保存結果を確かめてください。

一覧を取得してから別の編集が保存されていれば、`revision_conflict` で拒否します。最新の一覧を取り直し、対象の位置を確かめてから編集してください。未対応の保存形式や未知のprotobufフィールドがあれば、データを落とさずに編集を拒否します。

ほかの操作も同じJSONファイルで指定します。

| 操作 | `operation` | 追加する項目 |
|---|---|---|
| 既存の語の更新 | `upsert` | `dictionary_index`、`entry_index`、`entry` |
| 辞書を指定して語を追加 | `upsert` | `dictionary_index`、`entry` |
| 語の削除 | `delete` | `dictionary_index`、`entry_index` |
| 空の辞書の作成 | `create_dictionary` | `name` |
| 辞書名の変更 | `rename_dictionary` | `dictionary_index`、`name` |
| 辞書全体の削除 | `delete_dictionary` | `dictionary_index` |

すべての操作に `revision` が必要です。語には任意の `comment` を付けられます。読み・表記・コメント・辞書名はそれぞれ300バイト以下で、制御文字は使えません。同じ辞書に同じ読み・表記・品詞を追加した場合は、重複を増やさずコメントを更新します。

訂正記録にある語を明示して登録する場合は、`learning list` で記録を確認し、そのIDを指定します。次の内容を `promotion.json` に保存してください。品詞と登録先は省略できます。

```json
{ "revision": "REVISION", "pos": "noun" }
```

```bash
"$KANADE_UI" learning list
"$KANADE_UI" dictionary promote RECORD_ID promotion.json
```

読みと表記は、文書への反映を確認済みの訂正記録から取得します。削除済みのIDは受け付けません。登録先を省略すると「Kanade 手動登録」に入ります。この操作でKanadeの訂正記録は削除しません。逆に `learning clear` を実行しても、Mozcへ登録した語は残ります。登録した語を消す場合は、辞書側で削除してください。
