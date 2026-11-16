# i-db

`db` is a terminal database editor for PostgreSQL and Redis. It reads an existing DataGrip project (`.idea/dataSources.xml` and friends) as its connection list and takes passwords from a `.env` file next to it, so there is no second place to maintain connections. One binary gives you a full-screen TUI, a scriptable CLI and a read-only MCP server for AI agents.

The behavior is specified rule by rule in [SPEC.md](SPEC.md). The UI text and the spec are currently written in Korean.

## Features

- Connection tree grouped like DataGrip, with schemas, tables, views, functions and Redis key namespaces
- SQL editor with syntax highlighting, completion, live templates (`sel`, `ins`, `upd`, ...), formatting, `:name`/`$1` parameters, go-to-definition
- Results grid: sort (`s` or header click), filter with `/` (`abc`, `!abc`, `col:abc`, `col=v`, `col>v`, `col=null`), re-read from the server when possible (PostgreSQL, OpenSearch `_search`, Loki `time`, DynamoDB sort key; otherwise the status line says why), `]` loads the next 1,000 rows of a truncated PostgreSQL result, copy as TSV/CSV/JSON/INSERT/UPDATE/`IN (...)` (a `Ctrl+K` or right-click menu picks row / selection / visible rows / column × TSV or CSV with header, JSON, Markdown table, INSERT, `IN` list; copies go to OSC 52 and the system clipboard), charts, JSON tree, cell aggregates, result pinning and diff
- In-place editing of single-table results (preview SQL before submit, undo of the last submit), paste from spreadsheets, CSV import
- Server-side table view with a DataGrip-style WHERE / ORDER BY bar (column completion, `/` short filters), paging, exact count; follow foreign keys in both directions
- Linked rows (`Ctrl+B` on a result cell, or right-click → `연결된 곳 찾기`) and `db linked`: find where a value such as `user_id = 1315` appears in other tables even without foreign keys, by column naming (`user_id`, `*_user_id`, `users.id`) across the same database and the other PostgreSQL connections of the same group and environment (a read replica such as `app-stg-read` is skipped when the writer for the same database is searched too). Only columns that lead an index are counted (up to 100+ rows, 2 s per table, read-only); `Enter` opens the table view filtered to that value. The value is never put in tab titles, history or the session
- History (`Ctrl+H`) with result snapshots: every run is logged with its connection, row count and time, and table/text results (first 1,000 rows, up to 1 MiB) are kept as gzip snapshots for 30 days / 200 MiB. `Enter` reopens a snapshot read-only in a new tab (`r` re-runs it), `Ctrl+E` puts the SQL in the editor, `Ctrl+D` deletes the snapshot, filter by text, connection, `오늘`/`today`, `어제`/`yesterday`. Prod results are not kept unless `DB_HISTORY_PROD=1`; statements with secrets never are. `db history` lists and prints them
- Command palette (`Ctrl+P`) that runs any action or jumps to any table or connection, frequently used actions and recently opened tables first
- Remembers per table the last sort, WHERE (not on prod) and column widths, `↑`/`↓` recall in the `/` filter and WHERE inputs, and which tree nodes were expanded (`DB_RECENT=0` turns this off)
- EXPLAIN / EXPLAIN ANALYZE as a plan tree, activity/locks/table sizes, cancel or terminate backends
- Transactions in Auto or Manual mode, full-schema data search, schema diff between connections, run a statement on another connection and diff the results
- Redis: key browser, value editor for every type, TTL/rename, monitor (INFO, SLOWLOG, big keys), pattern bulk delete
- `queries/` folder integration: files are attached to connections via DataGrip's `data_source_mapping.xml`, auto-save, session restore
- Optional client-side decryption view and `enc('...')` expansion for AES-CBC values stored as `<iv hex>^|^<ciphertext hex>` (pgcrypto `encrypt_iv` compatible). Keys come from `.env` (`<GROUP>_AES_KEY`, `<GROUP>_AES_IV`); nothing is installed in your database. Without a key/IV, `enc(...)` is sent as a call to an existing `staticEncrypt(...)` function, so it only works if your database already has one
- Read-only sources next to the databases: OpenSearch, Loki, Prometheus, Grafana (Loki/Prometheus data sources), DynamoDB and Athena in the same tree, editor and results grid, plus a tree-only `aws` view of Lambda, EventBridge, Scheduler and Step Functions (SNS, SQS and S3 too with `DB_ALLOW_BILLED=1`). AWS calls billed per request or per byte scanned are never sent unless you opt in (see Sources)
- Dashboard (`Ctrl+G`) and `db status`: every connection's state, latency, version and key numbers at a glance (see Dashboard)
- Event flows (`Alt+F`) and `db flows`: an outbox table → SNS topic → SQS queues → DLQ · Lambda diagram with live outbox counts, per-event-type retry limits and the Lambda consumers of each queue (no billed AWS calls), configured in `.dbtui/flows.json` (see Flows)
- Query boards (`Alt+B`) and `db board`: your own grid of read-only query panels (numbers, tables, charts, flows) across PostgreSQL, Redis and every source, configured in `.dbtui/boards/<name>.json` (see Boards)
- Optional "user lookup" that runs a configurable set of read-only queries across several databases from one email/uid/id/nickname (see [examples/user360.json](examples/user360.json))

## Install

### Release archives

Each `v*` tag publishes `db_<os>_<arch>.tar.gz` for `darwin` and `linux` on `amd64` and `arm64`, plus `checksums.txt`, on the [Releases](https://github.com/Jeong-Jae-Hun/i-db/releases) page. The archive contains a single binary named `db`.

```sh
os=$(uname -s | tr A-Z a-z); arch=$(uname -m | sed 's/x86_64/amd64/;s/aarch64/arm64/')
curl -fsSL "https://github.com/Jeong-Jae-Hun/i-db/releases/latest/download/db_${os}_${arch}.tar.gz" | tar -xz -C ~/.local/bin db
db -version
```

### Updating

`db update` downloads the latest release for this OS and CPU, verifies it against `checksums.txt` and replaces the running binary in place (through a symlink, it replaces the target). No `gh` or login is needed; `GITHUB_TOKEN`, when set, raises the API rate limit.

```sh
db update --check            # prints "새 판 v0.11.0 → v0.12.0" and exits 10 when a newer release exists
db update                    # v0.11.0 → v0.12.0
db update --version v0.11.0 --force   # go back to an older release
```

If the binary's folder is not writable, it stops with `쓸 수 없음: <path> (sudo 로 다시)`. Builds from source (`dev`) are only replaced with `--force`. The TUI checks for a new release once a day at startup and shows `새 판 vX — db update` in the status line (`DB_UPDATE_CHECK=0` turns this off).

### go install

```sh
go install github.com/Jeong-Jae-Hun/i-db@latest
```

Go names the binary after the module, so this installs `i-db`, not `db`. Rename or alias it if you want the short name (`mv "$(go env GOPATH)/bin/i-db" "$(go env GOPATH)/bin/db"`). The examples below use `db`.

## Quick start

Point `db` at a folder that contains a DataGrip `.idea` directory:

```sh
export DBTUI_PROJECT=~/work/db-project   # or: db -p ~/work/db-project, default is the current directory
db
```

It reads:

| File | Used for |
|---|---|
| `.idea/dataSources.xml` | connection name, group, driver (`postgresql` / `redis`), JDBC URL |
| `.idea/dataSources.local.xml` | user name per connection |
| `.idea/data_source_mapping.xml` | which connection each `queries/*.sql` file belongs to |
| `.env` | passwords and optional encryption keys (below) |
| `.dbtui/connections.json` | optional extra connections added from `db` itself (see Connections) |
| `.dbtui/folders.json` | optional tree folders you moved connections into (see Folders) |
| `.dbtui/flows.json` | optional event flows for the flow tab and `db flows` (see Flows) |
| `.dbtui/boards/<name>.json` | optional query boards for the board tab and `db board` (see Boards) |
| `.dbtui/favorites.json`, `.dbtui/user360.json` | optional, meant to be committed with the project |

Passwords come from `.env`, URL-encoded, keyed by the connection name upper-cased with non-alphanumerics turned into `_`:

```sh
PG_APP_STG_PASSWORD=p%40ssword        # connection "app-stg"
REDIS_APP_STG_REDIS_PASSWORD=secret   # connection "app-stg-redis"
APP_AES_KEY=...                       # optional, for group "app" (decrypt view, enc())
APP_AES_IV=00112233445566778899aabbccddeeff
```

Other drivers (MySQL, ...) in the DataGrip project are skipped.

### Connections

Connections you add from `db` go to `.dbtui/connections.json`; `.idea` is never written. They show up with a `(db)` suffix in the tree, the palette and `db conns`. Passwords still go to `.env` (URL-encoded, same key scheme; a new `.env` is created with mode 0600, an existing one keeps its mode). If a name clashes with a DataGrip connection, the `db` one is skipped with a warning.

```json
[{"uuid":"…","name":"app-stg2","group":"app","kind":"postgresql","host":"db.stg.example.internal","port":5432,"db":"app","user":"app_ro"}]
```

In the TUI, `N` on a group or connection line (or `새 접속` in the palette) opens a form. Paste a URL such as `postgres://user:pw@host:5432/db`, `redis://:pw@host:6379/0` or `rediss://…` into the first field to fill the rest. `Ctrl+T` tests without saving, `Enter` tests and then saves (after a failed test it asks before saving). `E` edits a `(db)` connection (an empty password keeps the old one), `X` on a `(db)` PostgreSQL connection deletes it together with its `.env` password. On Redis lines `X` stays the pattern delete; use `Alt+X` (`접속 지우기` in the palette) for Redis connections. DataGrip connections are edited in DataGrip.

```sh
db conn add app-stg2 --url 'postgres://app_ro:pw@db.stg.example.internal:5432/app' --group app
db conn edit app-stg2 --port 6432
printf '%s' "$NEW_PW" | db conn edit app-stg2 --password-stdin
db conn test app-stg2            # or: db conn test --url 'redis://localhost:6379/0'
db conn rm app-stg2 --yes
```

Passwords are only taken from `--url` or `--password-stdin` and never printed. `add` and `edit` test the connection first and refuse to save on failure unless `--force` is given (exit code 1). `rm` asks on a terminal and needs `--yes` otherwise. Connecting to a prod connection needs `--prod`, as with the other commands.

### Folders

The tree groups connections into folders by their group, sorted by name with ungrouped ones (`기타`) last; inside a folder they go local → stg → prod. `E` on a folder line renames it, `M` on a connection line moves it to another folder (empty = back to its group). This works for DataGrip connections too: only `.dbtui/folders.json` is written, and the group itself is kept, so encryption keys (`<GROUP>_AES_KEY`) still resolve the same way.

```sh
db conn folder app-stg2 ops          # move
db conn folder app-stg2 --reset      # back to its group
db conn folder --rename mon monitoring
```

### Sources

OpenSearch, Loki, Prometheus, Grafana, DynamoDB and Athena connections are read-only and live only in `.dbtui/connections.json` (DataGrip sources are unchanged). Fields are `url` (DynamoDB: optional endpoint), `auth` (`basic`, `sigv4` for OpenSearch, `token` for Grafana, or none), `user`, `region`/`profile` for SigV4, DynamoDB and Athena, and `workgroup` (default `primary`)/`output` (`s3://…`, default: the workgroup's result location) for Athena. Secrets go to `.env` as `OS_<NAME>_PASSWORD`, `LOKI_<NAME>_PASSWORD`, `PROM_<NAME>_PASSWORD` or `GRAFANA_<NAME>_TOKEN`; AWS credentials are never stored and come from `profile` or the standard chain.

```json
[{"uuid":"…","name":"search-stg","kind":"opensearch","url":"https://search.stg.example.internal","auth":"sigv4","region":"ap-northeast-2","profile":"stg"},
 {"uuid":"…","name":"logs-stg","kind":"loki","url":"http://loki.stg.internal:3100"},
 {"uuid":"…","name":"orders","kind":"dynamodb","region":"ap-northeast-2"},
 {"uuid":"…","name":"athena-prod","group":"data","kind":"athena","region":"ap-northeast-2","profile":"team","workgroup":"primary","output":"s3://bucket/prefix/"},
 {"uuid":"…","name":"infra-stg","kind":"aws","region":"ap-northeast-2","profile":"stg"}]
```

In the connection form the kind field cycles through all nine kinds and shows only the fields that kind needs; pasting an `https://user:pw@host:9200` URL fills url, user and password. In the tree, these connections expand lazily (indices and mapping fields, Loki labels and values, Prometheus metrics, Grafana data sources, DynamoDB tables and keys) with details dimmed on the right. `Enter` previews an index, label value, metric or table in a result tab, `l` expands, `r` reloads. `Ctrl+R` sends the editor text as the source's own query language (`GET idx/_search` + JSON body or `SELECT …` for OpenSearch, LogQL, PromQL, PartiQL `SELECT`). Anything that looks like a write is refused before sending (`읽기 전용 저장소라 보내지 않음`), and expensive queries (DynamoDB full scan, OpenSearch `size` over 10,000, Loki/Prometheus ranges over 7 days) ask `비싼 질의: y/N` first. Loki/Prometheus/Grafana tabs show the range (`최근 1h`) above the editor; `Ctrl+]` cycles 15m, 1h, 6h, 24h, 7d per tab. Results are ordinary grids, so `c` charts metric series. Table view, editing and transactions do not apply.

```sh
db conn add logs-stg --kind loki --url http://loki.stg.internal:3100
printf '%s' "$GRAFANA_TOKEN" | db conn add graf --kind grafana --url https://grafana.example.com --password-stdin
db tables -c search-stg
db query -c search-stg 'GET logs-*/_search
{"query":{"match":{"status":500}},"size":20}'
db logs -c logs-stg '{app="api"} |= "error"' --since 6h
db metrics -c prom 'sum by (status) (rate(http_requests_total[5m]))' --since 24h --until 1h
db query -c orders 'SELECT * FROM "orders"' --force   # full scan needs --force (exit 3 without)
db conn add athena-prod --kind athena --region ap-northeast-2 --profile team --output s3://bucket/prefix/
db query -c athena-prod --prod $'-- database: sales\nSELECT * FROM orders WHERE dt = \'2026-11-01\' LIMIT 100'
```

Athena accepts only one `SELECT`, `WITH … SELECT`, `SHOW`, `DESCRIBE` or `EXPLAIN` statement; CTAS, `INSERT`, `UNLOAD`, `MSCK` and the like are refused. Source tabs have no selected database, so write `"db"."table"` or put `-- database: <name>` on the first line. Queries reuse results from the last 60 minutes to avoid paying for the same scan twice, the line above the grid shows `스캔 1.2GB · 엔진 0.6초`, and cancelling a running query sends `StopQueryExecution` so it stops scanning. The dashboard shows the workgroup state and per-query scan cutoff. IAM needs `athena:ListDatabases`, `ListTableMetadata`, `GetTableMetadata`, `StartQueryExecution`, `GetQueryExecution`, `GetQueryResults`, `StopQueryExecution`, `GetWorkGroup`, Glue catalog reads, S3 reads on the data and writes on the result location.

**Billed calls are off by default.** AWS calls that are charged per request or per byte scanned are refused before anything is sent, with `요금이 붙는 조회라 막음 (DB_ALLOW_BILLED=1 로 허용)` in the status line, `db` exit code 3, or an MCP tool error. Start `db` (or `db mcp`) with `DB_ALLOW_BILLED=1` to allow them. Blocked: every SQS action, SNS topic and subscription calls, S3 list/get/head, Athena queries and its Glue catalog lookups (so the whole Athena tree, preview and editor), and DynamoDB data reads (table `Enter`, PartiQL, server-side re-sorting, MCP `query`, board panels). Always allowed because they are free: `sts:GetCallerIdentity`, Lambda, EventBridge, EventBridge Scheduler and Step Functions list calls, DynamoDB `ListTables`/`DescribeTable` (the DynamoDB tree) and Athena `GetWorkGroup` (the dashboard and the connection test, which skips the billed catalog listing). DynamoDB Local (`url` on `localhost`) is never blocked. The dashboard and flows never make billed calls even when allowed, since they refresh on their own. Data transfer (egress) charges are out of scope.

`--max-rows` defaults to 1,000 for sources. Prod sources need `--prod` like everything else; write-looking queries and unconfirmed expensive queries exit with 3. `describe`, `ddl`, `explain`, `redis`, `search`, `schemas` and `schema-diff` are usage errors on these connections.

An `aws` connection (fields: name, group, kind, region, profile; no url) has no query language: it is browsed in the tree only, and `Ctrl+R` or `db query` answers `aws 접속은 트리로 본다`. The tree has the folders `Lambda`, `EventBridge`, `Scheduler` and `Step Functions`; with `DB_ALLOW_BILLED=1` it also has `SNS 토픽`, `SQS 큐` and `S3`, which are billed per request. In full: `SNS 토픽` (topics → subscriptions; same-account SQS endpoints show the queue name), `SQS 큐` (each queue shows `대기 12 · 처리 중 3 · 지연 0`, plus `· DLQ <name> <n>` highlighted when its dead-letter queue has messages), `Lambda` (runtime and last modified), `EventBridge` (buses → rules with schedule or `pattern` and state), `Scheduler` (groups → schedules with expression and state), `Step Functions` (state machines) and `S3` (buckets → `/` prefixes → objects, first 1,000 per level). `Enter` on a topic lists its subscriptions (`protocol, endpoint, subscription_arn, raw, filter`), on a queue all of its attributes, on a function its event source mappings, on a rule its targets, on a schedule its target, on a state machine its last 50 executions, and on an S3 object its metadata. For text-like objects up to 64 KiB the body is shown too, fetched with a ranged `GetObject` (`bytes=0-65535`), so large objects are never downloaded. A folder the credentials may not list shows a single `권한 없음: <action>` line and the other folders keep working. Only `List*`, `Get*`, `Describe*` and `Head*` calls (plus that ranged `GetObject`) are made (a test scans the package for anything else); nothing receives, sends, purges or deletes messages. The dashboard shows `Lambda 86 · 매핑 꺼짐 2 · 실패 1` for these connections: the function count, event source mappings that are not `Enabled`, and mappings whose last result is a problem (`No records processed` is fine). It uses only the free Lambda list calls, never SQS.

```sh
db conn add infra-stg --kind aws --region ap-northeast-2 --profile stg
db tables -c infra-stg    # top-level folders
```

IAM permissions used (the SNS, SQS and S3 ones only with `DB_ALLOW_BILLED=1`): `sts:GetCallerIdentity`, `sns:ListTopics`, `sns:ListSubscriptionsByTopic`, `sns:GetSubscriptionAttributes`, `sqs:ListQueues`, `sqs:GetQueueAttributes`, `sqs:GetQueueUrl`, `lambda:ListFunctions`, `lambda:ListEventSourceMappings`, `events:ListEventBuses`, `events:ListRules`, `events:ListTargetsByRule`, `scheduler:ListScheduleGroups`, `scheduler:ListSchedules`, `scheduler:GetSchedule`, `states:ListStateMachines`, `states:ListExecutions`, `s3:ListAllMyBuckets`, `s3:ListBucket`, `s3:GetBucketLocation`, `s3:GetObject` (also covers `HeadObject`).

## TUI

Press `F1` for every key binding, or `Ctrl+P` for the command palette. Outside the editor, `?` lists only the keys of the current pane (result, tree, dashboard, flows, board, Redis value editor). The status line shows the keys that work right now, most useful first (for example `a 행 추가 · Enter 고치기 · …` on an editable result, `변경 2건 · Ctrl+S 제출 · u 되돌리기 · …` with pending edits, `y 복사 · C IN 목록 · …` while selecting) and drops items from the end when the terminal is narrow. The first time you land on an editable result, a read-only result, a table view, a flow tab or a board tab, a one-time tip appears there; seen tips are remembered in `tips.json` next to the session file (`DB_TIPS=0` turns tips off). Some common keys:

| Key | Action |
|---|---|
| `Ctrl+R` / `F5` | run selection, or the statement under the cursor |
| `Ctrl+E` / `Alt+E` | EXPLAIN plan tree / EXPLAIN ANALYZE |
| `Ctrl+Space` | completion |
| `Ctrl+N` | find objects (tables, columns, functions, Redis keys); recently opened tables first |
| `Ctrl+O` / `Ctrl+S` | open / save a file under `queries/` |
| `F9` / `F10` / `F11` | Auto/Manual transactions / COMMIT / ROLLBACK |
| `Ctrl+L` | unlock writes on a prod connection for a few minutes |
| `Ctrl+G` | dashboard of all connections (see Dashboard) |
| `Alt+F` · tree `F` on an SNS topic (with `DB_ALLOW_BILLED=1`) | event flows (see Flows) |
| `Alt+B` (⌘⇧B) | query boards (see Boards) |
| `Ctrl+]` | Loki/Prometheus range 15m · 1h · 6h · 24h · 7d (per tab) |
| `Ctrl+H` | history (`Enter` opens the ◆ snapshot in a read-only tab, else the editor; `Ctrl+E` editor; `Ctrl+D` delete snapshot) |
| result `r` in a snapshot tab | re-run that SQL on its connection |
| tree `Enter` on a table | table view (`w`·`/` WHERE, `O` ORDER BY, `[` `]` pages) |
| result `Ctrl+K` (⌘K) · right-click | copy menu: target × format, remembers the last format |
| result `Ctrl+B` (⌘B) | linked rows: other tables (and same-group, same-environment connections) holding the cell value; `Enter` opens one |
| result `]` | load the next 1,000 rows of a truncated PostgreSQL result (up to 10,000) |
| tree `N` / `E` | add / edit a connection (see Connections) |

On macOS, in terminals that speak the kitty keyboard protocol (Ghostty, kitty, WezTerm, iTerm2), the same actions are bound to ⌘ keys. See SPEC R23 and R47.

### Dashboard

**Recent items.** `db` keeps `recent.json` (mode 0600) next to the session file. An empty `Ctrl+P` lists the actions you pick most often (count with a 7-day half-life, top 5), then recently opened tables, then recent connections (`접속 <name>`, `Enter` selects and opens it in the tree), then everything else. An empty `Ctrl+N` puts recently opened tables first. Reopening a table view restores its last `ORDER BY`, WHERE and the column widths you set with `<`/`>`, and the status line says `이전 정렬·조건 적용 (w·O 칸을 비우고 Enter 로 해제)`; on prod connections the WHERE is never written to the file, only sort and widths. In the result `/` filter and the table view WHERE input, `↑`/`↓` step through earlier entries (filters: last 100 across projects, WHERE: last 20 per table, none on prod). Collapsed folders are restored at startup; schemas, Redis folders and query groups under a connection are re-expanded the first time you open that connection in a run, so nothing connects at startup. Up to 500 tables are remembered (least recently opened dropped first). If the file is broken `db` starts empty; if it cannot be written, recent items are off for that run.

`Ctrl+G` (⌘G, palette `대시보드`) opens a single dashboard tab. The top row has summary cards: connections by kind, connections by environment, connected / failed, the sum of PostgreSQL active sessions and Redis clients, open transactions and prod connections whose write lock is currently open. Below is one row per connection: group, name, environment, kind, state (`●` up, `✕` failed, `…` not checked yet), latency, version and the main numbers (numbers over their threshold are highlighted). While a check is running again the rows keep their last values and the header shows `확인 중 N`.

Every connection is checked at the same time with a 5 second limit per query (opening a connection for the first time may take up to 15 seconds), again every 30 seconds while the dashboard is the active tab. The checks only ask for read-only metadata (PostgreSQL `pg_stat_activity` counts and `version()`, Redis `INFO`, OpenSearch cluster health, and so on; see SPEC R64). In the dashboard `r` checks again now, `Enter` selects the connection in the tree, `/` filters by group, name or environment and `esc` clears the filter.

From a shell, `db status` runs the same checks once:

```sh
db status                          # every non-prod connection
db status --prod --format json     # include prod
db status -c app-stg -c app-stg-redis --format tsv
```

Columns are `group`, `name`, `env`, `kind`, `ok`, `latency_ms`, `version` and `metrics` (in JSON, `metrics` is an array of `{name, value, warn}` and `ok` is `null` for skipped connections). Prod connections are not contacted without `--prod` and show `--prod 로 확인`. The exit code is 1 if any checked connection failed. Skipped prod connections do not count, so `db status -c app-prod` without `--prod` exits 0 having probed nothing.

### Flows

A flow follows one event path: an outbox table in PostgreSQL → an SNS topic → the SQS queues subscribed to it → each queue's DLQ and the Lambda functions reading it. Flows live in `.dbtui/flows.json`, which holds only connection, table, column, topic and queue names and is meant to be committed:

```json
[
  { "name": "shop-hub-stg",
    "outbox": { "conn": "shop-stg", "table": "public.t_outbox", "type": "event_type", "retry": "retry_count", "due": "check_needed_at", "where": "event_type like 'DATA_%'" },
    "topic": "stg-shop-hub",
    "aws": "aws-stg",
    "queues": [ { "name": "stg-shop-hub-worker", "dlq": "stg-shop-hub-worker-dlq" },
                { "name": "arn:aws:sqs:ap-northeast-2:999988887777:partner" } ] },
  { "name": "shop-outbox-stg",
    "outbox": { "conn": "shop-stg", "table": "public.t_outbox", "type": "event_type", "retry": "retry_count", "due": "check_needed_at", "limit": 5,
      "types": { "CREATE_PAYOUT": { "limit": 20, "consumer": "consume-outbox-create-payout" }, "CLIP": { "consumer": "consume-outbox-clip" } } } }
]
```

A flow needs an `outbox`, a `topic` or `queues` (any combination). SNS and SQS are billed per request, so a flow never asks them anything: `topic` (a name or ARN) is only a label, and `queues` lists the subscribed queues yourself (`name` is a queue name or ARN, `dlq` an optional dead-letter queue name or ARN, both labels). For each queue the only AWS call is the free `lambda:ListEventSourceMappings` filtered by the queue ARN, which shows the consumers as `λ function · state · last result`; queue names become ARNs with the account from `sts:GetCallerIdentity` and the connection's region. Queue and DLQ depths are not shown, and the flow name carries a dim `수치는 요금이 붙어 보지 않음`. `aws` must name an `aws` connection and is required with `queues` and only used for them (a `topic` alone needs no connection; an old `aws` + `topic` entry without `queues` shows `aws requires queues`). Inside `outbox`, `retry`, `due`, `where`, `limit` and `types` are optional, and `conn` must name a PostgreSQL connection. Table and column names are quoted as identifiers. `where` is raw SQL added with `AND`, must be a single condition without `;`, and every outbox query runs in a READ ONLY transaction with a 5 second limit. Rows are counted per event type in one query that also returns the flow total, so totals stay exact even past the 1,000 type lines kept (then the outbox shows `종류 1,000개만`). Rows with a NULL type show as `(NULL)`. With a `retry` column every type has a retry limit (`limit`, default 5, overridden per type in `types`; both need `retry` and must be at least 1) and rows that reached it are never picked up again, so the numbers are `대기` (retry below the limit), `재시도` (retried but still below the limit), `한도 소진` (retry at or above the limit) and `오래됨` (`due` older than 5 minutes and still below the limit; abandoned rows are not counted as stale). Without `retry`, `대기` is every row matching `where` and `오래됨` only looks at `due`. A missing file just means no flows; a broken entry shows `설정 오류: <reason>` and the others keep working.

`Alt+F` (palette `흐름`) opens a single flow tab, and with `DB_ALLOW_BILLED=1` `F` on an SNS topic in an `aws` tree adds that topic as an extra label-only flow. Flows are listed on the left (`●` fine, `!` warning, `✕` failed, `…` not checked yet) and the selected one is drawn on the right:

```text
shop-hub-stg  수치는 요금이 붙어 보지 않음

[아웃박스 shop-stg public.t_outbox | 대기 120 · 재시도 3 · 한도 소진 0 · 오래됨 1] ──▶ [SNS stg-shop-hub] ─┬─▶ [SQS stg-shop-hub-worker] ──▶ [λ worker OK]
                                                                                                               │   └─ DLQ stg-shop-hub-worker-dlq
                                                                                                               └─▶ [SQS partner]
```

Below the diagram an outbox gets one line per event type (configured types plus the types found in the table), sorted by `한도 소진`, then `대기`, then name; configured types with no rows are dimmed and only shown when there is room:

```text
[아웃박스 shop-stg public.t_outbox | 대기 9 · 재시도 2 · 한도 소진 3 · 오래됨 1]
  ├─ CREATE_PAYOUT  대기 3 · 재시도 1 · 한도 소진 3  → consume-outbox-create-payout
  ├─ CLIP                대기 4 · 재시도 1 · 한도 소진 0 · 오래됨 1  → consume-outbox-clip
  └─ DONATION_CHAT       대기 2 · 재시도 0 · 한도 소진 0  → ?
```

`→` shows the configured `consumer` (`?` when unknown). Type lines are selectable like boxes, and `Enter` on one opens the 50 rows of that type with `retry > 0` (highest retry first) in a read-only result tab.

When the pane is too narrow the boxes stack top to bottom. Retries, exhausted rows, stale rows, a disabled Lambda mapping and a failing last result are highlighted. Flows are resolved again every 30 seconds while the tab is active; `r` refreshes now, `↑↓` picks a flow, `←→` picks a box and `Enter` opens its details in a result tab (outbox counts per type, the retrying rows of a type or the function's event source mappings; queue attributes and the subscriptions of a topic given as an ARN are billed and need `DB_ALLOW_BILLED=1`). Resolution only calls `sts:GetCallerIdentity` and `lambda:ListEventSourceMappings`. When flows have a `retry` column the dashboard adds an `아웃박스 한도 소진` card (rows that hit their retry limit).

```sh
db flows                                  # every flow, prod connections skipped
db flows -f shop-hub-stg --format json    # one row per box: flow, stage, name, metrics, warn (stage outbox-type per event type)
db flows --prod
```

Prod connections are not contacted without `--prod` (`--prod 로 확인`). The exit code is 1 if any entry is broken or any check failed.

### Boards

A board is a grid of query panels you define in `.dbtui/boards/<name>.json` (one file per board, meant to be committed):

```json
{
  "title": "shop 운영",
  "refresh": "60s",
  "panels": [
    { "title": "오늘 결제", "conn": "shop-prod-read", "query": "select count(*) from t_payments where created_at >= current_date", "view": "number", "warn": ">0" },
    { "title": "에러 로그", "conn": "loki-prod", "query": "sum(count_over_time({app=\"shop-api\"} |= \"ERROR\" [5m]))", "since": "6h", "view": "chart" },
    { "title": "DLQ", "flow": "shop-hub-stg", "view": "flow" },
    { "title": "최근 실패 아웃박스", "conn": "shop-stg", "query": "select id, event_type, last_fail_reason from t_outbox where retry_count > 0 order by id desc limit 20", "view": "table" }
  ]
}
```

`view` is `number` (first cell of the first row, highlighted when the optional `warn` rule such as `">0"` or `">=100"` matches), `table` (as many rows as fit), `chart` (the result chart) or `flow` (a flow from `.dbtui/flows.json`, one line per box plus up to 10 outbox event types that have rows). `refresh` defaults to `60s` and cannot go below `10s`; `since` sets the Loki / Prometheus / Grafana range like `--since`. Panels only read: PostgreSQL panels must be a single read-only statement and run in a READ ONLY transaction, Redis panels must be one bounded read command (single values and counts such as `GET`, `MGET`, `HGET`, `LLEN`, `INFO`, `LRANGE`/`ZRANGE` with an explicit range of at most 50, `SCAN` with `COUNT` up to 50; `KEYS`, `HGETALL`, `SMEMBERS` and `LRANGE k 0 -1` are refused) and are cut to 50 rows, and sources go through the same read-only checks as the editor. Expensive source queries (full scans, ranges over 7 days) are never forced; the panel shows `비싼 질의라 보드에서 돌리지 않음: <reason>` instead. `aws` connections cannot be query panels, use a `flow` panel. DynamoDB and Athena panels show `요금이 붙는 조회라 막음 (DB_ALLOW_BILLED=1 로 허용)` and send nothing unless billed calls are allowed. Each panel has a 15 second limit. A broken file or panel shows `설정 오류: <reason>` in place and `db` keeps running.

`Alt+B` (⌘⇧B, palette `보드`) picks a board and opens it in its own tab. Panels are laid out in 1 column below 80 columns of board width, 2 below 140 and 3 above. Each panel header shows its title, connection (with a red `prod` badge for prod connections), when it last ran and how long it took. Panels on the same connection run one after another and different connections run in parallel (4 at a time). The board refreshes every `refresh` while its tab is active; `r` re-reads the file and refreshes now, arrows or `hjkl` move between panels, and `Enter` opens the panel's query in a new query tab and runs it there (a flow panel opens the flow tab on that flow). `Ctrl+B` stays "go to definition", so boards use `Alt+B`.

```sh
db board                          # list boards
db board shop-ops                 # run every panel once, prod panels skipped
db board shop-ops --prod --format json
```

Each row is `panel`, `conn`, `view`, `value` and `error`. In JSON, number panels carry `value` (and `warn`), table / chart / flow panels carry `columns` and `rows` (capped at 50, with `truncated`). Prod panels are not run without `--prod` (`--prod 로 확인`). The exit code is 1 if any panel is broken or failed, 2 for an unknown board.

## CLI

`db <command>` runs without the screen, prints the result and exits. `db help` lists commands, `db <command> -h` shows flags.

```sh
db conns --format json
db tables -c app-stg -s public
db describe -c app-stg orders
db query -c app-stg "select id from orders limit 3" --format json
db query -c app-stg -f queries/app/report.sql --param id=7
db explain -c app-stg "select * from orders where id = 1"
db redis -c app-stg-redis GET some:key
db schema-diff -c app-stg --to app-prod --prod -s public
db linked -c app-stg --column user_id --value 1315 --format json
db status --format json
db flows --format json
db board shop-ops --format json
db history --conn app-stg --since 24h
db history show 20261116-011530-a3f9c1 --format json
db conn add app-stg2 --url 'postgres://app_ro:pw@db.stg.example.internal:5432/app'
```

Output formats are `table`, `tsv`, `csv` and `json` (table on a terminal, tsv otherwise). Reads run inside a server-side READ ONLY transaction. Exit codes: 0 success, 1 execution error, 2 usage, 3 refused by a safety check. CLI runs are not written to history.

## MCP

`db mcp` starts a read-only MCP server over stdio with the same project settings. Prod connections are only exposed with `--allow-prod`.

Tools: `list_connections`, `list_schemas`, `list_tables`, `describe_table`, `query`, `redis_scan`, `redis_get`, `redis_command`, `aws_tree`, `aws_open`. `query` also runs read-only source queries (Athena, DynamoDB, OpenSearch, Loki, Prometheus, Grafana); expensive ones such as an Athena `SELECT` without `LIMIT` or `WHERE` (billed per byte scanned) or a DynamoDB full scan are refused unless the call passes `"force": true`. Athena and DynamoDB queries are billed, so they are refused (nothing sent) unless the server was started with `DB_ALLOW_BILLED=1`. `aws` connections have no query language: `aws_tree {connection, path}` lists a node's children and `aws_open {connection, path}` returns its detail table; the billed `SNS 토픽`, `SQS 큐` and `S3` folders are hidden and refused by path without `DB_ALLOW_BILLED=1`. Results are capped by `--max-rows`.

```sh
claude mcp add db -e DB_ALLOW_BILLED=1 -- db mcp   # allow billed AWS calls (Athena, DynamoDB reads, SNS, SQS, S3)
```

```sh
claude mcp add db -- db mcp
claude mcp add db -- db mcp -p ~/work/db-project --max-rows 100
```

## Safety model

- A connection is **prod** when its name contains `prod`/`prd`, or when any `.`/`-` separated segment of its host is `prod`/`prd` (so `redis01.prod.example.internal` counts even if the name does not say so).
- Prod connections start write-locked every session. `Ctrl+L` unlocks one connection for `DB_PROD_UNLOCK_MIN` minutes; even then every write asks `y/N`.
- Statements that are dangerous anywhere (DELETE/UPDATE without WHERE, TRUNCATE, DROP, Redis FLUSHDB/FLUSHALL) always ask first, with an EXPLAIN row estimate where possible.
- The CLI refuses prod reads without `--prod`, refuses writes on prod entirely, and needs `--write` (plus `--force` for dangerous statements) elsewhere. MCP is read-only.
- Passwords, AES keys and IVs never appear in the status line, history, logs or error messages. Parameter values whose names look secret (`pass`, `secret`, `token`, `key`, `pwd`) are not remembered.

## Environment variables

| Variable | Meaning |
|---|---|
| `DBTUI_PROJECT` | project folder (default: current directory) |
| `DB_PROD_UNLOCK_MIN` | prod unlock duration in minutes (default 10, max 1440) |
| `DB_NOTIFY_SECS` | send a terminal notification (OSC 9) when a run takes longer than this (default 10, `0` off) |
| `DB_AUTO_SAVE=0` | do not auto-save query tabs to `queries/` |
| `DB_AUTO_OPEN=0` | do not open the last file when picking a connection |
| `DB_RESTORE=0` | do not save or restore tabs |
| `DB_KITTY_KEYS=0`/`1` | force the kitty keyboard protocol off/on |
| `DB_KEYS=cmd`/`ctrl` | show key hints as ⌘ or Ctrl |
| `DB_TIPS=0` | do not show the one-time tips in the status line |
| `DB_RECENT=0` | do not read or write `recent.json` (frequent palette actions, recent connections, per-table sort/WHERE/widths, input recall, tree expansion) |
| `DB_CLIPBOARD=osc52`/`system`/`both` | where copies go: OSC 52 only, the system clipboard only (`pbcopy`, `clip` through `cmd.exe` with the UTF-8 code page on WSL and Windows, `wl-copy`/`xclip`), or both (default). Copies over 10 MB are refused; export with `e`/`E` |
| `DB_ALLOW_BILLED=1` | allow AWS calls billed per request or per byte scanned (SQS, SNS, S3, Athena, DynamoDB data reads). Off by default; only `1` turns it on |
| `DB_HISTORY_RESULTS=0` | do not keep result snapshots with history (SQL, rows and time are still logged) |
| `DB_HISTORY_PROD=1` | also keep result snapshots of prod connections (off by default; only `1` turns it on) |
| `DB_UPDATE_CHECK=0` | do not check GitHub for a new release at startup |
| `GITHUB_TOKEN` | sent with `db update` and the startup check to GitHub API requests (higher rate limit) |
| `XDG_STATE_HOME` | where history, session, result snapshots (`db/results/`), seen tips, recent items, parameter values, undo log and the last update check live (default `~/.local/state/db/`) |

## Development

```sh
go vet ./... && go test ./...
```

Integration tests run when these point at disposable servers:

```sh
docker run -d --name dbtui-it-pg -e POSTGRES_PASSWORD=itpass -p 127.0.0.1:55432:5432 postgres:16-alpine
docker run -d --name dbtui-it-redis -p 127.0.0.1:56379:6379 redis:7-alpine
DBTUI_TEST_PG_URL=postgres://postgres:itpass@127.0.0.1:55432/postgres DBTUI_TEST_REDIS_ADDR=127.0.0.1:56379 go test ./...
```

`DBTUI_TEST_PROJECT=<project>` with `DBTUI_TEST_CONNS=<name>,<name>` additionally connects read-only to those real connections.

Pushing a `v*` tag runs `.github/workflows/release.yml`: tests against Postgres and Redis service containers, then builds the four archives and creates the GitHub Release.

## License

To be decided. Until a license file is added, no license is granted.
