# Languages and scoped App MCP / 多語系與 App MCP / 言語と App MCP

## English

FilmCraft retains English, Spanish and Brazilian Portuguese and adds Traditional Chinese
(`zh-TW`) with expanded Japanese (`ja`). Choose **Edit → Language**; the choice is
saved in Preferences and restored on restart. `system` uses the OS language preference;
`zh-Hant`, Taiwan, Hong Kong and Macao select Traditional Chinese. Simplified Chinese does
not silently select Traditional Chinese. Missing translations fall back to English. Project
names, file names, command ids and automation ids remain unchanged. The main editing,
Project, Media Browser, Timeline, Source/Program, Effect Controls, Export and menu surfaces
are covered; secondary dialogs/effect names can still fall back to English. Diagnostic
categories are translated while the original engine detail is retained.

Chinese and Japanese need an installed system CJK font or the optional
[craft-fonts build input](../CLAUDE.md). No fonts are installed automatically. Chinese
prefers Traditional Chinese glyph faces; Japanese retains its Japanese fallback chain.
Unavailable CJK fonts produce a message and an English interface.

### Connect an agent to the real desktop App

The server uses the existing [official Rust MCP SDK](https://github.com/modelcontextprotocol/rust-sdk)
and [stdio transport](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/stdio).
Build the desktop App and CLI, create a dedicated local media workspace, then start:

```powershell
cargo build --locked -j1 -p filmcraft -p filmcraft-cli
New-Item -ItemType Directory -Force C:\FilmCraftWork
.\target\debug\filmcraft.exe --empty --no-recover --control 9876
# Configure your MCP client to launch this command on stdio:
.\target\debug\filmcraft-cli.exe mcp --bridge 127.0.0.1:9876 --workspace C:\FilmCraftWork
```

The desktop control listener is loopback only. Do not expose it publicly. `--bridge` sends
commands into the same App session that the UI uses; edits, History, saved project and
export jobs are visible there. Without `--bridge`, capabilities report `headless_engine`:
that mode is a separate engine session and does not operate a desktop App.

A client configuration example (replace both absolute paths for your machine):

```json
{
  "mcpServers": {
    "filmcraft": {
      "command": "C:\\FilmCraft\\filmcraft-cli.exe",
      "args": ["mcp", "--bridge", "127.0.0.1:9876", "--workspace", "C:\\FilmCraftWork"]
    }
  }
}
```

Apply this configuration yourself in the client you choose. FilmCraft does not install
tools, edit persistent agent configuration or grant permissions. The repository's old
`.mcp.json` example predates workspace scope; add an explicit absolute `--workspace` before
using it. MCP rejects `--project` startup; open via `project_write_v1` so media references
are checked before loading. All paths, including project media/proxies, must stay inside
the authorized workspace; URLs, UNC/device paths, traversal, alternate streams, protected
configuration directories and escaping symlinks/junctions are rejected. Existing parents
are required for output files. Use a workspace containing only material the agent may read.
Project media and scratch-directory references must be absolute; older relative references
need relinking before agent access. Relative tool arguments are normalized to the workspace.

### Versioned tool contract and editing example

`tools/list` supplies the generated JSON schemas. Every v1 response includes
`schema_version: 1` and `ok`. Tool execution failures use MCP `isError: true` plus
`error.code`/`error.message`; malformed or unknown arguments use JSON-RPC invalid params.

| Tool | Actual App operation |
| --- | --- |
| `app_capabilities_v1` | Mode, workspace, supported/unsupported actions and safety rules |
| `project_read_v1` | Project tree, stable item ids, revision, dirty/saved state |
| `project_write_v1` | `create`, `open`, `save` (`.fcproj`) |
| `media_import_v1` | 1–32 MP4/MOV/MKV, WAV/MP3/FLAC/OGG/AIFF, PNG/JPEG files |
| `timeline_read_v1` | Active sequence, tracks, stable clip ids and source ranges |
| `timeline_edit_v1` | `create_sequence`, `activate_sequence`, `place`, `move`, `trim`, `split`, `delete` |
| `history_v1` | App `undo` or `redo` |
| `export_start_v1` | Asynchronous software H.264/AAC MP4, even size ≤1920×1080, 1–60 fps |
| `export_status_v1` | Progress, completion, failure and result of an owned App job |
| `export_cancel_v1` | Cancel an owned App job; partial output is removed |

Each mutation requires `guard: {request_id, revision, confirm?}`. Read the project first
and use its actual revision. The App atomically checks revision when dispatching the
underlying command, and undoable edits use normal History. Get item/clip ids from the
read/import result; do not guess ids. Track names such as `V1`/`A1` come from the sequence.
Times are integer ticks (254016000000 per second); split/place positions are integer frames.
Use `activate_sequence` with the stable `sequence` id to select an existing timeline. Undo/redo
restores project data; after redoing sequence creation, reactivate that restored id before editing.

Example `tools/call` argument objects, each using the **latest** returned revision:

```json
{"name":"project_write_v1","arguments":{"guard":{"request_id":"p-create-1","revision":0},"action":"create","name":"Agent project"}}
{"name":"timeline_edit_v1","arguments":{"guard":{"request_id":"seq-1","revision":1},"edit":{"action":"create_sequence","name":"Sequence","width":320,"height":180,"fps":24}}}
{"name":"media_import_v1","arguments":{"guard":{"request_id":"import-1","revision":2},"paths":["synthetic.mp4"]}}
```

Revision numbers above are illustrative. After importing, use the real returned media id:

```json
{"name":"timeline_edit_v1","arguments":{"guard":{"request_id":"place-1","revision":3},"edit":{"action":"place","item":12,"track":"V1","audio_track":"A1","frame":0,"insert":true}}}
{"name":"project_write_v1","arguments":{"guard":{"request_id":"save-1","revision":4},"action":"save","path":"agent.fcproj"}}
{"name":"export_start_v1","arguments":{"guard":{"request_id":"export-1","revision":4},"path":"preview.mp4","width":320,"height":180,"fps":24}}
{"name":"export_status_v1","arguments":{"job":1}}
```

**Retry and confirmation:** one request id represents one intended mutation. Repeating the
identical request returns the cached response with `replayed: true` and never repeats the
edit/import/export. Reusing it with different arguments returns `REQUEST_ID_CONFLICT`.
The serialized ledger lasts for this server process, up to 1024 unique mutations. It
also retains uncertain delivery results; a lost bridge reply is never automatically
retransmitted. Inspect actual App state before a new intended operation, especially after
server restart. `REVISION_CONFLICT` requires a fresh read and a new request id.

Obtain explicit user confirmation before `guard.confirm: true` for existing file overwrite,
clip deletion, overwrite placement/movement, discarding a dirty project, or export
cancellation. A rejected request id remains rejected; after approval submit a new id.
Jobs can only be inspected/cancelled by the MCP process that started them. `OUTPUT_BUSY`
prevents writing an output while an owned export is still running.
Keep each MCP process attached to one desktop App process; restart MCP after restarting
the App. Clip/item ids belong to the saved project, while job ids belong to the App session.

**Limits:** v1 does not publish/upload, read external workspace files, change agent
configuration, expose a public server, import project files as media, or export other
formats. Raw command mutations and raw UI input tools return explicit unsupported errors
in workspace mode; UI inspection, element listing, screenshots and previews remain
available. The trusted local desktop control protocol remains a developer interface, not
an OS sandbox. Workspace checks do not prevent another local process from changing files
or the App independently. Use dedicated local material and reread after user edits.

### Reproducible desktop validation

`node scripts/verify-mcp-app.cjs <filmcraft-cli.exe> <workspace> <port>` runs stdio initialize,
tool discovery, real App create/import/place/split/undo/redo/save/export, retry/conflict,
path rejection, failure and cancellation checks against the running App. Generate only
synthetic input (external ffmpeg is a fixture generator, never a product dependency):

```powershell
ffmpeg -f lavfi -i testsrc2=size=320x180:rate=24 -f lavfi -i sine=frequency=440:sample_rate=48000 -t 2 -c:v libx264 -pix_fmt yuv420p -threads 1 -c:a aac C:\FilmCraftWork\synthetic.mp4
node scripts/verify-mcp-app.cjs .\target\debug\filmcraft-cli.exe C:\FilmCraftWork 9876
```

The runner writes evidence JSON and synthetic output inside the workspace. It expects an
empty disposable App session and confirms only its own generated test files/edits.

## 繁體中文

保留英文、西班牙文與巴西葡萄牙文，新增繁體中文並擴充日文。在 **編輯 → 語言**
選擇語言，偏好會保存並在重啟後恢復。`system` 依作業系統偏好選擇；`zh-Hant`、臺灣、
香港及澳門使用繁中。缺少翻譯時回退英文，專案名稱、檔名、command 與 automation ID
維持原值。主要編輯、專案、媒體瀏覽器、時間軸、來源／節目監看、效果控制、匯出與選單
已有翻譯；部分次要對話框／效果名稱仍會顯示英文。錯誤分類翻譯後保留原始診斷細節。
需要系統 CJK 字型或選用 craft-fonts 建置輸入；不會自動安裝字型。

使用上方 PowerShell 指令啟動 App，再讓 MCP client 以 stdio 啟動 CLI，指定
`--bridge 127.0.0.1:9876 --workspace C:\FilmCraftWork`。`live_app` 模式操作真正桌面
App 的共用 command layer，專案、時間軸、undo/redo 與輸出 job 同步反映在 UI。
未指定 bridge 的 `headless_engine` 是獨立引擎 session。伺服器僅使用 loopback，請勿
對外開放；本程式不會修改 agent 持久設定或授予權限。請自行套用上方 MCP 設定範例。

先呼叫 `app_capabilities_v1`、`project_read_v1`，再依上方工具表操作。每次修改須帶
`guard.request_id` 及最新 `guard.revision`，使用讀取／匯入結果的穩定 ID。相同 request id
與相同參數重送會返回 `replayed: true`，不重複修改；不同參數使用同一 id 會報錯。
冪等記錄只在此 MCP process 存活，最多 1024 次；斷線回覆不確定時，先核對 App 狀態。
revision 衝突時重新讀取，再建立新的操作 id。工具錯誤有 `schema_version: 1`、
`ok: false`、`error.code`／`error.message`，並設 MCP `isError: true`。

覆寫檔案、刪除片段、覆寫放置／移動、捨棄未存內容及取消輸出，必須先取得使用者明確
確認，再以新 request id 設 `guard.confirm: true`。取消會移除產生中的部分輸出。
只可查詢／取消此 MCP process 啟動的 job。所有媒體、專案內引用及輸出路徑均限制在
workspace；拒絕越界、URL、UNC／裝置路徑、設定目錄及導向外部的 symlink／junction。
專案內媒體與暫存目錄必須使用絕對路徑；舊相對參照需先重新連結。工具的相對路徑參數
會正規化至 workspace。
每個 MCP process 綁定一次 App 執行；App 重啟後也應重啟 MCP。片段／媒體 id 屬於
保存的專案，job id 屬於該次 App session。
workspace 應只放允許 agent 閱讀的素材。它不限制其他本機 process 或使用者直接操作 App。

v1 匯出支援軟體 H.264/AAC MP4，偶數尺寸最高 1920×1080、1–60 fps；其他格式、
外傳／發佈、外部檔案、持久 agent 設定及原始 UI 輸入明示不支援。仍可讀取 UI 狀態、
元素及截圖。上方驗證指令使用自造影音，檢查真正 App 狀態、保存、短輸出、重送、失敗
與取消；不得將私人的錄影放入驗證 workspace。

## 日本語

プロジェクト内のメディアと作業フォルダーの参照は絶対パスが必要です。既存の相対参照は
再リンクしてください。ツール引数の相対パスは workspace 内に正規化されます。
App を再起動したら MCP process も再起動してください。item/clip id は保存した
プロジェクトに属し、job id は App session に属します。

英語・スペイン語・ブラジルポルトガル語を維持し、繁体字中国語を追加、日本語を拡充
しました。**編集 → 言語** で選択すると設定が保存され、再起動後に
復元されます。`system` は OS の言語設定を使います。不足する翻訳は英語に戻り、
プロジェクト名・ファイル名・command/automation ID は変わりません。主要な編集、
プロジェクト、メディアブラウザー、タイムライン、モニター、エフェクトコントロール、
書き出しとメニューを翻訳しています。一部の補助ダイアログやエフェクト名、診断詳細は
英語のままです。CJK システムフォントまたは任意の craft-fonts ビルド入力が必要です。
フォントを自動インストールしません。

上記 PowerShell の手順で App を起動し、MCP クライアントから CLI を stdio で起動して
`--bridge 127.0.0.1:9876 --workspace C:\FilmCraftWork` を指定します。`live_app` は
デスクトップ App と同じ command layer を使い、編集・履歴・保存・書き出し job が
実際の UI に反映されます。bridge を指定しない `headless_engine` は別のエンジン
セッションです。loopback 接続を公開しないでください。永続的な agent 設定や権限は
変更しません。上記の設定例は使用するクライアントに手動で適用してください。

最初に `app_capabilities_v1` と `project_read_v1` を呼び出します。変更には一意の
`guard.request_id` と最新の `guard.revision` が必要です。取得した item/clip ID を
使用してください。同じ id と引数の再送は `replayed: true` を返し、処理を繰り返し
ません。別の引数で同じ id を使用するとエラーです。記録は MCP process の寿命内で
最大 1024 件です。応答喪失や process 再起動後は App の状態を確認し、revision が
競合したら読み直して新しい操作 id を作成してください。実行エラーは MCP
`isError: true` と `schema_version: 1`、`error.code`／`error.message` を返します。

ファイル上書き、クリップ削除、上書き配置／移動、未保存内容の破棄、書き出し中止は
ユーザーの明示的な確認後に、新しい id で `guard.confirm: true` を設定します。
中止すると部分出力が削除されます。この MCP process が開始した job だけを確認・
中止できます。メディア、プロジェクト内の参照、出力は workspace 内に限定されます。
URL、UNC／デバイスパス、親ディレクトリ移動、設定ディレクトリ、外部への
symlink/junction は拒否します。読み取りを許可する素材だけを workspace に入れて
ください。他のローカル process やユーザーによる App 操作を制限する OS sandbox
ではありません。

v1 はソフトウェア H.264/AAC MP4、偶数サイズ最大 1920×1080、1–60 fps の書き出し
に対応します。その他の形式、アップロード／公開、workspace 外のファイル、永続
agent 設定、直接 UI 入力は明示的に未対応です。UI 状態、要素、スクリーンショットは
取得できます。上記の検証手順は合成映像・音声だけで実際の App、保存、短い書き出し、
再送、失敗、中止を確認します。個人の映像は検証に使用しないでください。

### 序列重新選取 / シーケンスの再選択

`timeline_edit_v1` 的 `edit: {"action":"activate_sequence","sequence":5}` 會透過 App 原有命令開啟指定序列；ID 必須來自 `project_read_v1`。建立序列的 undo/redo 恢復資料後，用原穩定 ID 重新選取才能繼續編輯。

`timeline_edit_v1` の `edit: {"action":"activate_sequence","sequence":5}` は App の既存コマンドでシーケンスを開きます。ID は `project_read_v1` で確認してください。シーケンス作成を redo した後は、復元された同じ ID を選択してから編集を続けます。
